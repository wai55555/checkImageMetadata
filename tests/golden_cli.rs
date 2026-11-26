use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct TempFixture {
    dir: PathBuf,
    path: PathBuf,
}

impl TempFixture {
    fn new(extension: &str, bytes: &[u8]) -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("fast-meta-phase0-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temporary fixture directory");
        let path = dir.join(format!("fixture.{extension}"));
        fs::write(&path, bytes).expect("write temporary fixture");
        Self { dir, path }
    }
}

impl Drop for TempFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 == 1 { 0xedb8_8320 } else { 0 };
        }
    }
    !crc
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(data.len() + 12);
    chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(data);
    chunk.extend_from_slice(&crc32(&chunk[4..]).to_be_bytes());
    chunk
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(data)
        .expect("compress fixture image data");
    encoder.finish().expect("finish fixture image data")
}

fn ihdr(width: u32, height: u32, color_type: u8) -> Vec<u8> {
    let mut data = Vec::with_capacity(13);
    data.extend_from_slice(&width.to_be_bytes());
    data.extend_from_slice(&height.to_be_bytes());
    data.extend_from_slice(&[8, color_type, 0, 0, 0]);
    png_chunk(b"IHDR", &data)
}

fn simple_png(metadata_before_idat: Vec<u8>) -> Vec<u8> {
    let mut png = PNG_SIGNATURE.to_vec();
    png.extend_from_slice(&ihdr(1, 1, 6));
    png.extend_from_slice(&metadata_before_idat);
    png.extend_from_slice(&png_chunk(b"IDAT", &zlib(&[0, 0, 0, 0, 0])));
    png.extend_from_slice(&png_chunk(b"IEND", &[]));
    png
}

fn text_chunk(keyword: &str, text: &str) -> Vec<u8> {
    let mut data = keyword.as_bytes().to_vec();
    data.push(0);
    data.extend_from_slice(text.as_bytes());
    png_chunk(b"tEXt", &data)
}

fn comfy_png_with_tail_workflow() -> Vec<u8> {
    let (width, height) = (256u32, 256u32);
    let mut raw = Vec::with_capacity(height as usize * (1 + width as usize * 3));
    let mut state = 0x1234_5678u32;
    for _ in 0..height {
        raw.push(0); // PNG filter: None
        for _ in 0..(width * 3) {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            raw.push((state >> 24) as u8);
        }
    }
    let compressed = zlib(&raw);
    assert!(compressed.len() > 65_536);

    let mut png = PNG_SIGNATURE.to_vec();
    png.extend_from_slice(&ihdr(width, height, 2));
    // The v1.2.0 ComfyUI fingerprint is an initial IDAT of exactly 64 KiB.
    png.extend_from_slice(&png_chunk(b"IDAT", &compressed[..65_536]));
    png.extend_from_slice(&png_chunk(b"IDAT", &compressed[65_536..]));
    png.extend_from_slice(&text_chunk("workflow", "{\"nodes\":[]}"));
    png.extend_from_slice(&png_chunk(b"IEND", &[]));
    png
}

fn jpeg_with_user_comment(text: &str) -> Vec<u8> {
    let mut user_comment = b"ASCII\0\0\0".to_vec();
    user_comment.extend_from_slice(text.as_bytes());

    let mut tiff = b"II*\0".to_vec();
    tiff.extend_from_slice(&8u32.to_le_bytes()); // first IFD
    tiff.extend_from_slice(&1u16.to_le_bytes()); // one IFD entry
    tiff.extend_from_slice(&[0x86, 0x92]); // UserComment (0x9286), little endian
    tiff.extend_from_slice(&7u16.to_le_bytes()); // UNDEFINED
    tiff.extend_from_slice(&(user_comment.len() as u32).to_le_bytes());
    tiff.extend_from_slice(&26u32.to_le_bytes()); // payload offset from TIFF header
    tiff.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    tiff.extend_from_slice(&user_comment);

    let mut app1_data = b"Exif\0\0".to_vec();
    app1_data.extend_from_slice(&tiff);

    let mut jpeg = vec![0xff, 0xd8]; // SOI
    jpeg.extend_from_slice(&jpeg_segment(0xe1, &app1_data)); // EXIF APP1

    // A minimal 8x8 baseline grayscale JPEG image (all-zero DCT block).
    let mut dqt = vec![0]; // 8-bit quantization table 0
    dqt.extend_from_slice(&[1; 64]);
    jpeg.extend_from_slice(&jpeg_segment(0xdb, &dqt));
    jpeg.extend_from_slice(&jpeg_segment(0xc0, &[8, 0, 8, 0, 8, 1, 1, 0x11, 0]));

    let mut dht = Vec::new();
    for table_class in [0, 0x10] {
        dht.push(table_class);
        dht.push(1); // one one-bit code: DC category 0 / AC EOB
        dht.extend_from_slice(&[0; 15]);
        dht.push(0);
    }
    jpeg.extend_from_slice(&jpeg_segment(0xc4, &dht));
    jpeg.extend_from_slice(&jpeg_segment(0xda, &[1, 1, 0, 0, 63, 0]));
    jpeg.push(0x3f); // DC=0, EOB, padded with one bits
    jpeg.extend_from_slice(&[0xff, 0xd9]); // EOI
    jpeg
}

fn jpeg_segment(marker: u8, data: &[u8]) -> Vec<u8> {
    let mut segment = vec![0xff, marker];
    segment.extend_from_slice(&((data.len() + 2) as u16).to_be_bytes());
    segment.extend_from_slice(data);
    segment
}

fn webp_with_xmp_description(description: &str) -> Vec<u8> {
    let xmp = format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:description>{description}</dc:description></rdf:Description></rdf:RDF></x:xmpmeta>"#
    );
    let mut riff_body = b"WEBP".to_vec();
    // VP8X declares XMP metadata; VP8L contains a valid 2x2 lossless image.
    riff_body.extend_from_slice(&webp_chunk(b"VP8X", &[0x04, 0, 0, 0, 1, 0, 0, 1, 0, 0]));
    riff_body.extend_from_slice(&webp_chunk(
        b"VP8L",
        &[
            0x2f, 0x01, 0x40, 0x00, 0x00, 0x07, 0x10, 0xf5, 0x8f, 0xfe, 0x07, 0x22, 0xa2, 0xff,
            0x01,
        ],
    ));
    riff_body.extend_from_slice(&webp_chunk(b"XMP ", xmp.as_bytes()));
    let mut webp = b"RIFF".to_vec();
    webp.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
    webp.extend_from_slice(&riff_body);
    webp
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

fn assert_cli_golden(extension: &str, bytes: &[u8], expected: &str) {
    let fixture = TempFixture::new(extension, bytes);
    let output = Command::new(env!("CARGO_BIN_EXE_fast_meta"))
        .arg(&fixture.path)
        .output()
        .expect("run fast_meta");
    assert!(
        output.status.success(),
        "fast_meta failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("CLI output is UTF-8");
    let normalized = stdout.replace(fixture.path.to_str().unwrap(), "<fixture>");
    assert_eq!(normalized, expected);
}

#[test]
fn golden_a1111_png_parameters_at_start() {
    let png = simple_png(text_chunk(
        "parameters",
        "positive prompt\nNegative prompt: low quality\nSteps: 20, Seed: 123",
    ));
    assert_cli_golden(
        "png",
        &png,
        "=== PNG File: <fixture> ===\n--- Stable Diffusion (A1111) ---\npositive prompt\nNegative prompt: low quality\nSteps: 20, Seed: 123\n",
    );
}

#[test]
fn golden_png_records_keep_file_order_through_buffered_stdout() {
    let mut metadata = text_chunk("Title", "first");
    metadata.extend_from_slice(&text_chunk("parameters", "second"));
    let png = simple_png(metadata);

    assert_cli_golden(
        "png",
        &png,
        "=== PNG File: <fixture> ===\n--- Title ---\nfirst\n--- Stable Diffusion (A1111) ---\nsecond\n",
    );
}

#[test]
fn golden_comfyui_png_workflow_after_idat() {
    let png = comfy_png_with_tail_workflow();
    assert_cli_golden(
        "png",
        &png,
        "=== PNG File: <fixture> ===\n(Large file or ComfyUI detected. Scanning tail for metadata...)\n--- workflow ---\n{\"nodes\":[]}\n",
    );
}

#[test]
fn golden_novelai_png_comment_json_formatting() {
    let png = simple_png(text_chunk("Comment", r#"{"steps":20,"seed":1}"#));
    assert_cli_golden(
        "png",
        &png,
        "=== PNG File: <fixture> ===\n--- NovelAI [Settings] ---\n{\n  \"seed\": 1,\n  \"steps\": 20\n}\n",
    );
}

#[test]
fn golden_jpeg_exif_user_comment() {
    let jpeg = jpeg_with_user_comment("parameters=golden JPEG metadata");
    assert_cli_golden(
        "jpg",
        &jpeg,
        "=== JPEG File: <fixture> ===\n--- EXIF UserComment ---\nparameters=golden JPEG metadata\n",
    );
}

#[test]
fn golden_webp_xmp_description_element() {
    let webp = webp_with_xmp_description("a WebP prompt");
    assert_cli_golden(
        "webp",
        &webp,
        "=== WebP File: <fixture> ===\n--- Midjourney (XMP) ---\na WebP prompt\n",
    );
}

#[test]
fn golden_unsupported_format_reports_error_and_exits_zero() {
    let fixture = TempFixture::new("bin", b"not an image");
    let output = Command::new(env!("CARGO_BIN_EXE_fast_meta"))
        .arg(&fixture.path)
        .output()
        .expect("run fast_meta");
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).expect("CLI stderr is UTF-8"),
        "Unsupported file format\n"
    );
}
