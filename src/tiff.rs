use std::borrow::Cow;

use crate::record::{RecordView, Source};

const MAX_EXIF_BYTES: usize = 16 * 1024 * 1024;
const USER_COMMENT: u16 = 0x9286;

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

    fn ifd0(&self) -> Option<(usize, usize, usize)> {
        self.ifd(self.ifd0_offset)
    }

    fn ifd(&self, offset: u32) -> Option<(usize, usize, usize)> {
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
        Some((entries_start, count, usize::try_from(next_ifd).ok()?))
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
        // D-07 adds support for values stored inline in the four-byte field.
        if value_len <= 4 {
            return None;
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
    let Some((entries_start, entry_count, _)) = reader.ifd0() else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for index in 0..entry_count {
        let Some(entry) = reader.entry(entries_start, index) else {
            break;
        };
        if entry.tag == USER_COMMENT {
            if let Some(data) = reader.value(entry) {
                if let Some(body) = decode_initial_user_comment(data) {
                    records.push(RecordView {
                        source,
                        label: Cow::Borrowed("EXIF UserComment"),
                        body,
                        keyword: Some(Cow::Borrowed("UserComment")),
                        detail: None,
                    });
                }
            }
        }
    }
    records
}

fn decode_initial_user_comment(data: &[u8]) -> Option<Cow<'_, str>> {
    let body = if data.len() >= 8 {
        let code = data.get(..8)?;
        if code == b"ASCII\0\0\0" || code == b"JIS\0\0\0\0\0" {
            data.get(8..)?
        } else {
            data
        }
    } else {
        data
    };
    let text = std::str::from_utf8(body).ok()?.trim_end_matches('\0');
    if text.is_empty() {
        return None;
    }
    Some(Cow::Borrowed(text))
}

#[cfg(test)]
mod tests {
    use super::{ByteOrder, TiffReader};

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
}
