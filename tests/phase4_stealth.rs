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

fn stealth_png(payload: &[u8], text_chunk: Option<Vec<u8>>) -> Vec<u8> {
    let mut stream = b"stealth_pnginfo".to_vec();
    stream.extend_from_slice(&((payload.len() * 8) as u32).to_be_bytes());
    stream.extend_from_slice(payload);

    // Alpha bits are read down the first column, one reconstructed row per bit.
    let height = stream.len() * 8;
    let mut raw = Vec::with_capacity(height * 5);
    for bit_index in 0..height {
        let bit = (stream[bit_index / 8] >> (7 - bit_index % 8)) & 1;
        raw.extend_from_slice(&[0, 0, 0, 0, bit]);
    }

    let mut png = PNG_SIGNATURE.to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&1u32.to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
    if let Some(chunk) = text_chunk {
        png.extend_from_slice(&chunk);
    }
    png.extend_from_slice(&png_chunk(b"IDAT", &compress(&raw)));
    png.extend_from_slice(&png_chunk(b"IEND", &[]));
    png
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
fn stealth_never_skips_the_stealth_scan() {
    let png = TempPng::new(&stealth_png(b"must be skipped", None));
    let output = run_cli(&["--stealth", "never", png.path.to_str().unwrap()]);

    assert!(output.contains("(No metadata detected)"));
    assert!(!output.contains("Stealth PNG Info"));
    assert!(!output.contains("must be skipped"));
}
