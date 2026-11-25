use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::HashSet;

use crate::record::{RecordView, Source};

const MAX_EXIF_BYTES: usize = 16 * 1024 * 1024;
const USER_COMMENT: u16 = 0x9286;
const IMAGE_DESCRIPTION: u16 = 0x010e;
const MAKE: u16 = 0x010f;
const MODEL: u16 = 0x0110;
const SOFTWARE: u16 = 0x0131;
const ARTIST: u16 = 0x013b;
const XP_COMMENT: u16 = 0x9c9c;

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

pub fn parse<'a>(bytes: &'a [u8], source: Source) -> Vec<RecordView<'a>> {
    let Some(reader) = TiffReader::new(bytes) else {
        return Vec::new();
    };
    let Some(ifd0) = reader.ifd0() else {
        return Vec::new();
    };
    let mut records = Vec::new();
    let mut visited = HashSet::new();
    let next_ifd = parse_ifd(
        &reader,
        reader.ifd0_offset,
        source,
        true,
        0,
        &mut visited,
        &mut records,
    )
    .unwrap_or(ifd0.next_ifd);
    if next_ifd != 0 {
        let _ = parse_ifd(
            &reader,
            next_ifd,
            source,
            false,
            0,
            &mut visited,
            &mut records,
        );
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
        let entry = reader.entry(ifd.entries_start, index)?;
        if let Some(data) = reader.value(entry) {
            if let Some(record) = decode_tag(entry.tag, data, source) {
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
    let text = std::str::from_utf8(data).ok()?.trim_end_matches('\0');
    if text.is_empty() {
        None
    } else {
        Some(Cow::Borrowed(text))
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
    let big_endian = decode_utf16_order(bytes, Utf16Order::Big);
    let little_endian = decode_utf16_order(bytes, Utf16Order::Little);
    if utf16_score(&little_endian) < utf16_score(&big_endian) {
        little_endian
    } else {
        big_endian
    }
}

fn utf16_score(text: &str) -> (usize, usize, Reverse<usize>) {
    let mut controls = 0usize;
    let mut replacements = 0usize;
    let mut ascii = 0usize;
    for character in text.chars() {
        if matches!(character as u32, 0x0000..=0x0008 | 0x000e..=0x001f) {
            controls += 1;
        }
        if character == '\u{fffd}' {
            replacements += 1;
        }
        if character.is_ascii() {
            ascii += 1;
        }
    }
    (controls, replacements, Reverse(ascii))
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
        parse, ByteOrder, TiffReader, Utf16Order, ARTIST, IMAGE_DESCRIPTION, MAKE, MODEL, SOFTWARE,
        XP_COMMENT,
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
    fn unicode_user_comment_honors_boms_and_scores_bomless_endianness() {
        for order in [Utf16Order::Little, Utf16Order::Big] {
            let expected = "猫parameters=1";
            for bom in [true, false] {
                let tiff = tiff_with_comment(&unicode_comment(expected, order, bom));
                let records = parse(&tiff, Source::JpegExif);
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].body, expected);
            }
        }
    }

    #[test]
    fn unicode_user_comment_preserves_internal_nuls_and_has_no_character_cap() {
        let expected = "first\0second";
        let tiff = tiff_with_comment(&unicode_comment(expected, Utf16Order::Little, true));
        let records = parse(&tiff, Source::JpegExif);
        assert_eq!(records[0].body, expected);

        let long = "x".repeat(10_001);
        let tiff = tiff_with_comment(&unicode_comment(&long, Utf16Order::Little, false));
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
