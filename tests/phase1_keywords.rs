use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

struct TempFixture {
    dir: PathBuf,
    path: PathBuf,
}

impl TempFixture {
    fn new(bytes: &[u8]) -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("fast-meta-phase1-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fixture.png");
        fs::write(&path, bytes).unwrap();
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

fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(data.len() + 12);
    result.extend_from_slice(&(data.len() as u32).to_be_bytes());
    result.extend_from_slice(kind);
    result.extend_from_slice(data);
    result.extend_from_slice(&crc32(&result[4..]).to_be_bytes());
    result
}

fn png_with_text(keyword: &str, text: &str) -> Vec<u8> {
    let mut png = PNG_SIGNATURE.to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&1u32.to_be_bytes());
    ihdr.extend_from_slice(&1u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend_from_slice(&chunk(b"IHDR", &ihdr));

    let mut text_data = keyword.as_bytes().to_vec();
    text_data.push(0);
    text_data.extend_from_slice(text.as_bytes());
    png.extend_from_slice(&chunk(b"tEXt", &text_data));

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&[0, 0, 0, 0, 0]).unwrap();
    png.extend_from_slice(&chunk(b"IDAT", &encoder.finish().unwrap()));
    png.extend_from_slice(&chunk(b"IEND", &[]));
    png
}

fn assert_cli_output(keyword: &str, text: &str, expected_label: &str) {
    let fixture = TempFixture::new(&png_with_text(keyword, text));
    let output = Command::new(env!("CARGO_BIN_EXE_fast_meta"))
        .arg(&fixture.path)
        .output()
        .expect("run fast_meta");
    assert!(
        output.status.success(),
        "fast_meta failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stdout = stdout.replace(fixture.path.to_str().unwrap(), "<fixture>");
    assert_eq!(
        stdout,
        format!("=== PNG File: <fixture> ===\n--- {expected_label} ---\n{text}\n")
    );
}

fn png_with_text_between_idats() -> Vec<u8> {
    let mut png = PNG_SIGNATURE.to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&1u32.to_be_bytes());
    ihdr.extend_from_slice(&1u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend_from_slice(&chunk(b"IHDR", &ihdr));
    png.extend_from_slice(&chunk(b"IDAT", &[1]));
    let text = b"parameters\0between IDAT chunks";
    png.extend_from_slice(&chunk(b"tEXt", text));
    png.extend_from_slice(&chunk(b"IDAT", &[2]));
    png.extend_from_slice(&chunk(b"IEND", &[]));
    png
}

#[test]
fn fixture_9_swarmui_parameters_are_displayed() {
    assert_cli_output(
        "sui_image_params",
        r#"{"steps":20,"seed":123}"#,
        "SwarmUI [Params]",
    );
}

#[test]
fn fixture_10_invokeai_metadata_is_displayed() {
    assert_cli_output(
        "invokeai_metadata",
        r#"{"steps":30,"model":"test"}"#,
        "InvokeAI",
    );
}

#[test]
fn fixture_26_only_strict_mode_reports_text_between_idats() {
    let fixture = TempFixture::new(&png_with_text_between_idats());
    let default = Command::new(env!("CARGO_BIN_EXE_fast_meta"))
        .arg(&fixture.path)
        .output()
        .expect("run default scan");
    assert!(default.status.success());
    let default_stdout = String::from_utf8(default.stdout).unwrap();
    assert!(default_stdout.contains("(No metadata detected)"));
    assert!(!default_stdout.contains("between IDAT chunks"));

    let strict = Command::new(env!("CARGO_BIN_EXE_fast_meta"))
        .arg("--strict")
        .arg(&fixture.path)
        .output()
        .expect("run strict scan");
    assert!(strict.status.success());
    let strict_stdout = String::from_utf8(strict.stdout).unwrap();
    assert!(strict_stdout.contains("--- Stable Diffusion (A1111) ---"));
    assert!(strict_stdout.contains("between IDAT chunks"));
}

#[test]
fn phase1_rejects_flags_that_are_not_yet_in_the_specified_phase() {
    let fixture = TempFixture::new(&png_with_text("parameters", "unused"));
    let output = Command::new(env!("CARGO_BIN_EXE_fast_meta"))
        .arg("--format")
        .arg(&fixture.path)
        .output()
        .expect("run fast_meta");
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Usage:"));
}
