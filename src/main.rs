pub mod jpeg;
pub mod keywords;
pub mod output;
pub mod png;
pub mod record;
pub mod tiff;
pub mod xmp;

use flate2::read::{GzDecoder, ZlibDecoder};
use memmap2::MmapOptions;
use std::fs::File;
use std::io::{self, Read, Write};

fn extract_png_metadata<W: Write>(
    mmap: &[u8],
    path: &str,
    strict: bool,
    stealth_policy: StealthPolicy,
    writer: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::output::TextSink;
    let result = png::scan(mmap, strict);
    writeln!(writer, "=== PNG File: {} ===", path)?;
    if result.stats.comfy_tail_notice {
        writeln!(
            writer,
            "(Large file or ComfyUI detected. Scanning tail for metadata...)"
        )?;
    }
    let has_normal_metadata = result.records.iter().any(|record| {
        matches!(
            record.source,
            record::Source::PngText
                | record::Source::PngExif
                | record::Source::PngXmp
                | record::Source::JpegExif
                | record::Source::JpegXmp
                | record::Source::JpegComment
                | record::Source::JpegIptc
                | record::Source::WebpExif
                | record::Source::WebpXmp
                | record::Source::TiffXmp
        )
    });
    let stealth_result = match stealth_policy {
        StealthPolicy::Never => None,
        StealthPolicy::Auto if has_normal_metadata => None,
        StealthPolicy::Auto | StealthPolicy::Always => check_stealth_png(mmap),
    };
    let mut sink = TextSink::default();
    for record in result.records {
        sink.push_record(record);
    }
    if let Some((mode, signature, body)) = stealth_result.as_ref() {
        sink.push_record(record::RecordView {
            source: record::Source::PngStealth,
            label: std::borrow::Cow::Owned(format!("Stealth PNG Info ({mode}) [{signature}]")),
            body: std::borrow::Cow::Owned(body.clone()),
            keyword: None,
            detail: Some(std::borrow::Cow::Owned(format!("{mode}/{signature}"))),
        });
    }
    let has_any_metadata_record = !sink.is_empty();
    sink.flush(writer)?;

    if !has_any_metadata_record {
        writeln!(writer, "(No metadata detected)")?;
    }

    Ok(())
}

const MAX_STEALTH_DIMENSION: usize = 16_384;
const MAX_STEALTH_DECODED_PNG_BYTES: usize = 512 * 1024 * 1024;
const MAX_STEALTH_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_STEALTH_IDAT_BYTES: usize = 512 * 1024 * 1024;
const STEALTH_HEADER_BITS: usize = ("stealth_pnginfo".len() * 8) + 32;
const STEALTH_SIGNATURE_BYTES: usize = "stealth_pnginfo".len();

struct StealthBitCursor<'a> {
    decoded: &'a [u8],
    height: usize,
    available_rows: usize,
    row_bytes: usize,
    row_stride: usize,
    bytes_per_pixel: usize,
    bytes_per_sample: usize,
    channel_count: usize,
    bit_capacity: usize,
    bits_read: usize,
    x: usize,
    y: usize,
    channel_idx: usize,
    row_start: usize,
    sample_offset: usize,
    pixel_sample_start: usize,
    sample_index: usize,
}

impl<'a> StealthBitCursor<'a> {
    fn new(
        decoded: &'a [u8],
        width: usize,
        height: usize,
        available_rows: usize,
        row_bytes: usize,
        bytes_per_pixel: usize,
        bytes_per_sample: usize,
        sample_channel: Option<usize>,
    ) -> Option<Self> {
        let row_stride = row_bytes.checked_add(1)?;
        let available_size = row_stride.checked_mul(available_rows)?;
        // The caller supplies only fully reconstructed rows. Full expansion verifies exact size
        // before constructing this cursor; the incremental path must pass its completed-row count.
        let decoded = decoded.get(..available_size)?;
        let sample_offset = usize::from(bytes_per_sample == 2);
        let channel_count = if sample_channel.is_some() { 1 } else { 3 };
        let pixel_count = if available_rows == height {
            width.checked_mul(height)?
        } else {
            available_rows
        };
        let bit_capacity = pixel_count.checked_mul(channel_count)?;
        let sample_index = sample_channel.unwrap_or(0).checked_mul(bytes_per_sample)?;
        if available_rows == 0
            || width == 0
            || sample_index.checked_add(sample_offset)? >= row_bytes
        {
            return None;
        }

        Some(Self {
            decoded,
            height,
            available_rows,
            row_bytes,
            row_stride,
            bytes_per_pixel,
            bytes_per_sample,
            channel_count,
            bit_capacity,
            bits_read: 0,
            x: 0,
            y: 0,
            channel_idx: 0,
            row_start: 0,
            sample_offset,
            pixel_sample_start: sample_index,
            sample_index,
        })
    }

    fn next_bit(&mut self) -> Option<u8> {
        if self.bits_read >= self.bit_capacity || self.y >= self.available_rows {
            return None;
        }
        // Row boundaries are validated at cursor creation and when advancing rows; keep the hot
        // path to one indexed LSB read per bit. In 16-bit samples sample_offset is byte two.
        let value = self.decoded[self.row_start + 1 + self.sample_index + self.sample_offset] & 1;
        self.bits_read += 1;

        if self.bits_read < self.bit_capacity {
            if self.channel_count == 1 {
                self.advance_pixel()?;
            } else {
                self.channel_idx += 1;
                self.sample_index += self.bytes_per_sample;
                if self.channel_idx == 3 {
                    self.channel_idx = 0;
                    self.sample_index = self.pixel_sample_start;
                    self.advance_pixel()?;
                }
            }
        }
        Some(value)
    }

    fn advance_pixel(&mut self) -> Option<()> {
        self.y += 1;
        if self.y == self.height {
            self.y = 0;
            self.x = self.x.checked_add(1)?;
            self.row_start = 0;
            self.pixel_sample_start = self.pixel_sample_start.checked_add(self.bytes_per_pixel)?;
            self.sample_index = self.pixel_sample_start;
        } else {
            self.row_start = self.row_start.checked_add(self.row_stride)?;
        }
        // Each reconstructed row has one filter byte followed by row_bytes sample bytes.
        if self.y < self.available_rows {
            self.decoded
                .get(self.row_start + 1..self.row_start + 1 + self.row_bytes)?;
        }
        Some(())
    }

    fn next_byte(&mut self) -> Option<u8> {
        let mut byte = 0u8;
        for bit_in_byte in 0..8 {
            byte |= self.next_bit()? << (7 - bit_in_byte);
        }
        Some(byte)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum StealthPolicy {
    #[default]
    Auto,
    Always,
    Never,
}

impl StealthPolicy {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

fn has_stealth_header(
    decoded: &[u8],
    width: usize,
    height: usize,
    available_rows: usize,
    row_bytes: usize,
    bytes_per_pixel: usize,
    bytes_per_sample: usize,
    color_type: u8,
) -> Option<bool> {
    let total_pixels = width.checked_mul(height)?;
    for mode in ["Alpha", "RGB"] {
        let sample_channel = match (mode, color_type) {
            ("Alpha", 4) => Some(1usize),
            ("Alpha", 6) => Some(3usize),
            ("RGB", 2 | 6) => None,
            _ => continue,
        };
        let Some(mut bit_cursor) = StealthBitCursor::new(
            decoded,
            width,
            height,
            available_rows,
            row_bytes,
            bytes_per_pixel,
            bytes_per_sample,
            sample_channel,
        ) else {
            continue;
        };
        if bit_cursor.bit_capacity < STEALTH_HEADER_BITS {
            continue;
        }

        let mut header = [0u8; STEALTH_SIGNATURE_BYTES + 4];
        let mut complete = true;
        for byte in &mut header {
            if let Some(value) = bit_cursor.next_byte() {
                *byte = value;
            } else {
                complete = false;
                break;
            }
        }
        if !complete {
            continue;
        }

        let signature_bytes = &header[..STEALTH_SIGNATURE_BYTES];
        let signature_info = if signature_bytes == b"stealth_pnginfo" {
            Some(("stealth_pnginfo", "Alpha", false))
        } else if signature_bytes == b"stealth_pngcomp" {
            Some(("stealth_pngcomp", "Alpha", true))
        } else if signature_bytes == b"stealth_rgbinfo" {
            Some(("stealth_rgbinfo", "RGB", false))
        } else if signature_bytes == b"stealth_rgbcomp" {
            Some(("stealth_rgbcomp", "RGB", true))
        } else {
            None
        };
        let Some((_, signature_mode, _)) = signature_info else {
            continue;
        };
        if signature_mode != mode {
            continue;
        }
        let bit_length =
            u32::from_be_bytes(header[STEALTH_SIGNATURE_BYTES..].try_into().ok()?) as usize;
        if bit_length % 8 != 0 {
            continue;
        }
        let channel_bits = total_pixels.checked_mul(if mode == "Alpha" { 1 } else { 3 })?;
        if bit_length / 8 > MAX_STEALTH_PAYLOAD_BYTES
            || STEALTH_HEADER_BITS.checked_add(bit_length)? > channel_bits
        {
            continue;
        }
        return Some(true);
    }
    Some(false)
}

struct PngIdatReader<'a> {
    chunks: Vec<&'a [u8]>,
    chunk_index: usize,
    chunk_offset: usize,
}

impl<'a> PngIdatReader<'a> {
    fn new(chunks: Vec<&'a [u8]>) -> Self {
        Self {
            chunks,
            chunk_index: 0,
            chunk_offset: 0,
        }
    }
}

impl<'a> Read for PngIdatReader<'a> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut written = 0;
        while written < buffer.len() && self.chunk_index < self.chunks.len() {
            let chunk = self.chunks[self.chunk_index];
            if self.chunk_offset == chunk.len() {
                self.chunk_index += 1;
                self.chunk_offset = 0;
                continue;
            }
            let amount = (buffer.len() - written).min(chunk.len() - self.chunk_offset);
            buffer[written..written + amount]
                .copy_from_slice(&chunk[self.chunk_offset..self.chunk_offset + amount]);
            written += amount;
            self.chunk_offset += amount;
        }
        Ok(written)
    }
}

fn check_stealth_png(png: &[u8]) -> Option<(String, String, String)> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if png.get(..8)? != PNG_SIGNATURE || png.get(12..16)? != b"IHDR" {
        return None;
    }
    let ihdr_len = u32::from_be_bytes(png.get(8..12)?.try_into().ok()?) as usize;
    if ihdr_len != 13 {
        return None;
    }
    let ihdr = png.get(16..29)?;
    let width = u32::from_be_bytes(ihdr[0..4].try_into().ok()?) as usize;
    let height = u32::from_be_bytes(ihdr[4..8].try_into().ok()?) as usize;
    let bit_depth = ihdr[8];
    let color_type = ihdr[9];
    // 拡張機能のステルススキャナーが対応する非インターレース配置に限定する。
    if width == 0
        || height == 0
        || width > MAX_STEALTH_DIMENSION
        || height > MAX_STEALTH_DIMENSION
        || ihdr[10] != 0
        || ihdr[11] != 0
        || ihdr[12] != 0
        || !matches!((color_type, bit_depth), (2 | 4 | 6, 8 | 16))
    {
        return None;
    }

    let mut cursor = 33usize; // signature + IHDR chunk and CRC
    let mut saw_idat = false;
    let mut idat_ended = false;
    let mut idat_chunks = Vec::new();
    let mut idat_size = 0usize;
    loop {
        let chunk_header = png.get(cursor..cursor.checked_add(8)?)?;
        let chunk_len = u32::from_be_bytes(chunk_header[..4].try_into().ok()?) as usize;
        let chunk_type = &chunk_header[4..8];
        let data_start = cursor.checked_add(8)?;
        let data_end = data_start.checked_add(chunk_len)?;
        let chunk_end = data_end.checked_add(4)?; // CRC
        let data = png.get(data_start..data_end)?;
        png.get(data_end..chunk_end)?; // CRC bytes must be present
        cursor = chunk_end;
        if chunk_type == b"IDAT" {
            idat_size = idat_size.checked_add(chunk_len)?;
            if idat_ended || idat_size > MAX_STEALTH_IDAT_BYTES {
                return None;
            }
            saw_idat = true;
            idat_chunks.push(data);
        } else {
            if saw_idat {
                idat_ended = true;
            }
            if chunk_type == b"IEND" {
                break;
            }
        }
    }
    if !saw_idat || idat_size == 0 {
        return None;
    }

    let channel_count = match color_type {
        2 => 3usize,
        4 => 2usize,
        6 => 4usize,
        _ => return None,
    };
    let bytes_per_sample = if bit_depth == 16 { 2usize } else { 1usize };
    let bytes_per_pixel = channel_count.checked_mul(bytes_per_sample)?;
    let row_bytes = width.checked_mul(bytes_per_pixel)?;
    let row_stride = row_bytes.checked_add(1)?;
    let decoded_size = row_stride.checked_mul(height)?;
    if decoded_size > MAX_STEALTH_DECODED_PNG_BYTES {
        return None;
    }

    let header_rows = if matches!(color_type, 4 | 6) {
        STEALTH_HEADER_BITS
    } else {
        STEALTH_HEADER_BITS.checked_add(2)? / 3
    };
    let prefix_rows = height.min(header_rows);
    let prefix_size = row_stride.checked_mul(prefix_rows)?;
    let mut decoder =
        ZlibDecoder::new(PngIdatReader::new(idat_chunks)).take(decoded_size as u64 + 1);
    let mut decoded = vec![0; prefix_size];
    decoder.read_exact(&mut decoded).ok()?;

    // Only complete rows enter the buffer and can be observed by the bit cursor.
    for y in 0..prefix_rows {
        unfilter_png_row(&mut decoded, y, row_bytes, bytes_per_pixel)?;
    }
    if !has_stealth_header(
        &decoded,
        width,
        height,
        prefix_rows,
        row_bytes,
        bytes_per_pixel,
        bytes_per_sample,
        color_type,
    )? {
        return None;
    }

    // A matching header justifies finishing the same stream; the already unfiltered prefix stays
    // in place and only the remaining complete rows are appended and restored.
    for y in prefix_rows..height {
        let row_end = decoded.len().checked_add(row_stride)?;
        let previous_len = decoded.len();
        decoded.resize(row_end, 0);
        decoder
            .read_exact(decoded.get_mut(previous_len..row_end)?)
            .ok()?;
        unfilter_png_row(&mut decoded, y, row_bytes, bytes_per_pixel)?;
    }
    if decoded.len() != decoded_size {
        return None;
    }
    let mut extra = [0u8; 1];
    if decoder.read(&mut extra).ok()? != 0 {
        return None;
    }

    for mode in ["Alpha", "RGB"] {
        let sample_channel = match (mode, color_type) {
            ("Alpha", 4) => Some(1usize),
            ("Alpha", 6) => Some(3usize),
            ("RGB", 2 | 6) => None,
            _ => continue,
        };
        let Some(mut bit_cursor) = StealthBitCursor::new(
            &decoded,
            width,
            height,
            height,
            row_bytes,
            bytes_per_pixel,
            bytes_per_sample,
            sample_channel,
        ) else {
            return None;
        };
        let channel_bits = bit_cursor.bit_capacity;
        if channel_bits < STEALTH_HEADER_BITS {
            continue;
        }

        let mut header = [0u8; STEALTH_SIGNATURE_BYTES + 4];
        for byte in &mut header {
            *byte = bit_cursor.next_byte()?;
        }
        let signature_bytes = &header[..STEALTH_SIGNATURE_BYTES];
        let (signature, signature_mode, compressed) = if signature_bytes == b"stealth_pnginfo" {
            ("stealth_pnginfo", "Alpha", false)
        } else if signature_bytes == b"stealth_pngcomp" {
            ("stealth_pngcomp", "Alpha", true)
        } else if signature_bytes == b"stealth_rgbinfo" {
            ("stealth_rgbinfo", "RGB", false)
        } else if signature_bytes == b"stealth_rgbcomp" {
            ("stealth_rgbcomp", "RGB", true)
        } else {
            continue;
        };
        if signature_mode != mode {
            continue;
        }
        let bit_length =
            u32::from_be_bytes(header[STEALTH_SIGNATURE_BYTES..].try_into().ok()?) as usize;
        if bit_length % 8 != 0 {
            continue;
        }
        let payload_len = bit_length / 8;
        if payload_len > MAX_STEALTH_PAYLOAD_BYTES
            || STEALTH_HEADER_BITS.checked_add(bit_length)? > channel_bits
        {
            continue;
        }
        let mut payload = Vec::with_capacity(payload_len);
        for _ in 0..payload_len {
            payload.push(bit_cursor.next_byte()?);
        }
        let text_bytes = if compressed {
            let Some(output) = inflate_stealth_payload(&payload) else {
                continue;
            };
            output
        } else {
            payload
        };
        if let Ok(text) = String::from_utf8(text_bytes) {
            return Some((mode.to_string(), signature.to_string(), text));
        }
    }
    None
}

fn inflate_stealth_payload(payload: &[u8]) -> Option<Vec<u8>> {
    read_limited_payload(GzDecoder::new(payload))
        .or_else(|| read_limited_payload(ZlibDecoder::new(payload)))
}

fn read_limited_payload<R: Read>(reader: R) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    let mut reader = reader.take(MAX_STEALTH_PAYLOAD_BYTES as u64 + 1);
    if reader.read_to_end(&mut output).is_err() || output.len() > MAX_STEALTH_PAYLOAD_BYTES {
        return None;
    }
    Some(output)
}

fn paeth_predictor(left: u8, above: u8, upper_left: u8) -> u8 {
    let left = left as i32;
    let above = above as i32;
    let upper_left = upper_left as i32;
    let predictor = left + above - upper_left;
    let left_distance = (predictor - left).abs();
    let above_distance = (predictor - above).abs();
    let upper_left_distance = (predictor - upper_left).abs();
    if left_distance <= above_distance && left_distance <= upper_left_distance {
        left as u8
    } else if above_distance <= upper_left_distance {
        above as u8
    } else {
        upper_left as u8
    }
}

fn unfilter_png_row(
    decoded: &mut [u8],
    y: usize,
    row_bytes: usize,
    bytes_per_pixel: usize,
) -> Option<()> {
    let row_stride = row_bytes.checked_add(1)?;
    let row_start = y.checked_mul(row_stride)?;
    let (before_row, current_with_filter) = decoded.split_at_mut(row_start);
    let (filter, current_row_and_rest) = current_with_filter.split_first_mut()?;
    let current_row = current_row_and_rest.get_mut(..row_bytes)?;
    let previous_row = if y == 0 {
        None
    } else {
        let previous_start = row_start.checked_sub(row_stride)?.checked_add(1)?;
        let previous_end = previous_start.checked_add(row_bytes)?;
        Some(before_row.get(previous_start..previous_end)?)
    };

    match *filter {
        0 => {}
        1 => {
            for x in bytes_per_pixel..row_bytes {
                let left = current_row[x - bytes_per_pixel];
                current_row[x] = current_row[x].wrapping_add(left);
            }
        }
        2 => {
            if let Some(above) = previous_row {
                for x in 0..row_bytes {
                    current_row[x] = current_row[x].wrapping_add(above[x]);
                }
            }
        }
        3 => match previous_row {
            None => {
                for x in bytes_per_pixel..row_bytes {
                    let left = current_row[x - bytes_per_pixel];
                    current_row[x] = current_row[x].wrapping_add(left / 2);
                }
            }
            Some(above) => {
                for x in 0..bytes_per_pixel {
                    current_row[x] = current_row[x].wrapping_add(above[x] / 2);
                }
                for x in bytes_per_pixel..row_bytes {
                    let left = current_row[x - bytes_per_pixel];
                    let average = ((left as u16 + above[x] as u16) / 2) as u8;
                    current_row[x] = current_row[x].wrapping_add(average);
                }
            }
        },
        4 => match previous_row {
            None => {
                for x in bytes_per_pixel..row_bytes {
                    let left = current_row[x - bytes_per_pixel];
                    current_row[x] = current_row[x].wrapping_add(left);
                }
            }
            Some(above) => {
                for x in 0..bytes_per_pixel {
                    current_row[x] = current_row[x].wrapping_add(above[x]);
                }
                for x in bytes_per_pixel..row_bytes {
                    let left = current_row[x - bytes_per_pixel];
                    let upper_left = above[x - bytes_per_pixel];
                    current_row[x] =
                        current_row[x].wrapping_add(paeth_predictor(left, above[x], upper_left));
                }
            }
        },
        _ => return None,
    }
    Some(())
}

fn extract_webp_metadata<W: Write>(
    mmap: &[u8],
    path: &str,
    writer: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    writeln!(writer, "=== WebP File: {} ===", path)?;
    let mut sink = output::TextSink::default();
    for record in webp_records(mmap) {
        sink.push_record(record);
    }
    let found = !sink.is_empty();
    sink.flush(writer)?;
    if !found {
        writeln!(writer, "(No metadata detected)")?;
    }
    Ok(())
}

fn webp_records(mmap: &[u8]) -> Vec<record::RecordView<'_>> {
    if mmap.get(..4) != Some(b"RIFF") || mmap.get(8..12) != Some(b"WEBP") {
        return Vec::new();
    }
    let mut cursor = 12usize;
    let len = mmap.len();
    let mut records = Vec::new();
    while let Some(header_end) = cursor.checked_add(8) {
        if header_end > len {
            break;
        }
        let Some(chunk_type_end) = cursor.checked_add(4) else {
            break;
        };
        let Some(chunk_type) = mmap.get(cursor..chunk_type_end) else {
            break;
        };
        let Some(size_bytes) = mmap.get(chunk_type_end..header_end) else {
            break;
        };
        let Ok(size_bytes) = <[u8; 4]>::try_from(size_bytes) else {
            break;
        };
        let chunk_size = u32::from_le_bytes(size_bytes) as usize;
        let data_start = header_end;
        let Some(data_end) = data_start.checked_add(chunk_size) else {
            break;
        };
        if data_end > len {
            break;
        }

        if chunk_type == b"EXIF" {
            let exif_data = &mmap[data_start..data_end];
            records.extend(tiff::parse(exif_data, record::Source::WebpExif));
        } else if chunk_type == b"XMP " {
            records.extend(xmp::parse(
                &mmap[data_start..data_end],
                record::Source::WebpXmp,
            ));
        }

        let Some(next_cursor) = data_end.checked_add(chunk_size % 2) else {
            break;
        };
        cursor = next_cursor;
    }
    records
}

fn extract_exif_metadata<W: Write>(
    mmap: &[u8],
    path: &str,
    format: &str,
    writer: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    writeln!(writer, "=== {} File: {} ===", format, path)?;
    let parsed = jpeg::scan(mmap);
    let has_records = !parsed.records.is_empty();
    let mut sink = output::TextSink::default();
    for record in parsed.records {
        sink.push_record(record);
    }
    sink.flush(writer)?;
    if !has_records {
        if parsed.saw_exif {
            writeln!(writer, "(No metadata detected)")?;
        } else {
            writeln!(writer, "(No EXIF marker found)")?;
        }
    }
    Ok(())
}

fn extract_universal_metadata<W: Write>(
    path: &str,
    strict: bool,
    stealth_policy: StealthPolicy,
    writer: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let mmap = unsafe { MmapOptions::new().map(&file)? };

    if mmap.len() >= 8 && &mmap[0..8] == &[137, 80, 78, 71, 13, 10, 26, 10] {
        return extract_png_metadata(&mmap, path, strict, stealth_policy, writer);
    }
    if mmap.len() >= 12 && &mmap[0..4] == b"RIFF" && &mmap[8..12] == b"WEBP" {
        return extract_webp_metadata(&mmap, path, writer);
    }
    if mmap.len() >= 3 && &mmap[0..3] == &[0xFF, 0xD8, 0xFF] {
        #[cfg(unix)]
        let _ = mmap.advise(memmap2::Advice::Random);
        return extract_exif_metadata(&mmap, path, "JPEG", writer);
    }

    eprintln!("Unsupported file format");
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut strict = false;
    let mut stealth_policy = StealthPolicy::Auto;
    let mut image_path = None;
    let mut arguments = args.iter().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--strict" => strict = true,
            "--stealth" => {
                let Some(value) = arguments
                    .next()
                    .and_then(|value| StealthPolicy::parse(value))
                else {
                    eprintln!(
                        "Usage: {} [--strict] [--stealth <auto|always|never>] <image_file>",
                        args[0]
                    );
                    std::process::exit(1);
                };
                stealth_policy = value;
            }
            _ if argument.starts_with("--") || image_path.is_some() => {
                eprintln!(
                    "Usage: {} [--strict] [--stealth <auto|always|never>] <image_file>",
                    args[0]
                );
                std::process::exit(1);
            }
            _ => image_path = Some(argument.as_str()),
        }
    }
    let Some(image_path) = image_path else {
        eprintln!(
            "Usage: {} [--strict] [--stealth <auto|always|never>] <image_file>",
            args[0]
        );
        std::process::exit(1);
    };
    let stdout = io::stdout();
    let mut writer = io::BufWriter::new(stdout.lock());
    let result = extract_universal_metadata(image_path, strict, stealth_policy, &mut writer);
    let flush_result = writer.flush();
    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
    if let Err(e) = flush_result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod webp_tests {
    use super::webp_records;
    use crate::record::Source;

    fn webp_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = Vec::new();
        chunk.extend_from_slice(kind);
        chunk.extend_from_slice(&(data.len() as u32).to_le_bytes());
        chunk.extend_from_slice(data);
        if data.len() % 2 != 0 {
            chunk.push(0);
        }
        chunk
    }

    fn webp(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut body = b"WEBP".to_vec();
        for chunk in chunks {
            body.extend_from_slice(chunk);
        }
        let mut image = b"RIFF".to_vec();
        image.extend_from_slice(&(body.len() as u32).to_le_bytes());
        image.extend_from_slice(&body);
        image
    }

    #[test]
    fn fixture_23_reads_webp_xmp_element_description() {
        let packet = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/'><d:description>a WebP prompt</d:description></rdf:Description></rdf:RDF></x:xmpmeta>";
        let image = webp(&[webp_chunk(b"XMP ", packet)]);
        let records = webp_records(&image);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].source, Source::WebpXmp);
        assert_eq!(records[0].label, "XMP Description");
        assert_eq!(records[0].body, "a WebP prompt");
    }
}
