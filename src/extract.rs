use flate2::read::{GzDecoder, ZlibDecoder};
use memmap2::MmapOptions;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use crate::record::{self, RecordView};
use crate::{jpeg, output, png, tiff, xmp};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StealthPolicy {
    #[default]
    Auto,
    Always,
    Never,
}

impl StealthPolicy {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanOptions {
    pub strict: bool,
    pub stealth: StealthPolicy,
}

/// レコード受け取り用の公開 sink。レコードの借用は `on_record` の呼び出し中のみ有効で、
/// 保持したい場合は実装側で所有化すること。
pub trait RecordSink {
    fn on_record(&mut self, record: &RecordView<'_>);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    Png,
    Jpeg,
    Webp,
}

#[derive(Clone, Debug, Default)]
pub struct ScanOutcome {
    /// 未対応形式では None。
    pub format: Option<Format>,
    /// sink へ渡したレコード数。
    pub record_count: usize,
    /// JPEG: EXIF マーカーの有無。
    pub saw_exif: bool,
    /// PNG: CLI の tail 案内文言の条件。
    pub comfy_tail_notice: bool,
}

#[derive(Debug)]
pub enum ScanError {
    Io(std::io::Error),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
        }
    }
}

impl From<std::io::Error> for ScanError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

fn format_of(bytes: &[u8]) -> Option<Format> {
    if bytes.len() >= 8 && bytes[0..8] == [137, 80, 78, 71, 13, 10, 26, 10] {
        Some(Format::Png)
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(Format::Webp)
    } else if bytes.len() >= 3 && bytes[0..3] == [0xFF, 0xD8, 0xFF] {
        Some(Format::Jpeg)
    } else {
        None
    }
}

/// REQ-EXT-01: Stealth Auto ゲート。「生成内容／生成パラメータを担うレコード」
/// （実質レコード）が 1 件でもあるときだけ検査を閉じる。汎用レコードだけでは閉じない。
/// 値ではなくレコード種別で決める（抽出層の検査要否であって生成元の判定ではない）。
fn is_substantive_record(record: &RecordView<'_>) -> bool {
    // Trailing は汎用側（ゲートを閉じない）。
    if matches!(record.source, record::Source::Trailing) {
        return false;
    }
    match record.source {
        record::Source::PngText => !matches!(
            record.keyword.as_deref(),
            Some("Software" | "Title" | "Source")
        ),
        record::Source::PngExif
        | record::Source::JpegExif
        | record::Source::WebpExif => !matches!(
            record.keyword.as_deref(),
            Some("Software" | "Make" | "Model" | "Artist" | "XPComment")
        ),
        record::Source::JpegComment => true,
        record::Source::PngXmp
        | record::Source::JpegXmp
        | record::Source::WebpXmp
        | record::Source::TiffXmp => !matches!(
            record.keyword.as_deref(),
            Some("CreatorTool" | "title")
        ),
        record::Source::JpegIptc => !matches!(record.keyword.as_deref(), Some("Caption/Abstract")),
        // 表に無い種別は「内容を担う側」に倒す（仕様書 §5 REQ-EXT-01 の注記）。
        _ => true,
    }
}

fn has_substantive_record(records: &[RecordView<'_>]) -> bool {
    records.iter().any(is_substantive_record)
}

#[cfg(test)]
mod stealth_gate_tests {
    use super::*;
    use std::borrow::Cow;

    fn record(
        source: record::Source,
        body: &'static str,
        keyword: Option<&'static str>,
    ) -> RecordView<'static> {
        RecordView {
            source,
            label: Cow::Borrowed("label"),
            body: Cow::Borrowed(body),
            keyword: keyword.map(Cow::Borrowed),
            detail: None,
        }
    }

    /// T-40: 実質レコード（閉じる側）の代表。
    #[test]
    fn t40_substantive_records_close_the_stealth_gate() {
        for record in [
            record(record::Source::PngText, "Steps: 20", Some("parameters")),
            record(record::Source::PngText, "a cat", Some("Description")),
            record(record::Source::JpegExif, "a cat", Some("UserComment")),
            record(record::Source::PngExif, "a cat", Some("ImageDescription")),
            record(record::Source::JpegComment, "a cat", None),
            record(record::Source::PngXmp, "a cat", Some("description")),
            record(record::Source::PngXmp, "a cat", Some("AIPromptInformation")),
            record(record::Source::PngXmp, "x", Some("DigitalSourceType")),
        ] {
            assert!(
                has_substantive_record(&[record.clone()]),
                "{:?}/{:?} must close the gate",
                record.source,
                record.keyword
            );
        }
    }

    /// T-40: 汎用レコード（閉じない側）の代表。
    #[test]
    fn t40_generic_records_keep_the_stealth_gate_open() {
        for record in [
            record(record::Source::PngText, "Celsys Studio Tool", Some("Software")),
            record(record::Source::PngText, "a title", Some("Title")),
            record(record::Source::PngExif, "Adobe", Some("Software")),
            record(record::Source::JpegXmp, "Midjourney", Some("CreatorTool")),
            record(record::Source::PngXmp, "a title", Some("title")),
            record(record::Source::JpegIptc, "caption", Some("Caption/Abstract")),
            record(record::Source::Trailing, "novelai", None),
        ] {
            assert!(
                !has_substantive_record(&[record.clone()]),
                "{:?}/{:?} must keep the gate open",
                record.source,
                record.keyword
            );
        }
    }
}

fn stealth_record(result: Option<(String, String, String)>) -> Option<RecordView<'static>> {
    let (mode, signature, body) = result?;
    Some(RecordView {
        source: record::Source::PngStealth,
        label: std::borrow::Cow::Owned(format!("Stealth PNG Info ({mode}) [{signature}]")),
        body: std::borrow::Cow::Owned(body),
        keyword: None,
        detail: Some(std::borrow::Cow::Owned(format!("{mode}/{signature}"))),
    })
}

/// path のファイルを mmap し、形式判定→形式別走査→（PNG のみ）Stealth 判定の順に
/// レコードを sink へ渡す。レコードの借用は本関数の呼び出し中のみ有効。
pub fn scan_file(
    path: &Path,
    options: &ScanOptions,
    sink: &mut dyn RecordSink,
) -> Result<ScanOutcome, ScanError> {
    let file = File::open(path)?;
    if file.metadata()?.len() == 0 {
        return Err(ScanError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "cannot map a zero-length file",
        )));
    }
    let mmap = unsafe { MmapOptions::new().map(&file)? };
    let Some(format) = format_of(&mmap) else {
        return Ok(ScanOutcome::default());
    };

    let mut outcome = ScanOutcome {
        format: Some(format),
        ..ScanOutcome::default()
    };
    match format {
        Format::Png => {
            let result = png::scan(&mmap, options.strict);
            outcome.comfy_tail_notice = result.stats.comfy_tail_notice;
            let stealth = match options.stealth {
                StealthPolicy::Never => None,
                StealthPolicy::Auto if has_substantive_record(&result.records) => None,
                StealthPolicy::Auto | StealthPolicy::Always => check_stealth_png(&mmap),
            };
            for record in &result.records {
                sink.on_record(record);
                outcome.record_count += 1;
            }
            if let Some(record) = stealth_record(stealth) {
                sink.on_record(&record);
                outcome.record_count += 1;
            }
        }
        Format::Jpeg => {
            #[cfg(unix)]
            let _ = mmap.advise(memmap2::Advice::Random);
            let parsed = jpeg::scan(&mmap);
            outcome.saw_exif = parsed.saw_exif;
            for record in &parsed.records {
                sink.on_record(record);
                outcome.record_count += 1;
            }
        }
        Format::Webp => {
            let records = webp_records(&mmap);
            for record in &records {
                sink.on_record(record);
                outcome.record_count += 1;
            }
        }
    }
    Ok(outcome)
}

/// CLI 用：見出し・案内文言・レコード出力を mmap と同じフレーム内で行う。
/// 診断文（レコードでも見出しでもない案内文言）は従来どおり `out` へ出す。
pub fn extract_to_writer(
    path: &Path,
    options: &ScanOptions,
    out: &mut dyn Write,
) -> Result<ScanOutcome, ScanError> {
    // REQ-DSP-02: 呼び出し側が診断文の行き先を分けられるよう、同じ writer を共有する。
    let shared = std::cell::RefCell::new(out);
    let mut primary = SharedWriter {
        inner: &shared,
    };
    let mut diagnostics = SharedWriter {
        inner: &shared,
    };
    extract_universal_metadata(path, options, &mut primary, &mut diagnostics)
}

/// REQ-DSP-02: レコードと見出しは `out`、診断文は `diagnostics` へ出す。
/// `out` の内容（見出し行とレコード出力）は従来どおりで、診断文だけが分離される。
pub fn extract_to_writer_with_diagnostics(
    path: &Path,
    options: &ScanOptions,
    out: &mut dyn Write,
    diagnostics: &mut dyn Write,
) -> Result<ScanOutcome, ScanError> {
    extract_universal_metadata(path, options, out, diagnostics)
}

/// 同じ writer を 2 つの `&mut dyn Write` から使うための小さな共有ラッパー。
struct SharedWriter<'a, 'b: 'a> {
    inner: &'a std::cell::RefCell<&'b mut dyn Write>,
}

impl Write for SharedWriter<'_, '_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.inner.borrow_mut().write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.borrow_mut().flush()
    }
}


fn extract_png_metadata<W: Write + ?Sized>(
    mmap: &[u8],
    path: &str,
    strict: bool,
    stealth_policy: StealthPolicy,
    writer: &mut W,
    diagnostics: &mut dyn Write,
) -> Result<(usize, bool), ScanError> {
    use crate::output::TextSink;
    let result = png::scan(mmap, strict);
    let comfy_tail_notice = result.stats.comfy_tail_notice;
    writeln!(writer, "=== PNG File: {} ===", path)?;
    if comfy_tail_notice {
        // REQ-DSP-02: 案内文言はレコードでも見出しでもないため stderr 側へ出す。
        writeln!(
            diagnostics,
            "(Large file or ComfyUI detected. Scanning tail for metadata...)"
        )?;
    }
    let stealth_result = match stealth_policy {
        StealthPolicy::Never => None,
        StealthPolicy::Auto if has_substantive_record(&result.records) => None,
        StealthPolicy::Auto | StealthPolicy::Always => check_stealth_png(mmap),
    };
    let mut sink = TextSink::default();
    for record in result.records {
        sink.push_record(record);
    }
    if let Some(record) = stealth_record(stealth_result) {
        sink.push_record(record);
    }
    let record_count = sink.len();
    let has_any_metadata_record = !sink.is_empty();
    sink.flush(writer)?;

    if !has_any_metadata_record {
        writeln!(writer, "(No metadata detected)")?;
    }

    Ok((record_count, comfy_tail_notice))
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

fn extract_webp_metadata<W: Write + ?Sized>(
    mmap: &[u8],
    path: &str,
    writer: &mut W,
) -> Result<usize, ScanError> {
    writeln!(writer, "=== WebP File: {} ===", path)?;
    let mut sink = output::TextSink::default();
    for record in webp_records(mmap) {
        sink.push_record(record);
    }
    let found = !sink.is_empty();
    let record_count = sink.len();
    sink.flush(writer)?;
    if !found {
        writeln!(writer, "(No metadata detected)")?;
    }
    Ok(record_count)
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

fn extract_exif_metadata<W: Write + ?Sized>(
    mmap: &[u8],
    path: &str,
    format: &str,
    writer: &mut W,
) -> Result<(usize, bool), ScanError> {
    writeln!(writer, "=== {} File: {} ===", format, path)?;
    let parsed = jpeg::scan(mmap);
    let has_records = !parsed.records.is_empty();
    let saw_exif = parsed.saw_exif;
    let mut record_count = 0;
    let mut sink = output::TextSink::default();
    for record in parsed.records {
        record_count += 1;
        sink.push_record(record);
    }
    sink.flush(writer)?;
    if !has_records {
        if saw_exif {
            writeln!(writer, "(No metadata detected)")?;
        } else {
            writeln!(writer, "(No EXIF marker found)")?;
        }
    }
    Ok((record_count, saw_exif))
}

fn extract_universal_metadata<W: Write + ?Sized>(
    path: &Path,
    options: &ScanOptions,
    writer: &mut W,
    diagnostics: &mut dyn Write,
) -> Result<ScanOutcome, ScanError> {
    let file = File::open(path)?;
    if file.metadata()?.len() == 0 {
        return Err(ScanError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "cannot map a zero-length file",
        )));
    }
    let mmap = unsafe { MmapOptions::new().map(&file)? };
    let display_path = path.to_string_lossy();

    let Some(format) = format_of(&mmap) else {
        return Ok(ScanOutcome::default());
    };

    match format {
        Format::Png => {
            let (record_count, comfy_tail_notice) = extract_png_metadata(
                &mmap,
                &display_path,
                options.strict,
                options.stealth,
                writer,
                diagnostics,
            )?;
            Ok(ScanOutcome {
                format: Some(format),
                record_count,
                saw_exif: false,
                comfy_tail_notice,
            })
        }
        Format::Jpeg => {
            #[cfg(unix)]
            let _ = mmap.advise(memmap2::Advice::Random);
            let (record_count, saw_exif) =
                extract_exif_metadata(&mmap, &display_path, "JPEG", writer)?;
            Ok(ScanOutcome {
                format: Some(format),
                record_count,
                saw_exif,
                comfy_tail_notice: false,
            })
        }
        Format::Webp => {
            let record_count = extract_webp_metadata(&mmap, &display_path, writer)?;
            Ok(ScanOutcome {
                format: Some(format),
                record_count,
                saw_exif: false,
                comfy_tail_notice: false,
            })
        }
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

#[cfg(test)]
mod scan_file_tests {
    use super::*;
    use crate::record::Source;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn new(extension: &str, bytes: &[u8]) -> Self {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "fast-meta-lib-{}-{id}.{extension}",
                std::process::id()
            ));
            std::fs::write(&path, bytes).expect("write fixture");
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[derive(Default)]
    struct VecSink {
        records: Vec<(Source, String, String, Option<String>, Option<String>)>,
    }

    impl RecordSink for VecSink {
        fn on_record(&mut self, record: &RecordView<'_>) {
            self.records.push((
                record.source,
                record.label.to_string(),
                record.body.to_string(),
                record.keyword.as_ref().map(ToString::to_string),
                record.detail.as_ref().map(ToString::to_string),
            ));
        }
    }

    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
        chunk.extend_from_slice(kind);
        chunk.extend_from_slice(data);
        chunk.extend_from_slice(&crc32fast::hash(&chunk[4..]).to_be_bytes());
        chunk
    }

    fn text_chunk(keyword: &str, text: &str) -> Vec<u8> {
        let mut data = keyword.as_bytes().to_vec();
        data.push(0);
        data.extend_from_slice(text.as_bytes());
        png_chunk(b"tEXt", &data)
    }

    fn png_with_text(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&[0, 0, 0, 0, 0]).unwrap();
        let idat = encoder.finish().unwrap();
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        png.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
        for chunk in chunks {
            png.extend_from_slice(chunk);
        }
        png.extend_from_slice(&png_chunk(b"IDAT", &idat));
        png.extend_from_slice(&png_chunk(b"IEND", &[]));
        png
    }

    fn jpeg_segment(marker: u8, data: &[u8]) -> Vec<u8> {
        let mut segment = vec![0xff, marker];
        segment.extend_from_slice(&((data.len() + 2) as u16).to_be_bytes());
        segment.extend_from_slice(data);
        segment
    }

    fn jpeg_with_user_comment(text: &str) -> Vec<u8> {
        let mut user_comment = b"ASCII\0\0\0".to_vec();
        user_comment.extend_from_slice(text.as_bytes());
        let mut tiff = b"II*\0".to_vec();
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&[0x86, 0x92]);
        tiff.extend_from_slice(&7u16.to_le_bytes());
        tiff.extend_from_slice(&(user_comment.len() as u32).to_le_bytes());
        tiff.extend_from_slice(&26u32.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(&user_comment);
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);

        let mut jpeg = vec![0xff, 0xd8];
        jpeg.extend_from_slice(&jpeg_segment(0xe1, &app1));
        jpeg.extend_from_slice(&jpeg_segment(0xdb, &[0, 1, 1, 1, 1]));
        jpeg.extend_from_slice(&jpeg_segment(0xda, &[1, 1, 0, 0, 63, 0]));
        jpeg.extend_from_slice(&[0x3f, 0xff, 0xd9]);
        jpeg
    }

    fn webp_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = kind.to_vec();
        chunk.extend_from_slice(&(data.len() as u32).to_le_bytes());
        chunk.extend_from_slice(data);
        if data.len() % 2 == 1 {
            chunk.push(0);
        }
        chunk
    }

    fn webp_with_xmp() -> Vec<u8> {
        let xmp = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/'><d:description>library prompt</d:description></rdf:Description></rdf:RDF></x:xmpmeta>";
        let mut body = b"WEBP".to_vec();
        body.extend_from_slice(&webp_chunk(b"XMP ", xmp));
        let mut image = b"RIFF".to_vec();
        image.extend_from_slice(&(body.len() as u32).to_le_bytes());
        image.extend_from_slice(&body);
        image
    }

    fn compare(
        sink: &VecSink,
        expected: impl Iterator<Item = (Source, String, String, Option<String>, Option<String>)>,
    ) {
        let expected: Vec<_> = expected.collect();
        assert_eq!(sink.records, expected);
    }

    #[test]
    fn scan_file_png_matches_direct_scan_order() {
        let bytes = png_with_text(&[
            text_chunk("Title", "first"),
            text_chunk("parameters", "second"),
        ]);
        let fixture = TempFile::new("png", &bytes);
        let mut sink = VecSink::default();
        let outcome = scan_file(&fixture.path, &ScanOptions::default(), &mut sink).unwrap();
        assert_eq!(outcome.format, Some(Format::Png));
        assert_eq!(outcome.record_count, 2);

        let direct = png::scan(&bytes, false);
        compare(
            &sink,
            direct.records.iter().map(|record| {
                (
                    record.source,
                    record.label.to_string(),
                    record.body.to_string(),
                    record.keyword.as_ref().map(ToString::to_string),
                    record.detail.as_ref().map(ToString::to_string),
                )
            }),
        );
    }

    #[test]
    fn scan_file_jpeg_matches_direct_scan_order() {
        let bytes = jpeg_with_user_comment("parameters=library jpeg");
        let fixture = TempFile::new("jpg", &bytes);
        let mut sink = VecSink::default();
        let outcome = scan_file(&fixture.path, &ScanOptions::default(), &mut sink).unwrap();
        assert_eq!(outcome.format, Some(Format::Jpeg));
        assert!(outcome.saw_exif);
        assert_eq!(outcome.record_count, sink.records.len());

        let direct = jpeg::scan(&bytes);
        compare(
            &sink,
            direct.records.iter().map(|record| {
                (
                    record.source,
                    record.label.to_string(),
                    record.body.to_string(),
                    record.keyword.as_ref().map(ToString::to_string),
                    record.detail.as_ref().map(ToString::to_string),
                )
            }),
        );
    }

    #[test]
    fn scan_file_webp_matches_direct_scan_order() {
        let bytes = webp_with_xmp();
        let fixture = TempFile::new("webp", &bytes);
        let mut sink = VecSink::default();
        let outcome = scan_file(&fixture.path, &ScanOptions::default(), &mut sink).unwrap();
        assert_eq!(outcome.format, Some(Format::Webp));
        assert_eq!(outcome.record_count, sink.records.len());

        compare(
            &sink,
            webp_records(&bytes).iter().map(|record| {
                (
                    record.source,
                    record.label.to_string(),
                    record.body.to_string(),
                    record.keyword.as_ref().map(ToString::to_string),
                    record.detail.as_ref().map(ToString::to_string),
                )
            }),
        );
    }

    #[test]
    fn scan_file_reports_unsupported_format_without_error() {
        let fixture = TempFile::new("bin", b"not an image");
        let mut sink = VecSink::default();
        let outcome = scan_file(&fixture.path, &ScanOptions::default(), &mut sink).unwrap();
        assert_eq!(outcome.format, None);
        assert_eq!(outcome.record_count, 0);
        assert!(sink.records.is_empty());
    }

    #[test]
    fn scan_file_reports_zero_byte_file_as_io_error() {
        let fixture = TempFile::new("png", b"");
        let mut sink = VecSink::default();
        assert!(matches!(
            scan_file(&fixture.path, &ScanOptions::default(), &mut sink),
            Err(ScanError::Io(_))
        ));
    }

}
