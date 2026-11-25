use crate::record::{RecordView, Source};
use crate::tiff;

const EXIF_IDENTIFIER: &[u8; 6] = b"Exif\0\0";
const SOI: &[u8; 2] = b"\xff\xd8";

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
                result
                    .records
                    .extend(tiff::parse(tiff_data, Source::JpegExif));
            }
        }

        pos = segment_end;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::scan;

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
            let _ = scan(input);
        }
    }
}
