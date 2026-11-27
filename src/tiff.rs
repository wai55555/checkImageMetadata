use std::borrow::Cow;
use std::collections::HashSet;

use crate::record::{RecordView, Source};
use crate::xmp;

const MAX_EXIF_BYTES: usize = 16 * 1024 * 1024;
const USER_COMMENT: u16 = 0x9286;
const IMAGE_DESCRIPTION: u16 = 0x010e;
const MAKE: u16 = 0x010f;
const MODEL: u16 = 0x0110;
const SOFTWARE: u16 = 0x0131;
const ARTIST: u16 = 0x013b;
const XP_COMMENT: u16 = 0x9c9c;
const XML_PACKET: u16 = 0x02bc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ByteOrder {
    Little,
    Big,
}

impl ByteOrder {
    fn u16(self, bytes: [u8; 2]) -> u16 {
        match self {
            Self::Little => u16::from_le_bytes(bytes),
            Self::Big => u16::from_be_bytes(bytes),
        }
    }

    fn u32(self, bytes: [u8; 4]) -> u32 {
        match self {
            Self::Little => u32::from_le_bytes(bytes),
            Self::Big => u32::from_be_bytes(bytes),
        }
    }
}

struct TiffReader<'a> {
    bytes: &'a [u8],
    byte_order: ByteOrder,
    ifd0_offset: u32,
}

#[derive(Clone, Copy)]
struct IfdEntry {
    tag: u16,
    field_type: u16,
    count: u32,
    value_field_offset: usize,
}

struct IfdView {
    entries_start: usize,
    entry_count: usize,
    next_ifd: u32,
}

impl<'a> TiffReader<'a> {
    fn new(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() > MAX_EXIF_BYTES {
            return None;
        }
        let byte_order = match bytes.get(..2)? {
            b"II" => ByteOrder::Little,
            b"MM" => ByteOrder::Big,
            _ => return None,
        };
        if byte_order.u16(bytes.get(2..4)?.try_into().ok()?) != 42 {
            return None;
        }
        let ifd0_offset = byte_order.u32(bytes.get(4..8)?.try_into().ok()?);
        if usize::try_from(ifd0_offset).ok()? >= bytes.len() {
            return None;
        }
        Some(Self {
            bytes,
            byte_order,
            ifd0_offset,
        })
    }

    fn ifd0(&self) -> Option<IfdView> {
        self.ifd(self.ifd0_offset)
    }

    fn ifd(&self, offset: u32) -> Option<IfdView> {
        let start = usize::try_from(offset).ok()?;
        let count_end = start.checked_add(2)?;
        let count = usize::from(
            self.byte_order
                .u16(self.bytes.get(start..count_end)?.try_into().ok()?),
        );
        let entries_start = count_end;
        let entries_bytes = count.checked_mul(12)?;
        let next_offset = entries_start.checked_add(entries_bytes)?;
        let next_end = next_offset.checked_add(4)?;
        let next_ifd = self
            .byte_order
            .u32(self.bytes.get(next_offset..next_end)?.try_into().ok()?);
        Some(IfdView {
            entries_start,
            entry_count: count,
            next_ifd,
        })
    }

    fn entry(&self, entries_start: usize, index: usize) -> Option<IfdEntry> {
        let offset = index.checked_mul(12)?.checked_add(entries_start)?;
        let end = offset.checked_add(12)?;
        let bytes = self.bytes.get(offset..end)?;
        Some(IfdEntry {
            tag: self.byte_order.u16(bytes.get(..2)?.try_into().ok()?),
            field_type: self.byte_order.u16(bytes.get(2..4)?.try_into().ok()?),
            count: self.byte_order.u32(bytes.get(4..8)?.try_into().ok()?),
            value_field_offset: offset.checked_add(8)?,
        })
    }

    fn value(&self, entry: IfdEntry) -> Option<&'a [u8]> {
        let type_size = match entry.field_type {
            1 | 2 | 6 | 7 => 1usize,
            3 | 8 => 2,
            4 | 9 | 11 | 13 => 4,
            5 | 10 | 12 => 8,
            _ => return None,
        };
        let value_len = type_size.checked_mul(usize::try_from(entry.count).ok()?)?;
        if value_len <= 4 {
            let value_field_end = entry.value_field_offset.checked_add(value_len)?;
            return self.bytes.get(entry.value_field_offset..value_field_end);
        }
        let value_field_end = entry.value_field_offset.checked_add(4)?;
        let value_offset = self.byte_order.u32(
            self.bytes
                .get(entry.value_field_offset..value_field_end)?
                .try_into()
                .ok()?,
        );
        let start = usize::try_from(value_offset).ok()?;
        let end = start.checked_add(value_len)?;
        self.bytes.get(start..end)
    }
}

/// 次IFD を辿る上限（REQ-EXT-05(a)）。循環・巨大な連鎖に対する保険。
const MAX_IFD_CHAIN: usize = 4;

pub fn parse<'a>(bytes: &'a [u8], source: Source) -> Vec<RecordView<'a>> {
    let Some(reader) = TiffReader::new(bytes) else {
        return Vec::new();
    };
    if reader.ifd0().is_none() {
        return Vec::new();
    }
    let mut records = Vec::new();
    let mut visited = HashSet::new();
    let mut offset = reader.ifd0_offset;
    // REQ-EXT-05(a): IFD1 以降も次IFD を辿る（上限付き）。IFD1 以降の ExifIFD も
    // IFD0 と同じ条件で辿る（サムネイル側のメタデータを取りこぼさない）。
    for _ in 0..MAX_IFD_CHAIN {
        if offset == 0 {
            break;
        }
        let Some(next_ifd) =
            parse_ifd(&reader, offset, source, true, 0, &mut visited, &mut records)
        else {
            break;
        };
        offset = next_ifd;
    }
    records
}

fn parse_ifd<'a>(
    reader: &TiffReader<'a>,
    offset: u32,
    source: Source,
    follow_exif: bool,
    depth: usize,
    visited: &mut HashSet<u32>,
    records: &mut Vec<RecordView<'a>>,
) -> Option<u32> {
    if !visited.insert(offset) {
        return Some(0);
    }
    let ifd = reader.ifd(offset)?;
    let mut exif_offsets = Vec::new();
    for index in 0..ifd.entry_count {
        // REQ-EXT-02: 1 エントリが読めない場合に IFD 全体を放棄せず、
        // そのエントリだけを飛ばして残りの解析を続ける。
        let Some(entry) = reader.entry(ifd.entries_start, index) else {
            continue;
        };
        if let Some(data) = reader.value(entry) {
            if entry.tag == XML_PACKET {
                records.extend(xmp::parse(data, Source::TiffXmp));
            } else if let Some(record) = decode_tag(entry.tag, data, source) {
                records.push(record);
            }
        }
        if follow_exif && entry.tag == 0x8769 && entry.field_type == 4 && entry.count == 1 {
            if let Some(value) = reader.value(entry) {
                if let Ok(bytes) = <[u8; 4]>::try_from(value) {
                    exif_offsets.push(reader.byte_order.u32(bytes));
                }
            }
        }
    }
    if follow_exif && depth < 4 {
        for exif_offset in exif_offsets {
            if exif_offset != 0 {
                let _ = parse_ifd(
                    reader,
                    exif_offset,
                    source,
                    true,
                    depth + 1,
                    visited,
                    records,
                );
            }
        }
    }
    Some(ifd.next_ifd)
}

fn decode_tag<'a>(tag: u16, data: &'a [u8], source: Source) -> Option<RecordView<'a>> {
    let (label, keyword, body) = match tag {
        USER_COMMENT => (
            "EXIF UserComment",
            "UserComment",
            decode_user_comment(data)?,
        ),
        IMAGE_DESCRIPTION => (
            "EXIF ImageDescription",
            "ImageDescription",
            decode_ascii_field(data)?,
        ),
        SOFTWARE => ("EXIF Software", "Software", decode_ascii_field(data)?),
        MAKE => ("EXIF Make", "Make", decode_ascii_field(data)?),
        MODEL => ("EXIF Model", "Model", decode_ascii_field(data)?),
        ARTIST => ("EXIF Artist", "Artist", decode_ascii_field(data)?),
        XP_COMMENT => ("EXIF XPComment", "XPComment", decode_xp_comment(data)?),
        _ => return None,
    };
    Some(RecordView {
        source,
        label: Cow::Borrowed(label),
        body,
        keyword: Some(Cow::Borrowed(keyword)),
        detail: None,
    })
}

fn decode_ascii_field(data: &[u8]) -> Option<Cow<'_, str>> {
    // REQ-EXT-10: UTF-8 でないことを理由にレコードごと落とさない。
    // ASCII 系タグは Latin-1 として読める（`Steps: ` や `NovelAI` 等の照合対象を残す）。
    let text = decode_utf8_or_latin1(data)?;
    let trimmed = text.trim_end_matches('\0');
    if trimmed.is_empty() {
        return None;
    }
    // 借用のまま返せる場合はコピーしない（既存の高速経路を保つ）。
    match text {
        Cow::Borrowed(_) if trimmed.len() == text.len() => Some(text),
        Cow::Borrowed(borrowed) => Some(Cow::Owned(borrowed.trim_end_matches('\0').to_string())),
        Cow::Owned(owned) => Some(Cow::Owned(owned.trim_end_matches('\0').to_string())),
    }
}

fn decode_xp_comment(data: &[u8]) -> Option<Cow<'_, str>> {
    if data.len() % 2 != 0 {
        return None;
    }
    let mut units: Vec<u16> = data
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    while units.last() == Some(&0) {
        units.pop();
    }
    let text = String::from_utf16(&units).ok()?;
    if text.is_empty() {
        None
    } else {
        Some(Cow::Owned(text))
    }
}

fn decode_user_comment(data: &[u8]) -> Option<Cow<'_, str>> {
    let (body, encoding) = if let Some(code) = data.get(..8) {
        let body = data.get(8..)?;
        if code == b"ASCII\0\0\0" || code == b"JIS\0\0\0\0\0" {
            (body, UserCommentEncoding::Text)
        } else if code == b"UNICODE\0" {
            (body, UserCommentEncoding::Unicode)
        } else if code == [0; 8] {
            (body, UserCommentEncoding::Undefined)
        } else {
            (data, UserCommentEncoding::Text)
        }
    } else {
        (data, UserCommentEncoding::Text)
    };

    let decoded = match encoding {
        UserCommentEncoding::Text => decode_utf8_or_latin1(body)?,
        UserCommentEncoding::Unicode => Cow::Owned(decode_unicode(body)),
        UserCommentEncoding::Undefined => decode_undefined_code(body)?,
    };
    let decoded = match decoded {
        Cow::Borrowed(text) => Cow::Borrowed(text.trim_end_matches('\0')),
        Cow::Owned(text) => Cow::Owned(trim_trailing_nuls(text)),
    };
    if decoded.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&decoded) {
        if let Ok(pretty) = serde_json::to_string_pretty(&value) {
            return Some(Cow::Owned(pretty));
        }
    }
    Some(decoded)
}

#[derive(Clone, Copy)]
enum UserCommentEncoding {
    Text,
    Unicode,
    Undefined,
}

#[derive(Clone, Copy)]
enum Utf16Order {
    Little,
    Big,
}

fn decode_unicode(bytes: &[u8]) -> String {
    if let Some((order, body)) = utf16_bom(bytes) {
        return decode_utf16_order(body, order);
    }
    // REQ-EXT-09: EXIF 規格どおり BE を既定とする。LE を採るのは
    // 「BE で読むと明確に破綻し、LE で読むと明確に成立する」場合に限る
    // （旧実装は utf16_score の比較だけで LE を選び、健全な BE 本文を壊していた）。
    let big_endian = decode_utf16_order(bytes, Utf16Order::Big);
    let little_endian = decode_utf16_order(bytes, Utf16Order::Little);
    if utf16_le_is_clearly_better(&big_endian, &little_endian) {
        little_endian
    } else {
        big_endian
    }
}

fn utf16_le_is_clearly_better(big_endian: &str, little_endian: &str) -> bool {
    // LE 側が壊れているなら候補にしない。
    if utf16_is_clearly_broken(little_endian) {
        return false;
    }
    // BE 側が制御文字・置換文字を含むなら明確な破綻。LE が健全なら LE を採る。
    if utf16_is_clearly_broken(big_endian) {
        return true;
    }
    // 明確な破綻ではないが、BE 側に読める ASCII が 1 文字も無く、LE 側にはある
    // 場合はエンディアン取り違えの典型（LE 格納の ASCII 本文）とみなす。
    // BE 格納の非 ASCII 本文（日本語のみ等）は LE 側にも ASCII が出ないため
    // ここでは切り替わらず、BE のまま読まれる。
    let readable_ascii = |text: &str| {
        text.chars()
            .filter(|character| character.is_ascii_graphic() || *character == ' ')
            .count()
    };
    readable_ascii(big_endian) == 0 && readable_ascii(little_endian) > 0
}

fn utf16_is_clearly_broken(text: &str) -> bool {
    // NUL 以外の制御文字、または置換文字（デコード失敗）を含むなら明確な破綻。
    text.chars().any(|character| {
        matches!(character as u32, 0x0001..=0x0008 | 0x000b..=0x001f) || character == '\u{fffd}'
    })
}

fn decode_undefined_code(bytes: &[u8]) -> Option<Cow<'_, str>> {
    if let Some((order, body)) = utf16_bom(bytes) {
        return Some(Cow::Owned(decode_utf16_order(body, order)));
    }
    let (even_zeroes, odd_zeroes) =
        bytes
            .iter()
            .enumerate()
            .fold((0usize, 0usize), |(even, odd), (index, byte)| {
                if *byte == 0 {
                    if index % 2 == 0 {
                        (even + 1, odd)
                    } else {
                        (even, odd + 1)
                    }
                } else {
                    (even, odd)
                }
            });
    if even_zeroes >= 2 && even_zeroes > odd_zeroes.saturating_mul(2) {
        Some(Cow::Owned(decode_utf16_order(bytes, Utf16Order::Big)))
    } else if odd_zeroes >= 2 && odd_zeroes > even_zeroes.saturating_mul(2) {
        Some(Cow::Owned(decode_utf16_order(bytes, Utf16Order::Little)))
    } else {
        decode_utf8_or_latin1(bytes)
    }
}

fn utf16_bom(bytes: &[u8]) -> Option<(Utf16Order, &[u8])> {
    match bytes.get(..2)? {
        [0xff, 0xfe] => Some((Utf16Order::Little, bytes.get(2..)?)),
        [0xfe, 0xff] => Some((Utf16Order::Big, bytes.get(2..)?)),
        _ => None,
    }
}

fn decode_utf16_order(bytes: &[u8], order: Utf16Order) -> String {
    let mut units = Vec::with_capacity(bytes.len().div_ceil(2));
    for pair in bytes.chunks_exact(2) {
        let pair = [pair[0], pair[1]];
        units.push(match order {
            Utf16Order::Little => u16::from_le_bytes(pair),
            Utf16Order::Big => u16::from_be_bytes(pair),
        });
    }
    let mut text = String::from_utf16_lossy(&units);
    if bytes.len() % 2 != 0 {
        text.push('\u{fffd}');
    }
    text
}

fn decode_utf8_or_latin1(bytes: &[u8]) -> Option<Cow<'_, str>> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Some(Cow::Borrowed(text));
    }
    let mut text = String::new();
    text.try_reserve_exact(bytes.len()).ok()?;
    for byte in bytes {
        text.push(char::from(*byte));
    }
    Some(Cow::Owned(text))
}

fn trim_trailing_nuls(mut text: String) -> String {
    while text.ends_with('\0') {
        text.pop();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{
        parse, ByteOrder, TiffReader, Utf16Order, ARTIST, IMAGE_DESCRIPTION, MAKE, MAX_IFD_CHAIN,
        MODEL, SOFTWARE, USER_COMMENT, XP_COMMENT,
    };
    use crate::record::Source;

    fn entry(tag: u16, field_type: u16, count: u32, value: [u8; 4]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&field_type.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&value);
        bytes
    }

    fn tiff_with_comment(comment: &[u8]) -> Vec<u8> {
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&1u16.to_le_bytes());
        let mut value = [0u8; 4];
        if comment.len() <= value.len() {
            value[..comment.len()].copy_from_slice(comment);
        } else {
            value = 26u32.to_le_bytes();
        }
        tiff.extend_from_slice(&entry(0x9286, 7, comment.len() as u32, value));
        tiff.extend_from_slice(&0u32.to_le_bytes());
        if comment.len() > 4 {
            tiff.extend_from_slice(comment);
        }
        tiff
    }

    fn tiff_with_tags(tags: &[(u16, u16, &[u8])]) -> Vec<u8> {
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&(tags.len() as u16).to_le_bytes());
        let mut data_offset = 8 + 2 + tags.len() * 12 + 4;
        let mut external_values = Vec::new();
        for (tag, field_type, value_bytes) in tags {
            let mut value = [0u8; 4];
            if value_bytes.len() <= 4 {
                value[..value_bytes.len()].copy_from_slice(value_bytes);
            } else {
                value = (data_offset as u32).to_le_bytes();
                external_values.push(*value_bytes);
                data_offset += value_bytes.len();
            }
            tiff.extend_from_slice(&tag.to_le_bytes());
            tiff.extend_from_slice(&field_type.to_le_bytes());
            tiff.extend_from_slice(&(value_bytes.len() as u32).to_le_bytes());
            tiff.extend_from_slice(&value);
        }
        tiff.extend_from_slice(&0u32.to_le_bytes());
        for value in external_values {
            tiff.extend_from_slice(value);
        }
        tiff
    }

    #[test]
    fn phase2_tiff_header_accepts_little_and_big_endian() {
        let little = b"II*\0\x08\0\0\0\0\0\0\0";
        let big = b"MM\0*\0\0\0\x08\0\0\0\0";
        assert_eq!(
            TiffReader::new(little).unwrap().byte_order,
            ByteOrder::Little
        );
        assert_eq!(TiffReader::new(big).unwrap().byte_order, ByteOrder::Big);
        assert!(TiffReader::new(b"ZZ\0*\0\0\0\x08").is_none());
        assert!(TiffReader::new(b"II\0\0\0\0\0\x08").is_none());
    }

    #[test]
    fn tiff_xml_packet_tag_uses_the_common_xmp_parser_and_source() {
        let packet = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/'><d:title>TIFF XMP</d:title></rdf:Description></rdf:RDF></x:xmpmeta>";
        let tiff = tiff_with_tags(&[(0x02bc, 7, packet)]);
        let records = parse(&tiff, Source::PngExif);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].source, Source::TiffXmp);
        assert_eq!(records[0].label, "XMP Title");
        assert_eq!(records[0].body, "TIFF XMP");
    }

    #[test]
    fn fixture_16_reads_inline_user_comment() {
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&entry(0x9286, 7, 4, *b"test"));
        tiff.extend_from_slice(&0u32.to_le_bytes());

        let records = parse(&tiff, Source::JpegExif);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].body, "test");
    }

    #[test]
    fn tiff_follows_exif_ifd_before_ifd1_and_does_not_follow_nested_next_ifd() {
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&entry(0x8769, 4, 1, 26u32.to_le_bytes()));
        tiff.extend_from_slice(&58u32.to_le_bytes());

        // ExifIFD at 26 has an out-of-order UserComment and a nonzero next IFD.
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&entry(0x9286, 7, 14, 44u32.to_le_bytes()));
        tiff.extend_from_slice(&76u32.to_le_bytes());
        tiff.extend_from_slice(b"ASCII\0\0\0nested");

        // IFD1 contains another visible UserComment and must be visited after ExifIFD.
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&entry(0x9286, 7, 4, *b"IFD1"));
        tiff.extend_from_slice(&0u32.to_le_bytes());
        // The ExifIFD next pointer points here, but that IFD must not be followed.
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&entry(0x9286, 7, 4, *b"skip"));
        tiff.extend_from_slice(&0u32.to_le_bytes());

        let records = parse(&tiff, Source::JpegExif);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].body, "nested");
        assert_eq!(records[1].body, "IFD1");
    }

    #[test]
    fn tiff_visited_offsets_prevent_exif_cycles() {
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&entry(0x9286, 7, 4, *b"once"));
        tiff.extend_from_slice(&entry(0x8769, 4, 1, 8u32.to_le_bytes()));
        tiff.extend_from_slice(&0u32.to_le_bytes());

        let records = parse(&tiff, Source::JpegExif);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].body, "once");
    }

    /// T-10（REQ-EXT-02）: 壊れたエントリ（値の範囲外）の後ろにある UserComment が読める。
    #[test]
    fn t10_broken_entry_does_not_discard_the_rest_of_the_ifd() {
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&3u16.to_le_bytes());
        // 1) ImageDescription の値オフセットをファイル外へ向ける（読めないエントリ）。
        tiff.extend_from_slice(&entry(IMAGE_DESCRIPTION, 2, 64, 0xFFFF_0000u32.to_le_bytes()));
        // 2) その後ろの UserComment は読めること。
        tiff.extend_from_slice(&entry(USER_COMMENT, 7, 4, *b"late"));
        // 3) さらに後ろの Software も読めること。
        tiff.extend_from_slice(&entry(SOFTWARE, 2, 4, *b"Soft"));
        tiff.extend_from_slice(&0u32.to_le_bytes());

        let records = parse(&tiff, Source::JpegExif);
        let bodies: Vec<&str> = records.iter().map(|record| record.body.as_ref()).collect();
        assert!(bodies.contains(&"late"), "{bodies:?}");
        assert!(bodies.contains(&"Soft"), "{bodies:?}");
    }

    struct IfdPlan {
        entries: Vec<(u16, u16, Vec<u8>)>,
        exif_comment: Option<Vec<u8>>,
    }

    /// 複数 IFD（IFD0 → IFD1 → …）と各 IFD の ExifIFD を持つ最小 TIFF を組み立てる。
    fn build_multi_ifd(plans: &[IfdPlan]) -> Vec<u8> {
        let mut ifd_offsets = Vec::with_capacity(plans.len());
        let mut cursor = 8usize;
        for plan in plans {
            ifd_offsets.push(cursor);
            let count = plan.entries.len() + usize::from(plan.exif_comment.is_some());
            cursor += 2 + 12 * count + 4;
        }
        let mut exif_offsets = Vec::with_capacity(plans.len());
        for plan in plans {
            exif_offsets.push(if plan.exif_comment.is_some() {
                let offset = cursor;
                cursor += 2 + 12 + 4;
                Some(offset)
            } else {
                None
            });
        }
        let mut value_area = cursor;
        let mut values: Vec<(usize, Vec<u8>)> = Vec::new();

        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        for (index, plan) in plans.iter().enumerate() {
            let count = plan.entries.len() + usize::from(plan.exif_comment.is_some());
            tiff.extend_from_slice(&(count as u16).to_le_bytes());
            for (tag, field_type, value) in &plan.entries {
                tiff.extend_from_slice(&tag.to_le_bytes());
                tiff.extend_from_slice(&field_type.to_le_bytes());
                tiff.extend_from_slice(&(value.len() as u32).to_le_bytes());
                if value.len() <= 4 {
                    let mut inline = [0u8; 4];
                    inline[..value.len()].copy_from_slice(value);
                    tiff.extend_from_slice(&inline);
                } else {
                    tiff.extend_from_slice(&(value_area as u32).to_le_bytes());
                    values.push((value_area, value.clone()));
                    value_area += value.len();
                }
            }
            if plan.exif_comment.is_some() {
                tiff.extend_from_slice(&0x8769u16.to_le_bytes());
                tiff.extend_from_slice(&4u16.to_le_bytes());
                tiff.extend_from_slice(&1u32.to_le_bytes());
                tiff.extend_from_slice(&(exif_offsets[index].unwrap() as u32).to_le_bytes());
            }
            let next = ifd_offsets.get(index + 1).copied().unwrap_or(0);
            tiff.extend_from_slice(&(next as u32).to_le_bytes());
        }
        for (index, plan) in plans.iter().enumerate() {
            if let Some(comment) = &plan.exif_comment {
                assert_eq!(Some(tiff.len()), exif_offsets[index]);
                let mut value = b"ASCII\0\0\0".to_vec();
                value.extend_from_slice(comment);
                tiff.extend_from_slice(&1u16.to_le_bytes());
                tiff.extend_from_slice(&USER_COMMENT.to_le_bytes());
                tiff.extend_from_slice(&7u16.to_le_bytes());
                tiff.extend_from_slice(&(value.len() as u32).to_le_bytes());
                tiff.extend_from_slice(&(value_area as u32).to_le_bytes());
                values.push((value_area, value.clone()));
                value_area += value.len();
                tiff.extend_from_slice(&0u32.to_le_bytes());
            }
        }
        for (offset, value) in values {
            if tiff.len() < offset {
                tiff.resize(offset, 0);
            }
            tiff.extend_from_slice(&value);
        }
        tiff
    }

    /// T-13（REQ-EXT-05(a)）: IFD1 の ExifIFD 内 UserComment が読める。
    #[test]
    fn t13_reads_user_comment_from_ifd1_exif_ifd() {
        let tiff = build_multi_ifd(&[
            IfdPlan { entries: vec![], exif_comment: Some(b"ifd0".to_vec()) },
            IfdPlan {
                entries: vec![(SOFTWARE, 2, b"Thumb".to_vec())],
                exif_comment: Some(b"ifd1".to_vec()),
            },
        ]);
        let records = parse(&tiff, Source::JpegExif);
        let bodies: Vec<&str> = records.iter().map(|record| record.body.as_ref()).collect();
        assert!(bodies.contains(&"ifd0"), "{bodies:?}");
        assert!(bodies.contains(&"ifd1"), "{bodies:?}");
        assert!(bodies.contains(&"Thumb"), "{bodies:?}");
    }

    /// T-42（REQ-EXT-05(a)）: IFD2 以降のタグと IFD2 の ExifIFD 内 UserComment が読める。
    #[test]
    fn t42_reads_tags_and_exif_ifd_from_ifd2() {
        let tiff = build_multi_ifd(&[
            IfdPlan { entries: vec![], exif_comment: None },
            IfdPlan { entries: vec![], exif_comment: None },
            IfdPlan {
                entries: vec![(IMAGE_DESCRIPTION, 2, b"third".to_vec())],
                exif_comment: Some(b"ifd2".to_vec()),
            },
        ]);
        let records = parse(&tiff, Source::JpegExif);
        let bodies: Vec<&str> = records.iter().map(|record| record.body.as_ref()).collect();
        assert!(bodies.contains(&"third"), "{bodies:?}");
        assert!(bodies.contains(&"ifd2"), "{bodies:?}");
    }

    /// REQ-EXT-05(a): 次IFD の追跡は上限（4）で止まる。
    #[test]
    fn next_ifd_chain_stops_at_the_depth_cap() {
        let plans: Vec<IfdPlan> = (0..6)
            .map(|index| IfdPlan {
                entries: vec![(IMAGE_DESCRIPTION, 2, format!("ifd{index}").into_bytes())],
                exif_comment: None,
            })
            .collect();
        let tiff = build_multi_ifd(&plans);
        let records = parse(&tiff, Source::JpegExif);
        assert_eq!(records.len(), MAX_IFD_CHAIN);
        assert_eq!(records[0].body, "ifd0");
        assert_eq!(records[3].body, "ifd3");
    }

    /// T-26（REQ-EXT-10）: 非 UTF-8（Latin-1）の ImageDescription でも ASCII 部分が読める。
    #[test]
    fn t26_latin1_image_description_is_kept_for_ascii_matching() {
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        let value = b"caf\xe9 Steps: 20\0";
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&entry(IMAGE_DESCRIPTION, 2, value.len() as u32, 26u32.to_le_bytes()));
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(value);
        let records = parse(&tiff, Source::JpegExif);
        assert_eq!(records.len(), 1);
        assert!(records[0].body.starts_with("caf\u{e9}"), "{:?}", records[0].body);
        assert!(records[0].body.contains("Steps: 20"));
    }

    #[test]
    fn fixture_17_decodes_undefined_charset_utf16le_without_dropping_prefix() {
        let expected = "parameters=1\nSteps: 4";
        let mut comment = vec![0; 8];
        for unit in expected.encode_utf16() {
            comment.extend_from_slice(&unit.to_le_bytes());
        }
        let tiff = tiff_with_comment(&comment);
        let records = parse(&tiff, Source::JpegExif);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].body, expected);
    }

    #[test]
    fn user_comment_keeps_short_and_unknown_charset_bytes() {
        let short_bytes = tiff_with_comment(b"abc");
        let short = parse(&short_bytes, Source::JpegExif);
        assert_eq!(short[0].body, "abc");

        let unknown_bytes = tiff_with_comment(b"12345678");
        let unknown = parse(&unknown_bytes, Source::JpegExif);
        assert_eq!(unknown[0].body, "12345678");

        let latin1 = tiff_with_comment(b"ASCII\0\0\0\xe9");
        let decoded = parse(&latin1, Source::JpegExif);
        assert_eq!(decoded[0].body, "é");
    }

    #[test]
    fn undefined_charset_does_not_treat_one_ascii_nul_as_utf16() {
        let mut comment = vec![0; 8];
        comment.extend_from_slice(b"ab\0");
        let tiff = tiff_with_comment(&comment);
        let records = parse(&tiff, Source::JpegExif);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].body, "ab");
    }

    fn unicode_comment(text: &str, order: Utf16Order, bom: bool) -> Vec<u8> {
        let mut comment = b"UNICODE\0".to_vec();
        if bom {
            comment.extend_from_slice(match order {
                Utf16Order::Little => &[0xff, 0xfe],
                Utf16Order::Big => &[0xfe, 0xff],
            });
        }
        for unit in text.encode_utf16() {
            let bytes = match order {
                Utf16Order::Little => unit.to_le_bytes(),
                Utf16Order::Big => unit.to_be_bytes(),
            };
            comment.extend_from_slice(&bytes);
        }
        comment
    }

    #[test]
    fn unicode_user_comment_honors_boms_and_defaults_to_be_without_one() {
        let expected = "猫parameters=1";
        // BOM 付きは BOM がエンディアンを決める（両順序）。
        for order in [Utf16Order::Little, Utf16Order::Big] {
            let tiff = tiff_with_comment(&unicode_comment(expected, order, true));
            let records = parse(&tiff, Source::JpegExif);
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].body, expected);
        }
        // REQ-EXT-09: BOM なしは BE が既定。BE 格納の本文はそのまま読める。
        let tiff = tiff_with_comment(&unicode_comment(expected, Utf16Order::Big, false));
        let records = parse(&tiff, Source::JpegExif);
        assert_eq!(records[0].body, expected);
    }

    /// T-19（REQ-EXT-09）: BOM なし UNICODE の既定は BE。LE は BE が明確に破綻した
    /// ときだけ採る。ASCII 中心・非ASCII中心のどちらも壊れないことを固定する。
    #[test]
    fn t19_bomless_unicode_defaults_to_be_and_accepts_le_only_when_be_is_broken() {
        for expected in ["parameters=ascii", "猫のパラメータ=1"] {
            let tiff = tiff_with_comment(&unicode_comment(expected, Utf16Order::Big, false));
            let records = parse(&tiff, Source::JpegExif);
            assert_eq!(records[0].body, expected, "BE-stored text must decode as BE");
        }
        // LE 格納 [0x00, 0x01] は BE で読むと U+0001（制御文字）になり明確に破綻する。
        // この場合だけ LE を採用する。
        let mut comment = b"UNICODE\0".to_vec();
        comment.extend_from_slice(&[0x00, 0x01]);
        let tiff = tiff_with_comment(&comment);
        let records = parse(&tiff, Source::JpegExif);
        assert_eq!(records[0].body, "Ā");
    }

    #[test]
    fn unicode_user_comment_preserves_internal_nuls_and_has_no_character_cap() {
        let expected = "first\0second";
        let tiff = tiff_with_comment(&unicode_comment(expected, Utf16Order::Little, true));
        let records = parse(&tiff, Source::JpegExif);
        assert_eq!(records[0].body, expected);

        let long = "x".repeat(10_001);
        let tiff = tiff_with_comment(&unicode_comment(&long, Utf16Order::Big, false));
        let records = parse(&tiff, Source::JpegExif);
        assert_eq!(records[0].body, long);
    }

    #[test]
    fn fixture_15_reads_image_description_and_other_display_tags_in_ifd_order() {
        let mut xp_comment = Vec::new();
        for unit in "Windows comment".encode_utf16().chain([0]) {
            xp_comment.extend_from_slice(&unit.to_le_bytes());
        }
        let tiff = tiff_with_tags(&[
            (IMAGE_DESCRIPTION, 2, b"parameters=from-description\0"),
            (MAKE, 2, b"Example Camera\0"),
            (MODEL, 2, b"Model X\0"),
            (SOFTWARE, 2, b"Generator\0"),
            (ARTIST, 2, b"Artist\0"),
            (XP_COMMENT, 1, &xp_comment),
        ]);

        let records = parse(&tiff, Source::JpegExif);
        assert_eq!(
            records
                .iter()
                .map(|record| (record.label.as_ref(), record.body.as_ref()))
                .collect::<Vec<_>>(),
            [
                ("EXIF ImageDescription", "parameters=from-description"),
                ("EXIF Make", "Example Camera"),
                ("EXIF Model", "Model X"),
                ("EXIF Software", "Generator"),
                ("EXIF Artist", "Artist"),
                ("EXIF XPComment", "Windows comment"),
            ]
        );
    }
}
