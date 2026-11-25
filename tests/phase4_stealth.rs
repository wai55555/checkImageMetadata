use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
static NEXT_FILE_ID: AtomicUsize = AtomicUsize::new(0);

struct TempPng {
    path: PathBuf,
}

impl TempPng {
    fn new(bytes: &[u8]) -> Self {
        let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fast_meta_phase4_{}_{}.png",
            std::process::id(),
            id
        ));
        fs::write(&path, bytes).expect("write temporary PNG fixture");
        Self { path }
    }
}

impl Drop for TempPng {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(data.len() + 12);
    chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(data);
    chunk.extend_from_slice(&crc32fast::hash(&chunk[4..]).to_be_bytes());
    chunk
}

fn compress(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).expect("compress fixture IDAT");
    encoder.finish().expect("finish fixture IDAT")
}

fn paeth_predictor(left: u8, above: u8, upper_left: u8) -> u8 {
    let predictor = left as i32 + above as i32 - upper_left as i32;
    let left_distance = (predictor - left as i32).abs();
    let above_distance = (predictor - above as i32).abs();
    let upper_left_distance = (predictor - upper_left as i32).abs();
    if left_distance <= above_distance && left_distance <= upper_left_distance {
        left
    } else if above_distance <= upper_left_distance {
        above
    } else {
        upper_left
    }
}

fn stealth_png(payload: &[u8], text_chunk: Option<Vec<u8>>) -> Vec<u8> {
    stealth_png_variant(payload, "stealth_pnginfo", 8, false, false, text_chunk)
}

fn stealth_png_variant(
    payload: &[u8],
    signature: &str,
    bit_depth: u8,
    rgb: bool,
    compressed: bool,
    text_chunk: Option<Vec<u8>>,
) -> Vec<u8> {
    stealth_png_variant_with_max_height(
        payload, signature, bit_depth, rgb, compressed, text_chunk, 23,
    )
}

fn stealth_png_variant_with_max_height(
    payload: &[u8],
    signature: &str,
    bit_depth: u8,
    rgb: bool,
    compressed: bool,
    text_chunk: Option<Vec<u8>>,
    max_height: usize,
) -> Vec<u8> {
    let encoded_payload = if compressed {
        compress(payload)
    } else {
        payload.to_vec()
    };
    let mut stream = signature.as_bytes().to_vec();
    stream.extend_from_slice(&((encoded_payload.len() * 8) as u32).to_be_bytes());
    stream.extend_from_slice(&encoded_payload);

    // Use multiple columns so fixtures cover the extension's column-major pixel order.
    let channels_per_stream_pixel = if rgb { 3 } else { 1 };
    let stream_pixel_count = (stream.len() * 8).div_ceil(channels_per_stream_pixel);
    let height = stream_pixel_count.min(max_height);
    let width = stream_pixel_count.div_ceil(height);
    let bytes_per_sample = usize::from(bit_depth / 8);
    let row_bytes = width * 4 * bytes_per_sample;
    let row_stride = row_bytes + 1;
    let mut raw = vec![0; height * row_stride];
    for bit_index in 0..stream.len() * 8 {
        let bit = (stream[bit_index / 8] >> (7 - bit_index % 8)) & 1;
        let pixel_index = bit_index / channels_per_stream_pixel;
        let x = pixel_index / height;
        let y = pixel_index % height;
        let channel_idx = if rgb {
            bit_index % channels_per_stream_pixel
        } else {
            3
        };
        let sample_offset = usize::from(bit_depth == 16);
        let sample_index =
            x * 4 * bytes_per_sample + channel_idx * bytes_per_sample + sample_offset;
        raw[y * row_stride + 1 + sample_index] |= bit;
    }
    let bytes_per_pixel = 4 * bytes_per_sample;
    let mut filtered = vec![0; raw.len()];
    for y in 0..height {
        let row_start = y * row_stride;
        let filter = (y % 5) as u8;
        filtered[row_start] = filter;
        for x in 0..row_bytes {
            let value = raw[row_start + 1 + x];
            let left = if x >= bytes_per_pixel {
                raw[row_start + 1 + x - bytes_per_pixel]
            } else {
                0
            };
            let above = if y == 0 {
                0
            } else {
                raw[row_start - row_stride + 1 + x]
            };
            let upper_left = if y == 0 || x < bytes_per_pixel {
                0
            } else {
                raw[row_start - row_stride + 1 + x - bytes_per_pixel]
            };
            let predictor = match filter {
                0 => 0,
                1 => left,
                2 => above,
                3 => ((left as u16 + above as u16) / 2) as u8,
                4 => paeth_predictor(left, above, upper_left),
                _ => unreachable!(),
            };
            filtered[row_start + 1 + x] = value.wrapping_sub(predictor);
        }
    }

    let mut png = PNG_SIGNATURE.to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[bit_depth, 6, 0, 0, 0]);
    png.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
    if let Some(chunk) = text_chunk {
        png.extend_from_slice(&chunk);
    }
    png.extend_from_slice(&png_chunk(b"IDAT", &compress(&filtered)));
    png.extend_from_slice(&png_chunk(b"IEND", &[]));
    png
}

fn corrupt_idat_zlib_checksum(mut png: Vec<u8>) -> Vec<u8> {
    let mut cursor = PNG_SIGNATURE.len();
    loop {
        let chunk_len = u32::from_be_bytes(png[cursor..cursor + 4].try_into().unwrap()) as usize;
        let chunk_type = &png[cursor + 4..cursor + 8];
        let data_start = cursor + 8;
        let data_end = data_start + chunk_len;
        if chunk_type == b"IDAT" {
            assert!(chunk_len >= 4);
            png[data_end - 1] ^= 1;
            let crc = crc32fast::hash(&png[cursor + 4..data_end]);
            png[data_end..data_end + 4].copy_from_slice(&crc.to_be_bytes());
            return png;
        }
        cursor = data_end + 4;
    }
}

fn text_chunk(keyword: &str, text: &str) -> Vec<u8> {
    let mut data = keyword.as_bytes().to_vec();
    data.push(0);
    data.extend_from_slice(text.as_bytes());
    png_chunk(b"tEXt", &data)
}

fn run_cli(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_fast_meta"))
        .args(args)
        .output()
        .expect("run fast_meta");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("CLI output is UTF-8")
}

#[test]
fn fixture_11_stealth_only_is_found_by_default_auto_policy() {
    let png = TempPng::new(&stealth_png(b"fixture 11 payload", None));
    let output = run_cli(&[png.path.to_str().unwrap()]);

    assert!(output.contains("--- Stealth PNG Info (Alpha) [stealth_pnginfo] ---"));
    assert!(output.contains("fixture 11 payload"));
    assert!(!output.contains("(No metadata detected)"));
}

#[test]
fn stealth_scan_restores_rows_using_each_png_filter() {
    let png = TempPng::new(&stealth_png(b"filters 0 through 4", None));
    let output = run_cli(&[png.path.to_str().unwrap()]);

    assert!(output.contains("filters 0 through 4"));
}

#[test]
fn fixture_12_always_shows_stealth_alongside_normal_metadata() {
    let png = TempPng::new(&stealth_png(
        b"fixture 12 payload",
        Some(text_chunk("parameters", "prompt=ordinary metadata")),
    ));
    let path = png.path.to_str().unwrap();

    let always_output = run_cli(&["--stealth", "always", path]);
    assert!(always_output.contains("Stable Diffusion (A1111)"));
    assert!(always_output.contains("prompt=ordinary metadata"));
    assert!(always_output.contains("Stealth PNG Info (Alpha) [stealth_pnginfo]"));
    assert!(always_output.contains("fixture 12 payload"));
    assert!(
        always_output.find("Stable Diffusion (A1111)").unwrap()
            < always_output.find("Stealth PNG Info").unwrap()
    );

    let auto_output = run_cli(&["--stealth", "auto", path]);
    assert!(auto_output.contains("prompt=ordinary metadata"));
    assert!(!auto_output.contains("Stealth PNG Info"));
}

#[test]
fn fixture_31_reads_16_bit_alpha_from_the_second_sample_byte() {
    let bytes = stealth_png_variant(
        b"fixture 31 16-bit payload",
        "stealth_pnginfo",
        16,
        false,
        false,
        None,
    );
    let png = TempPng::new(&bytes);
    let output = run_cli(&[png.path.to_str().unwrap()]);

    assert!(output.contains("fixture 31 16-bit payload"));
}

#[test]
fn fixture_31_reads_rgb_channel_stream() {
    let bytes = stealth_png_variant(
        b"fixture 31 RGB payload",
        "stealth_rgbinfo",
        8,
        true,
        false,
        None,
    );
    let png = TempPng::new(&bytes);
    let output = run_cli(&[png.path.to_str().unwrap()]);

    assert!(output.contains("Stealth PNG Info (RGB) [stealth_rgbinfo]"));
    assert!(output.contains("fixture 31 RGB payload"));
}

#[test]
fn fixture_31_reads_compressed_stealth_payload() {
    let bytes = stealth_png_variant(
        b"fixture 31 compressed payload",
        "stealth_pngcomp",
        8,
        false,
        true,
        None,
    );
    let png = TempPng::new(&bytes);
    let output = run_cli(&[png.path.to_str().unwrap()]);

    assert!(output.contains("Stealth PNG Info (Alpha) [stealth_pngcomp]"));
    assert!(output.contains("fixture 31 compressed payload"));
}

#[test]
fn stealth_prefix_match_continues_the_same_decoder_to_finish_the_image() {
    let bytes = stealth_png_variant_with_max_height(
        b"matched prefix must still validate the remaining stream",
        "stealth_pnginfo",
        8,
        false,
        false,
        None,
        240,
    );
    let png = TempPng::new(&corrupt_idat_zlib_checksum(bytes));
    let output = run_cli(&[png.path.to_str().unwrap()]);

    assert!(output.contains("(No metadata detected)"));
    assert!(!output.contains("matched prefix"));
}

#[test]
fn stealth_prefix_match_returns_metadata_after_stream_completion() {
    let bytes = stealth_png_variant_with_max_height(
        b"fixture after prefix completion",
        "stealth_pnginfo",
        8,
        false,
        false,
        None,
        240,
    );
    let png = TempPng::new(&bytes);
    let output = run_cli(&[png.path.to_str().unwrap()]);

    assert!(output.contains("fixture after prefix completion"));
}

#[test]
fn stealth_never_skips_the_stealth_scan() {
    let png = TempPng::new(&stealth_png(b"must be skipped", None));
    let output = run_cli(&["--stealth", "never", png.path.to_str().unwrap()]);

    assert!(output.contains("(No metadata detected)"));
    assert!(!output.contains("Stealth PNG Info"));
    assert!(!output.contains("must be skipped"));
}
