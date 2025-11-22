use memmap2::MmapOptions;
use std::fs::File;
use std::str;
use serde_json::Value;

// 探す対象に NovelAI 用のキー (Description, Comment) を追加
const TARGET_KEYWORDS: [&str; 6] = [
    "parameters",      // SD (A1111)
    "prompt",          // ComfyUI
    "workflow",        // ComfyUI
    "generation_data", // ComfyUI (rare case)
    "Description",     // NovelAI (Prompt)
    "Comment"          // NovelAI (Settings JSON)
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
        "generation_data" => {
            println!("--- ComfyUI [Generation Data] ---");
            if let Ok(v) = serde_json::from_str::<Value>(text) {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("{}", text);
            }
        },
        _ => {
            println!("--- {} ---", keyword);
            println!("{}", text);
        }
    }
    Ok(())
}

fn extract_from_exif(exif_data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    // UserCommentは最初の8バイトが文字コード識別子（ただし先頭に余分なバイトがある場合がある）
    let (charset, text_data) = if exif_data.len() >= 12 && &exif_data[0..4] == b"\0\0\0\0" {
        // 先頭4バイトが\0\0\0\0の場合、オフセット4から8バイトが文字コード識別子
        (&exif_data[4..12], &exif_data[12..])
    } else if exif_data.len() >= 8 {
        // 標準的な位置
        (&exif_data[0..8], &exif_data[8..])
    } else {
        return Ok(());
    };
        
        // 文字コードに応じてデコード
        if charset == b"UNICODE\0" {
            // UTF-16としてデコード（BOMまたは最初の文字でエンディアン判定）
            if text_data.len() >= 2 {
                let is_le = if text_data.len() >= 2 {
                    let first = u16::from_le_bytes([text_data[0], text_data[1]]);
                    let first_be = u16::from_be_bytes([text_data[0], text_data[1]]);
                    // BOMチェック
                    if first == 0xFEFF { true }
                    else if first_be == 0xFEFF { false }
                    // ASCII範囲の文字かチェック
                    else { first >= 0x0020 && first <= 0x007E }
                } else {
                    false
                };
                
                let utf16_data: Vec<u16> = text_data
                    .chunks_exact(2)
                    .take(5000)
                    .map(|chunk| {
                        if is_le {
                            u16::from_le_bytes([chunk[0], chunk[1]])
                        } else {
                            u16::from_be_bytes([chunk[0], chunk[1]])
                        }
                    })
                    .take_while(|&c| c != 0 && c != 0xFEFF)
                    .collect();
                
                if let Ok(text) = String::from_utf16(&utf16_data) {
                    println!("--- EXIF UserComment ---");
                    println!("{}", text);
                }
            }
        } else if charset == b"ASCII\0\0\0" {
            // ASCII/UTF-8としてデコード
            if let Ok(text) = str::from_utf8(text_data) {
                let trimmed = text.trim_end_matches('\0');
                println!("--- EXIF UserComment ---");
                println!("{}", trimmed);
            }
        } else if charset == b"JIS\0\0\0\0\0" {
            // JIS (ISO-2022-JP)
            if let Ok(text) = str::from_utf8(text_data) {
                let trimmed = text.trim_end_matches('\0');
                println!("--- EXIF UserComment ---");
                println!("{}", trimmed);
            }
        } else if charset == b"\0\0\0\0\0\0\0\0" {
            // 未定義の場合、ASCII/UTF-8として試す
            if let Ok(text) = str::from_utf8(text_data) {
                let trimmed = text.trim_end_matches('\0');
                if !trimmed.is_empty() {
                    println!("--- EXIF UserComment ---");
                    println!("{}", trimmed);
                }
            }
        }
    
    Ok(())
}

fn extract_exif_metadata(mmap: &[u8], path: &str, format: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("=== {} File: {} ===", format, path);
    
    // TIFFヘッダーを探す（Exifマーカーの後）
    let search_len = mmap.len().min(65536);
    let mut tiff_start = 0;
    
    // "Exif\0\0" を探してTIFFヘッダーの位置を特定
    for i in 0..search_len.saturating_sub(10) {
        if &mmap[i..i+6] == b"Exif\0\0" {
            tiff_start = i + 6;
            break;
        }
    }
    
    if tiff_start == 0 {
        return Ok(());
    }
    
    // UserCommentタグ (0x9286) を探す
    for i in 0..search_len.saturating_sub(12) {
        if mmap[i] == 0x92 && mmap[i+1] == 0x86 {
            // データ長を取得
            let data_len = u32::from_be_bytes([mmap[i+4], mmap[i+5], mmap[i+6], mmap[i+7]]) as usize;
            
            // データオフセットを取得（TIFFヘッダーからの相対オフセット）
            let offset_value = u32::from_be_bytes([mmap[i+8], mmap[i+9], mmap[i+10], mmap[i+11]]) as usize;
            
            // 絶対オフセットを計算
            let data_offset = tiff_start + offset_value;
            
            // eprintln!("UserComment tag at {}, data_len={}, offset_value={}, tiff_start={}, data_offset={}", i, data_len, offset_value, tiff_start, data_offset);
            
            if data_offset + data_len <= mmap.len() && data_len >= 8 {
                let user_comment_data = &mmap[data_offset..data_offset + data_len];
                
                // eprintln!("First 20 bytes: {:02X?}", &user_comment_data[..20.min(user_comment_data.len())]);
                
                let (charset, text_data) = if user_comment_data.len() >= 12 && &user_comment_data[0..4] == b"\0\0\0\0" {
                    (&user_comment_data[4..12], &user_comment_data[12..])
                } else if user_comment_data.len() >= 8 {
                    (&user_comment_data[0..8], &user_comment_data[8..])
                } else {
                    break;
                };
                
                // 文字コードに応じてデコード
                if charset == b"UNICODE\0" {
                    // UTF-16としてデコード（BOMまたは最初の文字でエンディアン判定）
                    if text_data.len() >= 2 {
                        let is_le = if text_data.len() >= 2 {
                            // 最初の2バイトをチェック（BOMまたは最初の文字）
                            let first = u16::from_le_bytes([text_data[0], text_data[1]]);
                            let first_be = u16::from_be_bytes([text_data[0], text_data[1]]);
                            // BOMチェック
                            if first == 0xFEFF { true }
                            else if first_be == 0xFEFF { false }
                            // ASCII範囲の文字かチェック（LE: 0x0020-0x007E, BE: 0x2000-0x7E00）
                            else { first >= 0x0020 && first <= 0x007E }
                        } else {
                            false
                        };
                        
                        let utf16_data: Vec<u16> = text_data
                            .chunks_exact(2)
                            .map(|chunk| {
                                if is_le {
                                    u16::from_le_bytes([chunk[0], chunk[1]])
                                } else {
                                    u16::from_be_bytes([chunk[0], chunk[1]])
                                }
                            })
                            .take_while(|&c| c != 0 && c != 0xFEFF)
                            .collect();
                        
                        if let Ok(text) = String::from_utf16(&utf16_data) {
                            println!("--- EXIF UserComment ---");
                            println!("{}", text);
                        }
                    }
                } else if charset == b"ASCII\0\0\0" {
                    // ASCII/UTF-8としてデコード
                    if let Ok(text) = str::from_utf8(text_data) {
                        let trimmed = text.trim_end_matches('\0');
                        println!("--- EXIF UserComment ---");
                        println!("{}", trimmed);
                    }
                } else if charset == b"JIS\0\0\0\0\0" {
                    // JIS (ISO-2022-JP) - 基本的にはASCII互換として扱う
                    if let Ok(text) = str::from_utf8(text_data) {
                        let trimmed = text.trim_end_matches('\0');
                        println!("--- EXIF UserComment ---");
                        println!("{}", trimmed);
                    }
                } else if charset == b"\0\0\0\0\0\0\0\0" {
                    // 未定義の場合、ASCII/UTF-8として試す
                    if let Ok(text) = str::from_utf8(text_data) {
                        let trimmed = text.trim_end_matches('\0');
                        if !trimmed.is_empty() {
                            println!("--- EXIF UserComment ---");
                            println!("{}", trimmed);
                        }
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