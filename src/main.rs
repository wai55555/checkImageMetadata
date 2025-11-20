use memmap2::MmapOptions;
use std::fs::File;
use std::str;
use serde_json::Value;

// 探す対象に NovelAI 用のキー (Description, Comment) を追加
const TARGET_KEYWORDS: [&str; 5] = [
    "parameters", // SD (A1111)
    "prompt",     // ComfyUI
    "workflow",   // ComfyUI
    "Description",// NovelAI (Prompt)
    "Comment"     // NovelAI (Settings JSON)
];

fn extract_png_metadata(mmap: &[u8], path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut cursor = 8;
    let len = mmap.len();

    println!("=== PNG File: {} ===", path);

    while cursor + 8 < len {
        let chunk_len = u32::from_be_bytes(mmap[cursor..cursor+4].try_into()?) as usize;
        let chunk_type = &mmap[cursor+4..cursor+8];
        let data_start = cursor + 8;
        let next_chunk = data_start + chunk_len + 4;

        if next_chunk > len { break; }

        // テキストチャンク (tEXt) の処理
        if chunk_type == b"tEXt" {
            let data = &mmap[data_start..data_start + chunk_len];
            if let Some(null_pos) = data.iter().position(|&b| b == 0) {
                if let Ok(keyword) = str::from_utf8(&data[0..null_pos]) {
                    if TARGET_KEYWORDS.contains(&keyword) {
                        let text = str::from_utf8(&data[null_pos+1..]).unwrap_or("");
                        print_metadata(keyword, text)?;
                    }
                }
            }
        }

        cursor = next_chunk;
    }
    Ok(())
}

fn extract_webp_metadata(mmap: &[u8], path: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("=== WebP File: {} ===", path);
    
    let mut cursor = 12; // Skip "RIFF" + size + "WEBP"
    let len = mmap.len();

    while cursor + 8 <= len {
        let chunk_type = &mmap[cursor..cursor+4];
        let chunk_size = u32::from_le_bytes(mmap[cursor+4..cursor+8].try_into()?) as usize;
        let data_start = cursor + 8;
        
        if data_start + chunk_size > len { break; }

        // EXIFチャンクを探す
        if chunk_type == b"EXIF" {
            let exif_data = &mmap[data_start..data_start + chunk_size];
            extract_from_exif(exif_data)?;
            return Ok(());
        }

        cursor = data_start + chunk_size;
        if chunk_size % 2 == 1 { cursor += 1; } // パディング
    }
    
    Ok(())
}

fn print_metadata(keyword: &str, text: &str) -> Result<(), Box<dyn std::error::Error>> {
    match keyword {
        "Description" => {
            println!("--- NovelAI [Prompt] ---");
            println!("{}", text);
        },
        "Comment" => {
            println!("--- NovelAI [Settings] ---");
            if let Ok(v) = serde_json::from_str::<Value>(text) {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("{}", text);
            }
        },
        "parameters" => {
            println!("--- Stable Diffusion (A1111) ---");
            println!("{}", text);
        },
        _ => {
            println!("--- {} ---", keyword);
            println!("{}", text);
        }
    }
    Ok(())
}

fn extract_from_exif(exif_data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    // "UNICODE\0" を探す（UserCommentの文字コード指定）
    if let Some(pos) = exif_data.windows(8).position(|w| w == b"UNICODE\0") {
        let data_start = pos + 8;
        if data_start < exif_data.len() {
            let remaining = &exif_data[data_start..];
            
            // UTF-16BEとして読み取り
            if remaining.len() >= 2 {
                let utf16_data: Vec<u16> = remaining
                    .chunks_exact(2)
                    .take(5000)
                    .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                    .take_while(|&c| c != 0)
                    .collect();
                
                if let Ok(text) = String::from_utf16(&utf16_data) {
                    println!("--- EXIF UserComment ---");
                    println!("{}", text);
                }
            }
        }
    }
    
    Ok(())
}

fn extract_exif_metadata(mmap: &[u8], path: &str, format: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("=== {} File: {} ===", format, path);
    
    // UserCommentタグ (0x9286) を直接探す
    // EXIFデータは通常ファイルの先頭付近にあるので、最初の64KBのみ検索
    let search_len = mmap.len().min(65536);
    
    for i in 0..search_len.saturating_sub(12) {
        // ビッグエンディアンでタグを探す: 92 86
        if mmap[i] == 0x92 && mmap[i+1] == 0x86 {
            // データ長を取得（オフセット+4から4バイト、ビッグエンディアン）
            let data_len = u32::from_be_bytes([mmap[i+4], mmap[i+5], mmap[i+6], mmap[i+7]]) as usize;
            
            // データオフセットを取得（通常はタグの後12バイト）
            let data_offset = i + 12;
            
            if data_offset + data_len <= mmap.len() {
                let user_comment_data = &mmap[data_offset..data_offset + data_len];
                
                // "UNICODE\0" を探す
                if let Some(unicode_pos) = user_comment_data.windows(8).position(|w| w == b"UNICODE\0") {
                    let text_start = unicode_pos + 8;
                    let text_data = &user_comment_data[text_start..];
                    
                    // UTF-16BEとしてデコード
                    let utf16_data: Vec<u16> = text_data
                        .chunks_exact(2)
                        .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                        .take_while(|&c| c != 0)
                        .collect();
                    
                    if let Ok(text) = String::from_utf16(&utf16_data) {
                        println!("--- EXIF UserComment ---");
                        println!("{}", text);
                    }
                }
            }
            break;
        }
    }
    
    Ok(())
}

fn extract_universal_metadata(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let mmap = unsafe { MmapOptions::new().map(&file)? };

    // PNG判定
    if mmap.len() >= 8 && &mmap[0..8] == &[137, 80, 78, 71, 13, 10, 26, 10] {
        return extract_png_metadata(&mmap, path);
    }
    
    // WebP判定 (RIFF....WEBP)
    if mmap.len() >= 12 && &mmap[0..4] == b"RIFF" && &mmap[8..12] == b"WEBP" {
        return extract_webp_metadata(&mmap, path);
    }
    
    // JPEG判定 (FF D8 FF)
    if mmap.len() >= 3 && &mmap[0..3] == &[0xFF, 0xD8, 0xFF] {
        return extract_exif_metadata(&mmap, path, "JPEG");
    }
    
    // AVIF判定 (....ftypavif)
    if mmap.len() >= 12 && &mmap[4..8] == b"ftyp" {
        let brand = &mmap[8..12];
        if brand == b"avif" || brand == b"avis" {
            return extract_exif_metadata(&mmap, path, "AVIF");
        }
    }

    eprintln!("Unsupported file format");
    Ok(())

}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    
    if args.len() < 2 {
        eprintln!("Usage: {} <image_file.png>", args[0]);
        std::process::exit(1);
    }
    
    let path = &args[1];
    if let Err(e) = extract_universal_metadata(path) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}