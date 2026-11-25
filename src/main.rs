pub mod output;
pub mod record;

use memmap2::MmapOptions;
use std::fs::File;
use std::io::{self, Read};
use std::str;
use serde_json::Value;
use flate2::read::{GzDecoder, ZlibDecoder};

// 探す対象のキーワードリスト
const TARGET_KEYWORDS: [&str; 6] = [
    "parameters",      // SD (A1111)
    "prompt",          // ComfyUI
    "workflow",        // ComfyUI
    "generation_data", // ComfyUI (rare case)
    "Description",     // NovelAI (Prompt)
    "Comment"          // NovelAI (Settings JSON)
];

// 最初のスキャン上限 (64KB)
const FAST_SCAN_LIMIT: usize = 65536;

fn extract_png_metadata(mmap: &[u8], path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let len = mmap.len();
    let mut cursor = 8;
    let mut found_metadata = false;
    let mut is_comfy_style = false;
    let mut is_first_chunk_after_ihdr = true;

    println!("=== PNG File: {} ===", path);

    // 最初に FAST_SCAN_LIMIT (64KB) まで走査
    let scan_limit = mmap.len().min(FAST_SCAN_LIMIT);
    
    while cursor + 8 < scan_limit {
        let chunk_len = u32::from_be_bytes(mmap[cursor..cursor+4].try_into()?) as usize;
        let chunk_type = &mmap[cursor+4..cursor+8];
        let data_start = cursor + 8;
        let next_chunk = data_start + chunk_len + 4;

        // 次のチャンクが 64KB を超える場合でも、iTXt/tEXt なら読みに行く必要がある
        // それ以外の巨大データ (IDAT等) は飛ばす
        
        // ComfyUI 指紋判定: IHDRの直後のチャンクが IDAT で 65536バイト
        if chunk_type != b"IHDR" {
            if is_first_chunk_after_ihdr {
                if chunk_type == b"IDAT" && chunk_len == 65536 {
                    is_comfy_style = true;
                }
                is_first_chunk_after_ihdr = false;
            }
        }

        if chunk_type == b"tEXt" || chunk_type == b"iTXt" {
            // チャンクデータがファイル全体の範囲内か確認
            if next_chunk <= len {
                let data = &mmap[data_start..data_start + chunk_len];
                if parse_png_chunk(chunk_type, data)? {
                    found_metadata = true;
                }
            }
        }

        cursor = next_chunk;
        if chunk_type == b"IEND" { break; }
    }

    // 64KBで見つからず、ComfyUIスタイル（末尾にデータがある可能性）の場合は末尾をスキャン
    if (!found_metadata || is_comfy_style) && len > FAST_SCAN_LIMIT {
        // 末尾 128KB をスキャン
        let tail_size = 131072.min(len - FAST_SCAN_LIMIT);
        let tail_start = len - tail_size;
        
        println!("(Large file or ComfyUI detected. Scanning tail for metadata...)");
        
        // 末尾から tEXt / iTXt を探す (簡易スキャン)
        let mut tail_cursor = tail_start;
        while tail_cursor + 8 < len {
            // "tEXt" または "iTXt" という文字列をバイト列から探す
            if &mmap[tail_cursor+4..tail_cursor+8] == b"tEXt" || &mmap[tail_cursor+4..tail_cursor+8] == b"iTXt" {
                let chunk_len = u32::from_be_bytes(mmap[tail_cursor..tail_cursor+4].try_into()?) as usize;
                let chunk_type = &mmap[tail_cursor+4..tail_cursor+8];
                let data_start = tail_cursor + 8;
                if data_start + chunk_len <= len {
                    let data = &mmap[data_start..data_start + chunk_len];
                    if parse_png_chunk(chunk_type, data)? {
                        found_metadata = true;
                    }
                }
            }
            tail_cursor += 1; // 1バイトずつずらして検索 (効率は落ちるが確実)
        }
    }

    // 通常のPNGメタデータに加えて Stealth PNG も走査する
    if !found_metadata {
        if let Some((mode, signature, stealth_meta)) = check_stealth_png(mmap) {
            println!("--- Stealth PNG Info ({}) [{}] ---", mode, signature);
            println!("{}", stealth_meta);
            found_metadata = true;
        }
    }

    if !found_metadata {
        println!("(No metadata detected)");
    }

    Ok(())
}

fn parse_png_chunk(chunk_type: &[u8], data: &[u8]) -> Result<bool, Box<dyn std::error::Error>> {
    let mut meta_found = false;
    if chunk_type == b"tEXt" {
        if let Some(null_pos) = data.iter().position(|&b| b == 0) {
            if let Ok(keyword) = str::from_utf8(&data[0..null_pos]) {
                if TARGET_KEYWORDS.contains(&keyword) {
                    let text = str::from_utf8(&data[null_pos+1..]).unwrap_or("");
                    print_metadata(keyword, text)?;
                    meta_found = true;
                }
            }
        }
    } else if chunk_type == b"iTXt" {
        if let Some(null_pos) = data.iter().position(|&b| b == 0) {
            if let Ok(keyword) = str::from_utf8(&data[0..null_pos]) {
                if TARGET_KEYWORDS.contains(&keyword) {
                    if null_pos + 2 < data.len() {
                        let comp_flag = data[null_pos + 1];
                        // ヌル終端の lang_tag と trans_key を飛ばす
                        let rest = &data[null_pos + 3..];
                        if let Some(lang_end) = rest.iter().position(|&b| b == 0) {
                            let rest2 = &rest[lang_end + 1..];
                            if let Some(trans_end) = rest2.iter().position(|&b| b == 0) {
                                let text_bytes = &rest2[trans_end + 1..];
                                
                                if comp_flag == 1 {
                                    // 圧縮データの解凍
                                    let mut decoder = ZlibDecoder::new(text_bytes);
                                    let mut decoded_text = String::new();
                                    if decoder.read_to_string(&mut decoded_text).is_ok() {
                                        print_metadata(keyword, &decoded_text)?;
                                        meta_found = true;
                                    }
                                } else {
                                    // 非圧縮
                                    if let Ok(text) = str::from_utf8(text_bytes) {
                                        print_metadata(keyword, text)?;
                                        meta_found = true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(meta_found)
}

const MAX_STEALTH_DIMENSION: usize = 16_384;
const MAX_STEALTH_DECODED_PNG_BYTES: usize = 512 * 1024 * 1024;
const MAX_STEALTH_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_STEALTH_IDAT_BYTES: usize = 512 * 1024 * 1024;
const STEALTH_HEADER_BITS: usize = ("stealth_pnginfo".len() * 8) + 32;
const STEALTH_SIGNATURE_BYTES: usize = "stealth_pnginfo".len();

struct PngIdatReader<'a> {
    chunks: Vec<&'a [u8]>,
    chunk_index: usize,
    chunk_offset: usize,
}

impl<'a> PngIdatReader<'a> {
    fn new(chunks: Vec<&'a [u8]>) -> Self {
        Self { chunks, chunk_index: 0, chunk_offset: 0 }
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
    if width == 0 || height == 0 || width > MAX_STEALTH_DIMENSION ||
        height > MAX_STEALTH_DIMENSION || ihdr[10] != 0 || ihdr[11] != 0 || ihdr[12] != 0 ||
        !matches!((color_type, bit_depth), (2 | 4 | 6, 8 | 16)) {
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
    let decoded_size = row_bytes.checked_add(1)?.checked_mul(height)?;
    if decoded_size > MAX_STEALTH_DECODED_PNG_BYTES {
        return None;
    }

    let mut decoded = Vec::with_capacity(decoded_size);
    let mut decoder = ZlibDecoder::new(PngIdatReader::new(idat_chunks))
        .take(decoded_size as u64 + 1);
    decoder.read_to_end(&mut decoded).ok()?;
    if decoded.len() != decoded_size {
        return None;
    }

    // PNGフィルターを行単位で解除する。各行をインプレースで復元するため、
    // 画像全体の追加コピーを作らない。
    for y in 0..height {
        let row_start = y.checked_mul(row_bytes.checked_add(1)?)?;
        let previous_start = if y == 0 { None } else { Some(row_start - row_bytes) };
        let filter = *decoded.get(row_start)?;
        if filter > 4 {
            return None;
        }
        for x in 0..row_bytes {
            let index = row_start + 1 + x;
            let raw = decoded[index];
            let left = if x >= bytes_per_pixel { decoded[index - bytes_per_pixel] } else { 0 };
            let above = previous_start.map(|start| decoded[start + x]).unwrap_or(0);
            let upper_left = if x >= bytes_per_pixel {
                previous_start.map(|start| decoded[start + x - bytes_per_pixel]).unwrap_or(0)
            } else {
                0
            };
            decoded[index] = match filter {
                0 => raw,
                1 => raw.wrapping_add(left),
                2 => raw.wrapping_add(above),
                3 => raw.wrapping_add(((left as u16 + above as u16) / 2) as u8),
                4 => raw.wrapping_add(paeth_predictor(left, above, upper_left)),
                _ => unreachable!(),
            };
        }
    }

    let total_pixels = width.checked_mul(height)?;
    for mode in ["Alpha", "RGB"] {
        let mode_channels = match (mode, color_type) {
            ("Alpha", 4) => vec![1usize],
            ("Alpha", 6) => vec![3usize],
            ("RGB", 2 | 6) => vec![0usize, 1usize, 2usize],
            _ => continue,
        };
        let channel_bits = total_pixels.checked_mul(mode_channels.len())?;
        if channel_bits < STEALTH_HEADER_BITS {
            continue;
        }

        let read_stream_byte = |byte_index: usize| -> Option<u8> {
            let mut result = 0u8;
            for bit_in_byte in 0..8 {
                let bit_index = byte_index.checked_mul(8)?.checked_add(bit_in_byte)?;
                if bit_index >= channel_bits {
                    return None;
                }
                let (pixel_index, channel) = if mode == "Alpha" {
                    (bit_index, mode_channels[0])
                } else {
                    (bit_index / 3, mode_channels[bit_index % 3])
                };
                // 拡張機能と同じ列優先の座標: pixelIndex = x * height + y。
                let x = pixel_index / height;
                let y = pixel_index % height;
                let row_start = y.checked_mul(row_bytes.checked_add(1)?)?;
                let sample_offset = if bit_depth == 16 { 1 } else { 0 };
                let sample_index = x.checked_mul(bytes_per_pixel)?
                    .checked_add(channel.checked_mul(bytes_per_sample)?)?
                    .checked_add(sample_offset)?;
                let value = *decoded.get(row_start.checked_add(1)?.checked_add(sample_index)?)? & 1;
                result |= value << (7 - bit_in_byte);
            }
            Some(result)
        };

        let mut header = [0u8; STEALTH_SIGNATURE_BYTES + 4];
        for (index, byte) in header.iter_mut().enumerate() {
            *byte = read_stream_byte(index)?;
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
        let bit_length = u32::from_be_bytes(header[STEALTH_SIGNATURE_BYTES..].try_into().ok()?) as usize;
        if bit_length % 8 != 0 {
            continue;
        }
        let payload_len = bit_length / 8;
        if payload_len > MAX_STEALTH_PAYLOAD_BYTES ||
            STEALTH_HEADER_BITS.checked_add(bit_length)? > channel_bits {
            continue;
        }
        let mut payload = Vec::with_capacity(payload_len);
        for index in 0..payload_len {
            payload.push(read_stream_byte(header.len().checked_add(index)?)?);
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

fn extract_webp_metadata(mmap: &[u8], path: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("=== WebP File: {} ===", path);
    let mut cursor = 12;
    let len = mmap.len();
    let mut found = false;

    while cursor + 8 <= len {
        let chunk_type = &mmap[cursor..cursor+4];
        let chunk_size = u32::from_le_bytes(mmap[cursor+4..cursor+8].try_into()?) as usize;
        let data_start = cursor + 8;
        if data_start + chunk_size > len { break; }

        if chunk_type == b"EXIF" {
            let exif_data = &mmap[data_start..data_start + chunk_size];
            extract_from_exif(exif_data)?;
            found = true;
        } else if chunk_type == b"XMP " {
            let xmp_data = &mmap[data_start..data_start + chunk_size];
            if let Ok(xmp_str) = str::from_utf8(xmp_data) {
                // XMLから簡易的に parameters 属性を抽出
                if let Some(start) = xmp_str.find("parameters=\"") {
                    let rest = &xmp_str[start + 12..];
                    if let Some(end) = rest.find('\"') {
                        let params = &rest[..end];
                        // 実体参照のデコードは簡易的に
                        let decoded = params.replace("&quot;", "\"").replace("&#10;", "\n");
                        println!("--- WebP XMP [parameters] ---");
                        println!("{}", decoded);
                        found = true;
                    }
                }
            }
        }

        cursor = data_start + chunk_size;
        if chunk_size % 2 == 1 { cursor += 1; }
    }
    
    if !found { println!("(No metadata detected)"); }
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
    // 既存のEXIF解析ロジック
    let (charset, text_data) = if exif_data.len() >= 12 && &exif_data[0..4] == b"\0\0\0\0" {
        (&exif_data[4..12], &exif_data[12..])
    } else if exif_data.len() >= 8 {
        (&exif_data[0..8], &exif_data[8..])
    } else {
        return Ok(());
    };
        
    if charset.starts_with(b"UNICODE") {
        if text_data.len() >= 2 {
            let is_le = text_data[0..2] == [0xFF, 0xFE] || (text_data[1] == 0 && text_data[0] >= 0x20);
            let utf16_data: Vec<u16> = text_data.chunks_exact(2).take(10000).map(|c| {
                if is_le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }
            }).take_while(|&c| c != 0 && c != 0xFEFF).collect();
            if let Ok(text) = String::from_utf16(&utf16_data) {
                println!("--- EXIF UserComment (UNICODE) ---");
                println!("{}", text);
            }
        }
    } else if charset.starts_with(b"ASCII") || charset.iter().all(|&b| b == 0) {
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
    
    // "Exif\0\0" を探す
    let search_len = mmap.len().min(FAST_SCAN_LIMIT);
    let mut tiff_start = 0;
    for i in 0..search_len.saturating_sub(10) {
        if &mmap[i..i+6] == b"Exif\0\0" {
            tiff_start = i + 6;
            break;
        }
    }
    if tiff_start == 0 { println!("(No EXIF marker found)"); return Ok(()); }

    // UserComment 0x9286 を探す (簡易)
    let tag_id = [0x86, 0x92];
    for i in tiff_start..search_len.saturating_sub(12) {
        if mmap[i] == tag_id[0] && mmap[i+1] == tag_id[1] {
            // データオフセットと長さを取得 (LE前提)
            let data_len = u32::from_le_bytes(mmap[i+4..i+8].try_into()?) as usize;
            let offset_val = u32::from_le_bytes(mmap[i+8..i+12].try_into()?) as usize;
            let data_offset = tiff_start + offset_val;
            if data_offset + data_len <= mmap.len() && data_len >= 8 {
                extract_from_exif(&mmap[data_offset..data_offset + data_len])?;
            }
            return Ok(());
        }
    }
    println!("(No metadata detected)");
    Ok(())
}

fn extract_universal_metadata(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let mmap = unsafe { MmapOptions::new().map(&file)? };

    if mmap.len() >= 8 && &mmap[0..8] == &[137, 80, 78, 71, 13, 10, 26, 10] {
        return extract_png_metadata(&mmap, path);
    }
    if mmap.len() >= 12 && &mmap[0..4] == b"RIFF" && &mmap[8..12] == b"WEBP" {
        return extract_webp_metadata(&mmap, path);
    }
    if mmap.len() >= 3 && &mmap[0..3] == &[0xFF, 0xD8, 0xFF] {
        return extract_exif_metadata(&mmap, path, "JPEG");
    }
    
    eprintln!("Unsupported file format");
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <image_file>", args[0]);
        std::process::exit(1);
    }
    if let Err(e) = extract_universal_metadata(&args[1]) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
