use std::borrow::Cow;

use crate::record::{RecordView, Source};
use crate::tiff;
use crate::xmp;

const EXIF_IDENTIFIER: &[u8] = b"Exif\0\0";
const XMP_MAIN_IDENTIFIER: &str = "http://ns.adobe.com/xap/1.0/\0";
const XMP_EXTENSION_IDENTIFIER: &str = "http://ns.adobe.com/xmp/extension/\0";
const MAX_XMP_BYTES: usize = 16 * 1024 * 1024;
const SOI: &[u8; 2] = b"\xff\xd8";

struct LocatedRecord<'a> {
    offset: usize,
    record_index: usize,
    record: RecordView<'a>,
}

struct MainXmp<'a> {
    offset: usize,
    packet: &'a [u8],
}

struct ExtendedFragment<'a> {
    offset: usize,
    guid: &'a [u8],
    total_length: usize,
    fragment_offset: usize,
    body: &'a [u8],
}

#[derive(Default)]
pub struct JpegScan<'a> {
    pub records: Vec<RecordView<'a>>,
    pub saw_exif: bool,
}

pub fn scan<'a>(bytes: &'a [u8]) -> JpegScan<'a> {
    if bytes.get(..2) != Some(SOI.as_slice()) {
        return JpegScan::default();
    }

    let mut result = JpegScan::default();
    let mut records = Vec::new();
    let mut main_xmp = Vec::new();
    let mut extended_fragments = Vec::new();
    let mut pos = 2usize;
    loop {
        let Some(marker_end) = pos.checked_add(2) else {
            break;
        };
        if bytes.get(pos) != Some(&0xff) || marker_end > bytes.len() {
            break;
        }
        loop {
            let Some(next) = pos.checked_add(1) else {
                break;
            };
            if bytes.get(next) != Some(&0xff) {
                break;
            }
            pos = next;
        }
        let Some(marker_pos) = pos.checked_add(1) else {
            break;
        };
        let Some(&marker) = bytes.get(marker_pos) else {
            break;
        };

        if marker == 0xda || marker == 0xd9 {
            break;
        }
        if (0xd0..=0xd7).contains(&marker) || marker == 0x01 {
            let Some(next) = pos.checked_add(2) else {
                break;
            };
            pos = next;
            continue;
        }

        let Some(length_start) = pos.checked_add(2) else {
            break;
        };
        let Some(length_end) = length_start.checked_add(2) else {
            break;
        };
        let Some(length_bytes) = bytes.get(length_start..length_end) else {
            break;
        };
        let segment_length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if segment_length < 2 {
            break;
        }
        let Some(segment_end) = pos
            .checked_add(2)
            .and_then(|p| p.checked_add(segment_length))
        else {
            break;
        };
        if segment_end > bytes.len() {
            break;
        }
        let Some(payload_start) = pos.checked_add(4) else {
            break;
        };
        let Some(payload) = bytes.get(payload_start..segment_end) else {
            break;
        };

        if marker == 0xe1 && payload.starts_with(EXIF_IDENTIFIER) {
            result.saw_exif = true;
            if let Some(tiff_data) = payload.get(EXIF_IDENTIFIER.len()..) {
                records.extend(
                    tiff::parse(tiff_data, Source::JpegExif)
                        .into_iter()
                        .enumerate()
                        .map(|(record_index, record)| LocatedRecord {
                            offset: pos,
                            record_index,
                            record,
                        }),
                );
            }
        } else if marker == 0xe1 && payload.starts_with(XMP_MAIN_IDENTIFIER.as_bytes()) {
            if let Some(packet) = payload.get(XMP_MAIN_IDENTIFIER.len()..) {
                main_xmp.push(MainXmp {
                    offset: pos,
                    packet,
                });
            }
        } else if marker == 0xe1 && payload.starts_with(XMP_EXTENSION_IDENTIFIER.as_bytes()) {
            if let Some(fragment) = parse_extended_fragment(payload, pos) {
                extended_fragments.push(fragment);
            }
        } else if marker == 0xfe {
            if let Some(record) = jpeg_comment_record(payload) {
                records.push(LocatedRecord {
                    offset: pos,
                    record_index: 0,
                    record,
                });
            }
        } else if marker == 0xed {
            records.extend(parse_iptc(payload).into_iter().enumerate().map(
                |(record_index, record)| LocatedRecord {
                    offset: pos,
                    record_index,
                    record,
                },
            ));
        }

        pos = segment_end;
    }

    let main_xmp_present = !main_xmp.is_empty();
    let mut main_records = Vec::new();
    let mut extended_guids = Vec::new();
    for packet in main_xmp {
        let parsed = xmp::parse_packet(packet.packet, Source::JpegXmp);
        if let Some(guid) = parsed.has_extended_xmp {
            if valid_guid(guid.as_bytes()) {
                extended_guids.push(guid);
            }
        }
        main_records.extend(parsed.records.into_iter().enumerate().map(
            |(record_index, record)| LocatedRecord {
                offset: packet.offset,
                record_index,
                record,
            },
        ));
    }
    records.extend(main_records);

    if let Some((packet, offset)) =
        reassemble_extended_xmp(&extended_fragments, main_xmp_present, &extended_guids)
    {
        records.extend(
            xmp::parse_owned(packet, Source::JpegXmp)
                .into_iter()
                .enumerate()
                .map(|(record_index, record)| LocatedRecord {
                    offset,
                    record_index,
                    record,
                }),
        );
    }
    records.sort_by_key(|record| (record.offset, record.record_index));
    result.records = records.into_iter().map(|located| located.record).collect();
    result
}

fn parse_extended_fragment<'a>(payload: &'a [u8], offset: usize) -> Option<ExtendedFragment<'a>> {
    // Derive the identifier length from the specification literal itself.
    let header = payload.strip_prefix(XMP_EXTENSION_IDENTIFIER.as_bytes())?;
    let guid = header.get(..32)?;
    if !valid_guid(guid) {
        return None;
    }
    let total_end = 32usize.checked_add(4)?;
    let fragment_offset_end = total_end.checked_add(4)?;
    let total_bytes: [u8; 4] = header.get(32..total_end)?.try_into().ok()?;
    let fragment_offset_bytes: [u8; 4] = header
        .get(total_end..fragment_offset_end)?
        .try_into()
        .ok()?;
    let total_length = usize::try_from(u32::from_be_bytes(total_bytes)).ok()?;
    if total_length > MAX_XMP_BYTES {
        return None;
    }
    Some(ExtendedFragment {
        offset,
        guid,
        total_length,
        fragment_offset: usize::try_from(u32::from_be_bytes(fragment_offset_bytes)).ok()?,
        body: header.get(fragment_offset_end..)?,
    })
}

fn valid_guid(guid: &[u8]) -> bool {
    guid.len() == 32 && guid.iter().all(u8::is_ascii_hexdigit)
}

fn reassemble_extended_xmp(
    fragments: &[ExtendedFragment<'_>],
    main_has_xmp: bool,
    main_guids: &[String],
) -> Option<(Vec<u8>, usize)> {
    // REQ-EXT-04(b): GUID と total_length ごとに組を分ける。異なる組の断片が
    // 混在しているだけで全体を捨てず、完全な組があればそれを採用する。
    let mut groups: Vec<(&[u8], usize, Vec<&ExtendedFragment<'_>>)> = Vec::new();
    for fragment in fragments {
        match groups.iter_mut().find(|(guid, total_length, _)| {
            guid.eq_ignore_ascii_case(fragment.guid) && *total_length == fragment.total_length
        }) {
            Some((_, _, group)) => group.push(fragment),
            None => groups.push((
                fragment.guid,
                fragment.total_length,
                vec![fragment],
            )),
        }
    }

    // 主パケットが参照する GUID を最優先し、次に最初に現れた組を優先する。
    let matches_main = |guid: &[u8]| {
        main_guids
            .iter()
            .any(|candidate| candidate.as_bytes().eq_ignore_ascii_case(guid))
    };
    let mut ordered = groups;
    ordered.sort_by_key(|(guid, _, group)| {
        let first_seen = group
            .iter()
            .map(|fragment| fragment.offset)
            .min()
            .unwrap_or(0);
        (!matches_main(guid), first_seen)
    });

    for (guid, total_length, group) in ordered {
        // 主パケットが拡張 GUID を明示している場合、それ以外の組は採用しない
        // （本体と無関係な断片を混ぜない）。
        if main_has_xmp && !main_guids.is_empty() && !matches_main(guid) {
            continue;
        }
        if let Some(packet) = assemble_extended_group(&group, total_length) {
            return Some(packet);
        }
    }
    None
}

/// 1 つの GUID 組が連続・完全に揃っているかを検証して連結する。
fn assemble_extended_group(
    group: &[&ExtendedFragment<'_>],
    total_length: usize,
) -> Option<(Vec<u8>, usize)> {
    let first_offset = group.iter().map(|fragment| fragment.offset).min()?;
    let mut ordered: Vec<_> = group.to_vec();
    ordered.sort_by_key(|fragment| fragment.fragment_offset);
    let mut expected_offset = 0usize;
    let mut previous_offset = None;
    for fragment in &ordered {
        let end = fragment.fragment_offset.checked_add(fragment.body.len())?;
        if end > total_length
            || previous_offset == Some(fragment.fragment_offset)
            || fragment.fragment_offset != expected_offset
        {
            return None;
        }
        previous_offset = Some(fragment.fragment_offset);
        expected_offset = end;
    }
    if expected_offset != total_length {
        return None;
    }

    // Allocation occurs only after the advertised size and complete coverage
    // have been validated.
    let mut packet = Vec::with_capacity(total_length);
    for fragment in ordered {
        packet.extend_from_slice(fragment.body);
    }
    Some((packet, first_offset))
}

fn jpeg_comment_record(payload: &[u8]) -> Option<RecordView<'_>> {
    let body = match std::str::from_utf8(payload) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => Cow::Owned(payload.iter().map(|&byte| char::from(byte)).collect()),
    };
    if body.is_empty() {
        return None;
    }
    // REQ-API-01: A1111 形式かどうかを `detail` の構造化された印で渡す。
    // 表示用の label は従来どおり付けるが、sorter は label を使わない（REQ-CLS-04）。
    let has_negative_prompt = body.contains("Negative prompt:");
    let has_steps = body.contains("Steps:");
    let (label, detail) = match (has_negative_prompt, has_steps) {
        (true, true) => (
            "Stable Diffusion (A1111)",
            Some(crate::record::JPEG_COM_A1111_BOTH_MARKERS),
        ),
        (true, false) | (false, true) => (
            "Stable Diffusion (A1111)",
            Some(crate::record::JPEG_COM_A1111_ONE_MARKER),
        ),
        (false, false) => ("JPEG Comment", None),
    };
    Some(RecordView {
        source: Source::JpegComment,
        label: Cow::Borrowed(label),
        body,
        keyword: None,
        detail: detail.map(Cow::Borrowed),
    })
}

fn parse_iptc(payload: &[u8]) -> Vec<RecordView<'_>> {
    const IRB_IDENTIFIER: &[u8] = b"Photoshop 3.0\0";
    if !payload.starts_with(IRB_IDENTIFIER) {
        return Vec::new();
    }
    let mut cursor = IRB_IDENTIFIER.len();
    let mut records = Vec::new();
    while cursor < payload.len() {
        let Some(header_end) = cursor.checked_add(6) else {
            break;
        };
        let Some(header) = payload.get(cursor..header_end) else {
            break;
        };
        if header.get(..4) != Some(b"8BIM") {
            break;
        }
        let resource_id = u16::from_be_bytes([header[4], header[5]]);
        let Some(name_length) = payload.get(header_end).copied().map(usize::from) else {
            break;
        };
        let Some(name_field_len) = 1usize
            .checked_add(name_length)
            .and_then(|length| length.checked_add(length % 2))
        else {
            break;
        };
        let Some(name_end) = header_end.checked_add(name_field_len) else {
            break;
        };
        let Some(size_end) = name_end.checked_add(4) else {
            break;
        };
        let Some(size_bytes) = payload.get(name_end..size_end) else {
            break;
        };
        let Ok(size_bytes) = <[u8; 4]>::try_from(size_bytes) else {
            break;
        };
        let data_start = size_end;
        let Some(data_end) = data_start.checked_add(u32::from_be_bytes(size_bytes) as usize) else {
            break;
        };
        let Some(padded_end) = data_end.checked_add((data_end - data_start) % 2) else {
            break;
        };
        let Some(resource_data) = payload.get(data_start..data_end) else {
            break;
        };
        if payload.get(data_end..padded_end).is_none() {
            break;
        }
        if resource_id == 0x0404 {
            records.extend(parse_iim(resource_data));
        }
        cursor = padded_end;
    }
    records
}

fn parse_iim(bytes: &[u8]) -> Vec<RecordView<'_>> {
    let mut cursor = 0usize;
    let mut captions = Vec::new();
    let mut declared_utf8 = false;
    while cursor < bytes.len() {
        let Some(header_end) = cursor.checked_add(5) else {
            break;
        };
        let Some(header) = bytes.get(cursor..header_end) else {
            break;
        };
        if header[0] != 0x1c {
            break;
        }
        let record_number = header[1];
        let dataset_number = header[2];
        let encoded_length = u16::from_be_bytes([header[3], header[4]]);
        cursor = header_end;
        let data_length = if encoded_length & 0x8000 == 0 {
            usize::from(encoded_length)
        } else {
            let length_bytes_count = usize::from(encoded_length & 0x7fff);
            if length_bytes_count == 0 {
                break;
            }
            let Some(length_end) = cursor.checked_add(length_bytes_count) else {
                break;
            };
            let Some(length_bytes) = bytes.get(cursor..length_end) else {
                break;
            };
            cursor = length_end;
            let mut length = 0usize;
            let mut valid = true;
            for &byte in length_bytes {
                if let Some(next) = length
                    .checked_mul(256)
                    .and_then(|value| value.checked_add(usize::from(byte)))
                {
                    length = next;
                } else {
                    valid = false;
                    break;
                }
            }
            if !valid {
                break;
            }
            length
        };
        let Some(data_end) = cursor.checked_add(data_length) else {
            break;
        };
        let Some(data) = bytes.get(cursor..data_end) else {
            break;
        };
        cursor = data_end;
        match (record_number, dataset_number) {
            (1, 90) => declared_utf8 = data == b"\x1b%G",
            (2, 120) => captions.push(data),
            _ => {}
        }
    }

    captions
        .into_iter()
        .filter_map(|caption| {
            let body = if declared_utf8 {
                Cow::Borrowed(std::str::from_utf8(caption).ok()?)
            } else {
                match std::str::from_utf8(caption) {
                    Ok(text) => Cow::Borrowed(text),
                    Err(_) => Cow::Owned(caption.iter().map(|&byte| char::from(byte)).collect()),
                }
            };
            (!body.is_empty()).then_some(RecordView {
                source: Source::JpegIptc,
                label: Cow::Borrowed("IPTC Caption"),
                body,
                keyword: Some(Cow::Borrowed("Caption/Abstract")),
                detail: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::scan;
    use crate::output::TextSink;
    use crate::record::Source;

    const MAIN_XMP_IDENTIFIER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
    const EXT_XMP_IDENTIFIER: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";

    fn xmp_packet(description: &str) -> Vec<u8> {
        format!(
            "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'>{description}</rdf:RDF></x:xmpmeta>"
        )
        .into_bytes()
    }

    fn be_tiff_user_comment(comment: &[u8]) -> Vec<u8> {
        let mut value = b"ASCII\0\0\0".to_vec();
        value.extend_from_slice(comment);
        let mut tiff = b"MM\0*\0\0\0\x08".to_vec();
        tiff.extend_from_slice(&1u16.to_be_bytes());
        tiff.extend_from_slice(&0x9286u16.to_be_bytes());
        tiff.extend_from_slice(&7u16.to_be_bytes());
        tiff.extend_from_slice(&(value.len() as u32).to_be_bytes());
        tiff.extend_from_slice(&26u32.to_be_bytes());
        tiff.extend_from_slice(&0u32.to_be_bytes());
        tiff.extend_from_slice(&value);
        tiff
    }

    fn le_tiff_image_description(description: &[u8]) -> Vec<u8> {
        let mut value = description.to_vec();
        value.push(0);
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x010eu16.to_le_bytes());
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&(value.len() as u32).to_le_bytes());
        tiff.extend_from_slice(&26u32.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(&value);
        tiff
    }

    fn segment(marker: u8, data: &[u8]) -> Vec<u8> {
        let mut output = vec![0xff, marker];
        output.extend_from_slice(&((data.len() + 2) as u16).to_be_bytes());
        output.extend_from_slice(data);
        output
    }

    fn main_xmp_segment(packet: &[u8]) -> Vec<u8> {
        let mut payload = MAIN_XMP_IDENTIFIER.to_vec();
        payload.extend_from_slice(packet);
        segment(0xe1, &payload)
    }

    fn extended_xmp_segment(guid: &[u8], total: usize, offset: usize, body: &[u8]) -> Vec<u8> {
        let mut payload = EXT_XMP_IDENTIFIER.to_vec();
        payload.extend_from_slice(guid);
        payload.extend_from_slice(&(total as u32).to_be_bytes());
        payload.extend_from_slice(&(offset as u32).to_be_bytes());
        payload.extend_from_slice(body);
        segment(0xe1, &payload)
    }

    fn iim_dataset(record: u8, dataset: u8, body: &[u8]) -> Vec<u8> {
        let mut data = vec![0x1c, record, dataset];
        if body.len() <= 0x7fff {
            data.extend_from_slice(&(body.len() as u16).to_be_bytes());
        } else {
            panic!("test IIM dataset too long for short length");
        }
        data.extend_from_slice(body);
        data
    }

    fn iptc_irb(iim: &[u8]) -> Vec<u8> {
        let mut payload = b"Photoshop 3.0\0".to_vec();
        payload.extend_from_slice(b"8BIM");
        payload.extend_from_slice(&0x0404u16.to_be_bytes());
        payload.extend_from_slice(&[0, 0]); // empty Pascal name and even padding
        payload.extend_from_slice(&(iim.len() as u32).to_be_bytes());
        payload.extend_from_slice(iim);
        if iim.len() % 2 != 0 {
            payload.push(0);
        }
        payload
    }

    #[test]
    fn fixture_14_reads_big_endian_exif_from_app1() {
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&be_tiff_user_comment(b"parameters=BE"));
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&segment(0xe1, &exif));
        jpeg.extend_from_slice(b"\xff\xda");

        let scan = scan(&jpeg);
        assert!(scan.saw_exif);
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.records[0].body, "parameters=BE");
    }

    #[test]
    fn fixture_20_reads_exif_after_more_than_64_kib_of_segments() {
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&be_tiff_user_comment(b"parameters=late"));
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&segment(0xe2, &vec![0x11; 65_000]));
        jpeg.extend_from_slice(&segment(0xe2, &vec![0x22; 2_048]));
        jpeg.extend_from_slice(&segment(0xe1, &exif));
        jpeg.extend_from_slice(b"\xff\xda");

        let scan = scan(&jpeg);
        assert!(scan.saw_exif);
        assert_eq!(scan.records[0].body, "parameters=late");
    }

    #[test]
    fn fixture_15_reads_image_description_from_jpeg_exif() {
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&le_tiff_image_description(b"parameters=from-jpeg"));
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&segment(0xe1, &exif));
        jpeg.extend_from_slice(b"\xff\xda");

        let scan = scan(&jpeg);
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.records[0].label, "EXIF ImageDescription");
        assert_eq!(scan.records[0].body, "parameters=from-jpeg");
    }

    #[test]
    fn jpeg_marker_fill_bytes_are_skipped_before_segment_lengths() {
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&be_tiff_user_comment(b"parameters=fill"));
        let mut jpeg = b"\xff\xd8\xff".to_vec();
        jpeg.extend_from_slice(&segment(0xe1, &exif)[1..]);
        jpeg.extend_from_slice(b"\xff\xda");

        let scan = scan(&jpeg);
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.records[0].body, "parameters=fill");
    }

    #[test]
    fn jpeg_stops_at_sos_before_payload_bytes_that_look_like_app1() {
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&be_tiff_user_comment(b"must-not-read"));
        let mut jpeg = b"\xff\xd8\xff\xda".to_vec();
        jpeg.extend_from_slice(&segment(0xe1, &exif));

        let scan = scan(&jpeg);
        assert!(!scan.saw_exif);
        assert!(scan.records.is_empty());
    }

    #[test]
    fn malformed_jpeg_segments_stop_without_panicking() {
        let cases: [&[u8]; 4] = [
            b"\xff\xd8\xff\xe1\x00",
            b"\xff\xd8\xff\xe1\x00\x01",
            b"\xff\xd8\xff\xe1\xff\xff",
            b"\xff\xd8\xff\xe1\x00\x08Exif",
        ];
        for input in cases {
            let scan = scan(input);
            assert!(!scan.saw_exif);
            assert!(scan.records.is_empty());
        }
    }

    #[test]
    fn fixture_18_midjourney_description_keeps_the_xmp_description_label() {
        let packet = xmp_packet(
            "<rdf:Description xmlns:dc='http://purl.org/dc/elements/1.1/'><dc:description>a cat --ar 1:1</dc:description></rdf:Description>",
        );
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&main_xmp_segment(&packet));
        jpeg.extend_from_slice(b"\xff\xda");

        let parsed = scan(&jpeg);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].label, "XMP Description");
        let mut sink = TextSink::default();
        for record in parsed.records {
            sink.push_record(record);
        }
        let mut output = Vec::new();
        sink.flush(&mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "--- XMP Description ---\na cat --ar 1:1\n"
        );
    }

    #[test]
    fn fixture_19_reads_com_as_a1111_or_jpeg_comment() {
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&segment(
            0xfe,
            b"prompt\nNegative prompt: blurry\nSteps: 20",
        ));
        jpeg.extend_from_slice(&segment(0xfe, b"plain comment"));
        jpeg.extend_from_slice(b"\xff\xda");

        let parsed = scan(&jpeg);
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.records[0].source, Source::JpegComment);
        assert_eq!(parsed.records[0].label, "Stable Diffusion (A1111)");
        assert_eq!(
            parsed.records[0].detail.as_deref(),
            Some(crate::record::JPEG_COM_A1111_BOTH_MARKERS)
        );
        assert_eq!(parsed.records[1].label, "JPEG Comment");
        assert_eq!(parsed.records[1].detail, None);
        assert_eq!(parsed.records[1].body, "plain comment");
    }

    /// REQ-API-01: 片方のマーカーだけの COM は「片方のみ」の印になる。
    #[test]
    fn jpeg_com_with_one_marker_sets_the_partial_mark() {
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&segment(0xfe, b"Steps: 20 photo comment"));
        jpeg.extend_from_slice(b"\xff\xda");
        let parsed = scan(&jpeg);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(
            parsed.records[0].detail.as_deref(),
            Some(crate::record::JPEG_COM_A1111_ONE_MARKER)
        );
    }

    /// REQ-API-01: label は表示専用で、印は本文マーカーだけで決まる。
    #[test]
    fn jpeg_com_mark_does_not_depend_on_the_label() {
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&segment(0xfe, b"Negative prompt: blur\nSteps: 20"));
        jpeg.extend_from_slice(b"\xff\xda");
        let parsed = scan(&jpeg);
        assert_eq!(
            parsed.records[0].detail.as_deref(),
            Some(crate::record::JPEG_COM_A1111_BOTH_MARKERS)
        );
    }

    #[test]
    fn fixture_33_reassembles_extended_xmp_fragments_out_of_order() {
        let guid = b"0123456789abcdef0123456789ABCDEF";
        let extension = xmp_packet(
            "<rdf:Description xmlns:i='http://iptc.org/std/Iptc4xmpExt/2008-02-29/'><i:AIPromptInformation>extended prompt</i:AIPromptInformation></rdf:Description>",
        );
        let split = extension.len() / 2;
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&main_xmp_segment(&xmp_packet(&format!(
            "<rdf:Description xmlns:n='http://ns.adobe.com/xap/1.0/note/' n:HasExtendedXMP='{}'/>",
            String::from_utf8_lossy(guid)
        ))));
        jpeg.extend_from_slice(&extended_xmp_segment(
            guid,
            extension.len(),
            split,
            &extension[split..],
        ));
        jpeg.extend_from_slice(&extended_xmp_segment(
            guid,
            extension.len(),
            0,
            &extension[..split],
        ));
        jpeg.extend_from_slice(b"\xff\xda");

        let parsed = scan(&jpeg);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].source, Source::JpegXmp);
        assert_eq!(parsed.records[0].label, "XMP AIPromptInformation");
        assert_eq!(parsed.records[0].body, "extended prompt");
    }

    #[test]
    fn fixture_33_allows_a_valid_standalone_extended_xmp_packet() {
        let guid = b"fedcba9876543210fedcba9876543210";
        let extension = xmp_packet(
            "<rdf:Description xmlns:dc='http://purl.org/dc/elements/1.1/'><dc:description>standalone extension</dc:description></rdf:Description>",
        );
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&extended_xmp_segment(guid, extension.len(), 0, &extension));
        jpeg.extend_from_slice(b"\xff\xda");

        let parsed = scan(&jpeg);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].body, "standalone extension");
    }

    #[test]
    fn mismatched_extended_xmp_guid_keeps_only_the_main_packet() {
        let guid = b"0123456789abcdef0123456789abcdef";
        let main = xmp_packet(&format!(
            "<rdf:Description xmlns:n='http://ns.adobe.com/xap/1.0/note/' n:HasExtendedXMP='{}' xmlns:dc='http://purl.org/dc/elements/1.1/'><dc:description>main packet</dc:description></rdf:Description>",
            String::from_utf8_lossy(guid)
        ));
        let extension = xmp_packet(
            "<rdf:Description xmlns:dc='http://purl.org/dc/elements/1.1/'><dc:title>must be ignored</dc:title></rdf:Description>",
        );
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&main_xmp_segment(&main));
        jpeg.extend_from_slice(&extended_xmp_segment(
            b"fedcba9876543210fedcba9876543210",
            extension.len(),
            0,
            &extension,
        ));
        jpeg.extend_from_slice(b"\xff\xda");

        let parsed = scan(&jpeg);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].body, "main packet");
    }

    /// T-12（REQ-EXT-04(b)）: GUID／total_length の異なる断片が混在しても、
    /// 完全な組があればそれを採用する。
    #[test]
    fn t12_mixed_extended_xmp_fragments_still_use_the_complete_group() {
        let guid = b"0123456789abcdef0123456789abcdef";
        let main = xmp_packet(&format!(
            "<rdf:Description xmlns:n='http://ns.adobe.com/xap/1.0/note/' n:HasExtendedXMP='{}'/>",
            String::from_utf8_lossy(guid)
        ));
        let complete = xmp_packet(
            "<rdf:Description xmlns:dc='http://purl.org/dc/elements/1.1/'><dc:description>complete group</dc:description></rdf:Description>",
        );
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&main_xmp_segment(&main));
        // 別 GUID・別 total_length の断片を先に置く（混在）。
        jpeg.extend_from_slice(&extended_xmp_segment(
            b"fedcba9876543210fedcba9876543210",
            999,
            0,
            b"unrelated fragment",
        ));
        // 同じ GUID でも total_length の違う断片（不完全な組）。
        jpeg.extend_from_slice(&extended_xmp_segment(guid, complete.len() + 10, 0, b"broken"));
        // 完全な組。
        jpeg.extend_from_slice(&extended_xmp_segment(guid, complete.len(), 0, &complete));
        jpeg.extend_from_slice(b"\xff\xda");

        let parsed = scan(&jpeg);
        let bodies: Vec<&str> = parsed.records.iter().map(|record| record.body.as_ref()).collect();
        assert!(bodies.contains(&"complete group"), "{bodies:?}");
    }

    /// T-39（REQ-EXT-05(b)）: セグメント長不正で歩行を止めても、
    /// それまでに得たレコードは捨てずに返す。
    #[test]
    fn t39_jpeg_returns_records_before_an_invalid_segment_length() {
        let xmp = xmp_packet(
            "<rdf:Description xmlns:dc='http://purl.org/dc/elements/1.1/'><dc:description>before damage</dc:description></rdf:Description>",
        );
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&main_xmp_segment(&xmp));
        // 長さ 0 の不正セグメント。
        jpeg.extend_from_slice(&[0xff, 0xfe, 0x00, 0x00]);
        jpeg.extend_from_slice(b"\xff\xda");

        let parsed = scan(&jpeg);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].body, "before damage");
    }

    #[test]
    fn fixture_32_reads_exif_and_xmp_from_distinct_app1_segments() {
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&be_tiff_user_comment(b"parameters=EXIF"));
        let xmp = xmp_packet(
            "<rdf:Description xmlns:dc='http://purl.org/dc/elements/1.1/'><dc:title>XMP title</dc:title></rdf:Description>",
        );
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&segment(0xe1, &exif));
        jpeg.extend_from_slice(&main_xmp_segment(&xmp));
        jpeg.extend_from_slice(b"\xff\xda");

        let parsed = scan(&jpeg);
        assert!(parsed.saw_exif);
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.records[0].source, Source::JpegExif);
        assert_eq!(parsed.records[0].body, "parameters=EXIF");
        assert_eq!(parsed.records[1].source, Source::JpegXmp);
        assert_eq!(parsed.records[1].body, "XMP title");
    }

    #[test]
    fn fixture_34_reads_utf8_iptc_caption_from_app13_irb() {
        let mut iim = iim_dataset(1, 90, b"\x1b%G");
        iim.extend_from_slice(&iim_dataset(2, 120, "キャプション".as_bytes()));
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&segment(0xed, &iptc_irb(&iim)));
        jpeg.extend_from_slice(b"\xff\xda");

        let parsed = scan(&jpeg);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].source, Source::JpegIptc);
        assert_eq!(parsed.records[0].label, "IPTC Caption");
        assert_eq!(
            parsed.records[0].keyword.as_deref(),
            Some("Caption/Abstract")
        );
        assert_eq!(parsed.records[0].body, "キャプション");
    }

    #[test]
    fn iptc_without_utf8_declaration_uses_latin1_and_keeps_prior_captions_on_damage() {
        let mut iim = iim_dataset(2, 120, b"caf\xe9");
        iim.extend_from_slice(b"\x1c\x02"); // broken next dataset
        let mut payload = iptc_irb(&iim);
        payload.extend_from_slice(b"8BIM\x04"); // broken next IRB
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&segment(0xed, &payload));
        jpeg.extend_from_slice(b"\xff\xda");

        let parsed = scan(&jpeg);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].body, "café");
    }

    #[test]
    fn iptc_extended_iim_length_is_supported() {
        let iim = vec![0x1c, 2, 120, 0x80, 0x01, 0x02, b'o', b'k'];
        let mut jpeg = b"\xff\xd8".to_vec();
        jpeg.extend_from_slice(&segment(0xed, &iptc_irb(&iim)));
        jpeg.extend_from_slice(b"\xff\xda");

        let parsed = scan(&jpeg);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].body, "ok");
    }
}
