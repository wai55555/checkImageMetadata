use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FILE_ID: AtomicUsize = AtomicUsize::new(0);

fn tiff_with_comment(comment: &[u8]) -> Vec<u8> {
    let mut tiff = b"II*\0\x08\0\0\0".to_vec();
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x9286u16.to_le_bytes());
    tiff.extend_from_slice(&7u16.to_le_bytes());
    tiff.extend_from_slice(&(comment.len() as u32).to_le_bytes());
    if comment.len() <= 4 {
        let mut inline = [0; 4];
        inline[..comment.len()].copy_from_slice(comment);
        tiff.extend_from_slice(&inline);
    } else {
        tiff.extend_from_slice(&26u32.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(comment);
        return tiff;
    }
    tiff.extend_from_slice(&0u32.to_le_bytes());
    tiff
}

fn webp_with_chunks(chunks: &[(&[u8; 4], &[u8])], riff_size: Option<u32>) -> Vec<u8> {
    let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
    for (kind, data) in chunks {
        bytes.extend_from_slice(*kind);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        if data.len() % 2 == 1 {
            bytes.push(0);
        }
    }
    let size = riff_size.unwrap_or((bytes.len() - 8) as u32);
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    bytes
}

fn run_webp(bytes: &[u8]) -> String {
    let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let path: PathBuf = std::env::temp_dir().join(format!(
        "fast_meta_phase2_{}_{}.webp",
        std::process::id(),
        id
    ));
    fs::write(&path, bytes).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fast_meta"))
        .arg(&path)
        .output()
        .unwrap();
    let _ = fs::remove_file(&path);
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn fixture_21_extracts_webp_exif_tiff_user_comment() {
    let tiff = tiff_with_comment(b"ASCII\0\0\0parameters=webp");
    let bytes = webp_with_chunks(&[(b"EXIF", &tiff)], None);
    let output = run_webp(&bytes);
    assert!(output.contains("--- EXIF UserComment ---"));
    assert!(output.contains("parameters=webp"));
    assert!(!output.contains("(No metadata detected)"));
}

#[test]
fn fixture_22_webp_exif_without_display_tags_reports_no_metadata() {
    let tiff = b"II*\0\x08\0\0\0\0\0\0\0\0\0";
    let bytes = webp_with_chunks(&[(b"EXIF", tiff)], None);
    let output = run_webp(&bytes);
    assert!(output.contains("(No metadata detected)"));
}

#[test]
fn fixture_24_formats_novelai_json_from_webp_user_comment() {
    let json = r#"{"prompt":"cat","negative":"blur"}"#;
    let mut comment = b"UNICODE\0".to_vec();
    for unit in json.encode_utf16() {
        comment.extend_from_slice(&unit.to_le_bytes());
    }
    let tiff = tiff_with_comment(&comment);
    let bytes = webp_with_chunks(&[(b"EXIF", &tiff)], None);
    let output = run_webp(&bytes);
    assert!(output.contains("{\n  \"negative\": \"blur\",\n  \"prompt\": \"cat\"\n}"));
}

#[test]
fn fixture_37_reads_webp_exif_past_a_zero_riff_size_field() {
    let tiff = tiff_with_comment(b"ASCII\0\0\0parameters=past-zero-riff-size");
    let bytes = webp_with_chunks(&[(b"EXIF", &tiff)], Some(0));
    let output = run_webp(&bytes);
    assert!(output.contains("parameters=past-zero-riff-size"));
    assert!(!output.contains("(No metadata detected)"));
}
