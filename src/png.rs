//! PNG text extraction. The ordinary path is Negative-First: parse the prefix,
//! prove the IEND connection from the tail, then escalate only when needed.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read;

use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use memchr::memmem;
use serde_json::Value;

use crate::keywords;
use crate::record::{RecordView, Source};
use crate::tiff;

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const IEND: &[u8; 12] = b"\0\0\0\0IEND\xaeB`\x82";
const IEND_SEARCH_LIMIT: usize = 2 * 1024 * 1024;
const INITIAL_WINDOW: usize = 128 * 1024;
const MEDIUM_WINDOW: usize = 512 * 1024;
const MAX_WINDOW: usize = 2 * 1024 * 1024;
const MAX_IDAT_JUMPS: usize = 4096;
const MAX_TEXT_BYTES: usize = 64 * 1024 * 1024;
const MAX_TRAILING_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug)]
struct Chunk {
    offset: usize,
    end: usize,
    kind: u32,
    data_start: usize,
    data_end: usize,
}

impl Chunk {
    fn data<'a>(&self, bytes: &'a [u8]) -> &'a [u8] {
        &bytes[self.data_start..self.data_end]
    }
}

#[derive(Clone, Debug)]
struct LocatedRecord<'a> {
    offset: usize,
    record_index: usize,
    record: RecordView<'a>,
}

#[derive(Clone, Debug, Default)]
pub struct PngStats {
    /// True when the tail connection and reverse chain proved the post-IDAT
    /// region without entering the Stage C full fallback.
    pub stage_b_resolved: bool,
    /// Escalation step used (1..=4), if Stage B did not resolve the scan.
    pub stage_c_step: Option<u8>,
    pub idat_header_probes: usize,
    pub iend_offset: Option<usize>,
    pub comfy_tail_notice: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PngScan<'a> {
    pub records: Vec<RecordView<'a>>,
    pub stats: PngStats,
}

#[derive(Default)]
struct StageA<'a> {
    records: Vec<LocatedRecord<'a>>,
    stop: usize,
    first_idat: Option<usize>,
    iend_end: Option<usize>,
    malformed: bool,
    comfy_tail_notice: bool,
}

pub fn scan<'a>(bytes: &'a [u8], strict: bool) -> PngScan<'a> {
    if bytes.get(..8) != Some(SIGNATURE.as_slice()) {
        return PngScan::default();
    }

    let mut stats = PngStats::default();
    if strict {
        let (mut located, iend_end, iend_offset) = strict_walk(bytes);
        stats.iend_offset = iend_offset;
        append_trailing(bytes, iend_end, &mut located);
        located.sort_by_key(|entry| entry.offset);
        return PngScan {
            records: located.into_iter().map(|entry| entry.record).collect(),
            stats,
        };
    }

    let mut stage_a = stage_a(bytes);
    stats.comfy_tail_notice = stage_a.comfy_tail_notice;

    // With no IDAT, Stage A already walked the complete file. Corruption before
    // the first IDAT is likewise not a reason to guess at later byte patterns.
    let Some(first_idat) = stage_a.first_idat else {
        stats.iend_offset = stage_a.iend_end.map(|end| end.saturating_sub(12));
        let mut located = stage_a.records;
        append_trailing(bytes, stage_a.iend_end, &mut located);
        located.sort_by_key(|entry| entry.offset);
        return PngScan {
            records: located.into_iter().map(|entry| entry.record).collect(),
            stats,
        };
    };
    if stage_a.malformed {
        return PngScan {
            records: stage_a
                .records
                .into_iter()
                .map(|entry| entry.record)
                .collect(),
            stats,
        };
    }

    let (iend, probes, directly_after_idat) = find_connected_iend(bytes, stage_a.stop);
    stats.idat_header_probes += probes;
    stats.iend_offset = iend;

    let mut post_idat = Vec::new();
    let mut resolved = directly_after_idat;
    let mut chain_progress = reverse_chain_if_needed(bytes, first_idat, iend, directly_after_idat);
    if !directly_after_idat {
        if let Some(progress) = chain_progress.as_ref() {
            if let Some(chain) = progress.finish(bytes) {
                post_idat = chain.records;
                resolved = true;
            }
        }
        if resolved {
            stats.stage_b_resolved = true;
        }
    } else {
        stats.stage_b_resolved = true;
    }

    if !resolved {
        // Stage C discards each failed bounded-walk result before trying the
        // next strategy. Partial Stage B chain records remain separately
        // retained until a later strategy proves the complete post-IDAT range.
        let step1 = walk_post_idat(bytes, first_idat, iend, MAX_IDAT_JUMPS);
        stats.idat_header_probes += step1.probes;
        if step1.reached_iend {
            stats.stage_c_step = Some(1);
            post_idat = step1.records;
            stats.iend_offset = step1.iend_offset.or(stats.iend_offset);
            resolved = true;
        }
    }

    if !resolved {
        if let (Some(iend_at), Some(progress)) = (iend, chain_progress.as_mut()) {
            let mut old_window = INITIAL_WINDOW;
            for new_window in [MEDIUM_WINDOW, MAX_WINDOW] {
                extend_chain(bytes, first_idat, iend_at, progress, old_window, new_window);
                if let Some(chain) = progress.finish(bytes) {
                    stats.stage_c_step = Some(2);
                    post_idat = chain.records;
                    resolved = true;
                    break;
                }
                old_window = new_window;
            }
        }
    }

    let retained_chain_records = if resolved {
        Vec::new()
    } else {
        chain_progress
            .as_ref()
            .map(|progress| progress.partial_records(bytes))
            .unwrap_or_default()
    };

    if !resolved {
        if let Some(walk) = fast_walk(bytes, first_idat, iend) {
            stats.stage_c_step = Some(3);
            post_idat = retained_chain_records.clone();
            post_idat.extend(walk.records);
            stats.iend_offset = walk.iend_offset.or(stats.iend_offset);
            resolved = true;
        }
    }

    if !resolved {
        let fallback = full_post_idat_walk(bytes, first_idat);
        stats.stage_c_step = Some(4);
        post_idat = retained_chain_records;
        post_idat.extend(fallback.records);
        stats.iend_offset = fallback.iend_offset.or(stats.iend_offset);
    }

    stage_a.records.append(&mut post_idat);
    if let Some(iend_at) = stats.iend_offset {
        let iend_end = chunk_at(bytes, iend_at).map(|chunk| chunk.end);
        append_trailing(bytes, iend_end, &mut stage_a.records);
    }
    stage_a.records.sort_by_key(|entry| entry.offset);
    dedupe_chunk_records(&mut stage_a.records);
    PngScan {
        records: stage_a
            .records
            .into_iter()
            .map(|entry| entry.record)
            .collect(),
        stats,
    }
}

fn strict_walk<'a>(bytes: &'a [u8]) -> (Vec<LocatedRecord<'a>>, Option<usize>, Option<usize>) {
    let mut records = Vec::new();
    let mut cursor = 8usize;
    loop {
        let Some(chunk) = chunk_at(bytes, cursor) else {
            return (records, None, None);
        };
        collect_chunk_records(bytes, chunk, &mut records);
        if chunk.kind == kind(b"IEND") {
            return (records, Some(chunk.end), Some(chunk.offset));
        }
        cursor = chunk.end;
    }
}

fn stage_a<'a>(bytes: &'a [u8]) -> StageA<'a> {
    let mut result = StageA::default();
    let mut cursor = 8usize;
    let mut saw_ihdr = false;
    let mut next_after_ihdr = false;
    loop {
        let Some(chunk) = chunk_at(bytes, cursor) else {
            result.stop = cursor;
            result.malformed = true;
            return result;
        };
        if next_after_ihdr {
            result.comfy_tail_notice = chunk.kind == kind(b"IDAT")
                && chunk.data_end.saturating_sub(chunk.data_start) == 65_536
                && bytes.len() > 65_536;
            next_after_ihdr = false;
        }
        if chunk.kind == kind(b"IDAT") {
            result.stop = cursor;
            result.first_idat = Some(cursor);
            result.comfy_tail_notice |= bytes.len() > 65_536 && result.records.is_empty();
            return result;
        }
        collect_chunk_records(bytes, chunk, &mut result.records);
        cursor = chunk.end;
        if chunk.kind == kind(b"IHDR") {
            saw_ihdr = true;
            next_after_ihdr = true;
        } else if !saw_ihdr {
            next_after_ihdr = false;
        }
        if chunk.kind == kind(b"IEND") {
            result.stop = chunk.end;
            result.iend_end = Some(chunk.end);
            return result;
        }
    }
}

fn chunk_at(bytes: &[u8], offset: usize) -> Option<Chunk> {
    let header_end = offset.checked_add(8)?;
    let header = bytes.get(offset..header_end)?;
    let len = usize::try_from(u32::from_be_bytes(header[..4].try_into().ok()?)).ok()?;
    let data_start = header_end;
    let data_end = data_start.checked_add(len)?;
    let end = data_end.checked_add(4)?;
    bytes.get(data_start..end)?;
    Some(Chunk {
        offset,
        end,
        kind: u32::from_be_bytes(header[4..8].try_into().ok()?),
        data_start,
        data_end,
    })
}

fn kind(bytes: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*bytes)
}

fn collect_chunk_records<'a>(bytes: &'a [u8], chunk: Chunk, output: &mut Vec<LocatedRecord<'a>>) {
    let data = chunk.data(bytes);
    match chunk.kind {
        k if k == kind(b"tEXt") => append_record(
            output,
            chunk.offset,
            set_text_detail(parse_text(data), "tEXt"),
        ),
        k if k == kind(b"zTXt") => append_record(
            output,
            chunk.offset,
            set_text_detail(parse_ztxt(data), "zTXt"),
        ),
        k if k == kind(b"iTXt") => append_record(
            output,
            chunk.offset,
            set_text_detail(parse_itxt(data), "iTXt"),
        ),
        k if k == kind(b"eXIf") => {
            output.extend(
                tiff::parse(data, Source::PngExif)
                    .into_iter()
                    .enumerate()
                    .map(|(record_index, record)| LocatedRecord {
                        offset: chunk.offset,
                        record_index,
                        record,
                    }),
            );
        }
        _ => {}
    }
}

fn set_text_detail<'a>(
    record: Option<RecordView<'a>>,
    detail: &'static str,
) -> Option<RecordView<'a>> {
    record.map(|mut record| {
        record.detail = Some(Cow::Borrowed(detail));
        record
    })
}

fn append_record<'a>(
    output: &mut Vec<LocatedRecord<'a>>,
    offset: usize,
    record: Option<RecordView<'a>>,
) {
    if let Some(record) = record {
        output.push(LocatedRecord {
            offset,
            record_index: 0,
            record,
        });
    }
}

fn text_record<'a>(keyword_bytes: &[u8], body: Cow<'a, [u8]>) -> Option<RecordView<'a>> {
    let definition = keywords::lookup(keyword_bytes)?;
    if definition.xmp_packet {
        return None; // delegated to the Phase 3 XMP parser
    }
    let decoded = match body {
        Cow::Borrowed(bytes) => decode_text(bytes)?,
        Cow::Owned(bytes) => decode_text_owned(bytes)?,
    };
    let body = if definition.json_settings {
        match serde_json::from_str::<Value>(&decoded) {
            Ok(value) => match serde_json::to_string_pretty(&value) {
                Ok(pretty) => Cow::Owned(pretty),
                Err(_) => decoded,
            },
            Err(_) => decoded,
        }
    } else {
        decoded
    };
    Some(RecordView {
        source: Source::PngText,
        label: Cow::Borrowed(definition.label),
        body,
        keyword: Some(Cow::Borrowed(definition.name)),
        detail: None,
    })
}

fn parse_text<'a>(data: &'a [u8]) -> Option<RecordView<'a>> {
    let split = data.iter().position(|&byte| byte == 0)?;
    let body_start = split.checked_add(1)?;
    text_record(&data[..split], Cow::Borrowed(data.get(body_start..)?))
}

fn parse_ztxt(data: &[u8]) -> Option<RecordView<'static>> {
    let split = data.iter().position(|&byte| byte == 0)?;
    let method = *data.get(split.checked_add(1)?)?;
    if method != 0 {
        return None;
    }
    let body_start = split.checked_add(2)?;
    let decoded = decompress_text(data.get(body_start..)?)?;
    text_record(&data[..split], Cow::Owned(decoded))
}

fn parse_itxt<'a>(data: &'a [u8]) -> Option<RecordView<'a>> {
    let key_end = data.iter().position(|&byte| byte == 0)?;
    let flag = *data.get(key_end.checked_add(1)?)?;
    let method = *data.get(key_end.checked_add(2)?)?;
    if method != 0 || flag > 1 {
        return None;
    }
    let after_fields = key_end.checked_add(3)?;
    let lang_end_rel = data
        .get(after_fields..)?
        .iter()
        .position(|&byte| byte == 0)?;
    let translated_start = after_fields.checked_add(lang_end_rel)?.checked_add(1)?;
    let translated_end_rel = data
        .get(translated_start..)?
        .iter()
        .position(|&byte| byte == 0)?;
    let text_start = translated_start
        .checked_add(translated_end_rel)?
        .checked_add(1)?;
    let text_bytes = data.get(text_start..)?;
    let decoded = if flag == 1 {
        Cow::Owned(decompress_text(text_bytes)?)
    } else {
        Cow::Borrowed(text_bytes)
    };
    // iTXt's text is UTF-8 (compressed or not). Decode using the shared UTF-8 /
    // Latin-1 policy only for non-XMP registered text, after field validation.
    std::str::from_utf8(decoded.as_ref()).ok()?;
    text_record(&data[..key_end], decoded)
}

fn decode_text<'a>(bytes: &'a [u8]) -> Option<Cow<'a, str>> {
    if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe]) {
        return decode_utf16(rest, true).map(Cow::Owned);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xfe, 0xff]) {
        return decode_utf16(rest, false).map(Cow::Owned);
    }
    Some(match std::str::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => Cow::Owned(decode_latin1(bytes)?),
    })
}

fn decode_text_owned(bytes: Vec<u8>) -> Option<Cow<'static, str>> {
    if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe]) {
        return decode_utf16(rest, true).map(Cow::Owned);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xfe, 0xff]) {
        return decode_utf16(rest, false).map(Cow::Owned);
    }
    Some(match String::from_utf8(bytes) {
        Ok(text) => Cow::Owned(text),
        Err(error) => Cow::Owned(decode_latin1(&error.into_bytes())?),
    })
}

fn decode_latin1(bytes: &[u8]) -> Option<String> {
    // Latin-1 bytes can each require up to two UTF-8 bytes. Reserve the full
    // checked upper bound fallibly, without imposing a text-size limit.
    let capacity = bytes.len().checked_mul(2)?;
    let mut decoded = String::new();
    decoded.try_reserve_exact(capacity).ok()?;
    for &byte in bytes {
        decoded.push(char::from(byte));
    }
    Some(decoded)
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Option<String> {
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    String::from_utf16(&units).ok()
}

fn decompress_text(compressed: &[u8]) -> Option<Vec<u8>> {
    let mut successes = Vec::new();
    if let Some(value) = read_bounded(ZlibDecoder::new(compressed)) {
        successes.push(value);
    }
    if let Some(value) = read_bounded(GzDecoder::new(compressed)) {
        successes.push(value);
    }
    if let Some(value) = read_bounded(DeflateDecoder::new(compressed)) {
        successes.push(value);
    }
    let first_utf8 = successes
        .iter()
        .position(|value| std::str::from_utf8(value).is_ok());
    first_utf8
        .map(|index| successes.swap_remove(index))
        .or_else(|| {
            if successes.is_empty() {
                None
            } else {
                Some(successes.remove(0))
            }
        })
}

fn read_bounded<R: Read>(reader: R) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    let mut reader = reader.take((MAX_TEXT_BYTES as u64).checked_add(1)?);
    reader.read_to_end(&mut output).ok()?;
    if output.len() > MAX_TEXT_BYTES {
        None
    } else {
        Some(output)
    }
}

fn find_connected_iend(bytes: &[u8], start: usize) -> (Option<usize>, usize, bool) {
    let search_start = bytes.len().saturating_sub(IEND_SEARCH_LIMIT);
    let mut candidates = Vec::new();
    if bytes.len() >= IEND.len() && bytes.ends_with(IEND) {
        candidates.push(bytes.len() - IEND.len());
    }
    if let Some(window) = bytes.get(search_start..) {
        for type_offset in memmem::find_iter(window, b"IEND") {
            let Some(offset) = search_start
                .checked_add(type_offset)
                .and_then(|at| at.checked_sub(4))
            else {
                continue;
            };
            if offset
                .checked_add(IEND.len())
                .and_then(|end| bytes.get(offset..end))
                == Some(IEND.as_slice())
                && !candidates.contains(&offset)
            {
                candidates.push(offset);
                if candidates.len() >= 16 {
                    break;
                }
            }
        }
    }
    let priority = candidates.first().copied();
    let mut rest: Vec<usize> = candidates
        .into_iter()
        .filter(|candidate| Some(*candidate) != priority)
        .collect();
    rest.sort_unstable();
    let mut ordered = Vec::new();
    if let Some(priority) = priority {
        ordered.push(priority);
    }
    ordered.extend(rest);
    ordered.truncate(16);
    let mut probes: usize = 0;
    for candidate in ordered {
        let (reached, count, no_post_idat_chunks) = connect_to_iend(bytes, start, candidate);
        probes = probes.saturating_add(count);
        if reached {
            // Stage B can prove absence immediately when the final IDAT ends
            // exactly at the connected IEND. The ordinary reverse chain is
            // unnecessary in that case.
            return (Some(candidate), probes, no_post_idat_chunks);
        }
    }
    (None, probes, false)
}

fn connect_to_iend(bytes: &[u8], start: usize, target: usize) -> (bool, usize, bool) {
    let mut cursor = start;
    let mut probes = 0usize;
    let mut non_idat_since_last_idat = false;
    let mut saw_idat = false;
    while cursor <= target {
        if cursor == target {
            return (
                cursor
                    .checked_add(IEND.len())
                    .and_then(|end| bytes.get(cursor..end))
                    == Some(IEND.as_slice()),
                probes,
                !non_idat_since_last_idat && saw_idat,
            );
        }
        let Some(chunk) = chunk_at(bytes, cursor) else {
            return (false, probes, false);
        };
        if chunk.offset > target || chunk.end > target {
            return (false, probes, false);
        }
        if chunk.kind == kind(b"IEND") {
            return (false, probes, false);
        }
        if chunk.kind == kind(b"IDAT") {
            probes = probes.saturating_add(1);
            saw_idat = true;
            non_idat_since_last_idat = false;
            if probes > MAX_IDAT_JUMPS {
                return (false, probes, false);
            }
        } else if saw_idat {
            non_idat_since_last_idat = true;
        }
        cursor = chunk.end;
    }
    (false, probes, false)
}

struct Chain<'a> {
    records: Vec<LocatedRecord<'a>>,
}

struct ChainProgress {
    frontier: usize,
    reversed_chunks: Vec<Chunk>,
    reached_idat: bool,
    bad_crc_at_frontier: bool,
}

impl ChainProgress {
    fn finish<'a>(&self, bytes: &'a [u8]) -> Option<Chain<'a>> {
        if !self.reached_idat {
            return None;
        }
        let mut chunks = self.reversed_chunks.clone();
        chunks.reverse();
        let mut records = Vec::new();
        for chunk in chunks {
            collect_chunk_records(bytes, chunk, &mut records);
        }
        Some(Chain { records })
    }

    fn partial_records<'a>(&self, bytes: &'a [u8]) -> Vec<LocatedRecord<'a>> {
        let mut chunks = self.reversed_chunks.clone();
        chunks.reverse();
        let mut records = Vec::new();
        for chunk in chunks {
            collect_chunk_records(bytes, chunk, &mut records);
        }
        records.sort_by_key(|entry| entry.offset);
        records
    }
}

fn reverse_chain(bytes: &[u8], first_idat: usize, iend: usize, window: usize) -> ChainProgress {
    let low = iend.saturating_sub(window).max(first_idat);
    let candidates = reverse_candidates(bytes, low, iend);
    let mut progress = ChainProgress {
        frontier: iend,
        reversed_chunks: Vec::new(),
        reached_idat: false,
        bad_crc_at_frontier: false,
    };
    follow_candidates(bytes, first_idat, &candidates, &mut progress);
    progress
}

fn reverse_chain_if_needed(
    bytes: &[u8],
    first_idat: usize,
    iend: Option<usize>,
    directly_after_idat: bool,
) -> Option<ChainProgress> {
    if directly_after_idat {
        None
    } else {
        iend.map(|iend_at| reverse_chain(bytes, first_idat, iend_at, INITIAL_WINDOW))
    }
}

fn extend_chain(
    bytes: &[u8],
    first_idat: usize,
    iend: usize,
    progress: &mut ChainProgress,
    old_window: usize,
    new_window: usize,
) {
    let low = iend.saturating_sub(new_window).max(first_idat);
    let high = iend.saturating_sub(old_window).max(first_idat);
    if low >= high || progress.reached_idat {
        return;
    }
    // Only the newly exposed bytes are searched. All chunks reached in this
    // range have lower offsets, so their predecessors remain in this range.
    let candidates = reverse_candidates(bytes, low, high);
    follow_candidates(bytes, first_idat, &candidates, progress);
}

struct ReverseCandidates {
    by_end: HashMap<usize, Vec<Chunk>>,
}

fn reverse_candidates(bytes: &[u8], low: usize, high: usize) -> ReverseCandidates {
    if low >= high {
        return ReverseCandidates {
            by_end: HashMap::new(),
        };
    }
    let start = low.max(8);
    let Some(range) = bytes.get(start..high) else {
        return ReverseCandidates {
            by_end: HashMap::new(),
        };
    };
    let mut candidates = Vec::new();
    for known in KNOWN_TYPES {
        for found in memmem::find_iter(range, known) {
            let Some(offset) = start.checked_add(found).and_then(|at| at.checked_sub(4)) else {
                continue;
            };
            add_reverse_candidate(bytes, offset, low, high, &mut candidates);
        }
    }
    // Unknown ancillary types are admitted only inside the current fixed-size
    // window. No CRC is computed until a candidate satisfies the length equation.
    let type_start = start.saturating_add(4);
    for type_offset in type_start..high {
        if bytes.get(type_offset).is_some_and(u8::is_ascii_lowercase) {
            if let Some(offset) = type_offset.checked_sub(4) {
                add_reverse_candidate(bytes, offset, low, high, &mut candidates);
            }
        }
    }
    candidates.sort_by_key(|chunk| chunk.offset);
    candidates.dedup_by_key(|chunk| chunk.offset);
    let mut by_end = HashMap::new();
    for candidate in candidates {
        by_end
            .entry(candidate.end)
            .or_insert_with(Vec::new)
            .push(candidate);
    }
    ReverseCandidates { by_end }
}

fn add_reverse_candidate(
    bytes: &[u8],
    offset: usize,
    low: usize,
    high: usize,
    candidates: &mut Vec<Chunk>,
) {
    if offset < low || offset >= high {
        return;
    }
    if let Some(chunk) = chunk_at(bytes, offset) {
        if allowed_reverse_type(chunk.kind) {
            candidates.push(chunk);
        }
    }
}

fn follow_candidates(
    bytes: &[u8],
    first_idat: usize,
    candidates: &ReverseCandidates,
    progress: &mut ChainProgress,
) {
    while !progress.reached_idat {
        let (candidate, bad_crc) =
            predecessor_from_candidates(bytes, candidates, progress.frontier);
        let bad_crc = bad_crc || progress.bad_crc_at_frontier;
        let Some(candidate) = candidate else {
            progress.bad_crc_at_frontier = bad_crc;
            return;
        };
        if candidate.kind == kind(b"IDAT") {
            if bad_crc {
                progress.bad_crc_at_frontier = true;
                return;
            }
            progress.reached_idat = true;
            return;
        }
        if candidate.offset == first_idat {
            return;
        }
        progress.frontier = candidate.offset;
        progress.reversed_chunks.push(candidate);
        progress.bad_crc_at_frontier = false;
    }
}

fn predecessor_from_candidates(
    bytes: &[u8],
    candidates: &ReverseCandidates,
    frontier: usize,
) -> (Option<Chunk>, bool) {
    let mut non_idat = None;
    let mut idat = None;
    let mut bad_crc = false;
    let Some(candidates) = candidates.by_end.get(&frontier) else {
        return (None, false);
    };
    for &chunk in candidates {
        if chunk.offset >= frontier {
            continue;
        }
        if chunk.kind == kind(b"IDAT") {
            idat.get_or_insert(chunk);
            continue;
        }
        let mut hasher = crc32fast::Hasher::new();
        let Some(type_start) = chunk.offset.checked_add(4) else {
            continue;
        };
        let Some(type_end) = chunk.offset.checked_add(8) else {
            continue;
        };
        let Some(chunk_type) = bytes.get(type_start..type_end) else {
            continue;
        };
        hasher.update(chunk_type);
        hasher.update(chunk.data(bytes));
        let Some(crc_bytes) = bytes.get(chunk.data_end..chunk.end) else {
            continue;
        };
        let Ok(expected_bytes) = <[u8; 4]>::try_from(crc_bytes) else {
            continue;
        };
        if hasher.finalize() == u32::from_be_bytes(expected_bytes) {
            non_idat.get_or_insert(chunk);
        } else {
            bad_crc = true;
        }
    }
    (non_idat.or(idat), bad_crc)
}

fn allowed_reverse_type(value: u32) -> bool {
    KNOWN_TYPES
        .iter()
        .any(|kind| u32::from_be_bytes(**kind) == value)
        || value.to_be_bytes()[0].is_ascii_lowercase()
}

const KNOWN_TYPES: [&[u8; 4]; 23] = [
    b"IHDR", b"PLTE", b"IDAT", b"IEND", b"tEXt", b"zTXt", b"iTXt", b"eXIf", b"iCCP", b"sRGB",
    b"gAMA", b"cHRM", b"pHYs", b"tIME", b"sBIT", b"sPLT", b"tRNS", b"bKGD", b"hIST", b"sTER",
    b"acTL", b"fcTL", b"fdAT",
];

struct PostWalk<'a> {
    records: Vec<LocatedRecord<'a>>,
    reached_iend: bool,
    iend_offset: Option<usize>,
    probes: usize,
}

fn walk_post_idat<'a>(
    bytes: &'a [u8],
    first_idat: usize,
    expected_iend: Option<usize>,
    limit: usize,
) -> PostWalk<'a> {
    let mut cursor = first_idat;
    let mut pending = Vec::new();
    let mut probes = 0usize;
    loop {
        let Some(chunk) = chunk_at(bytes, cursor) else {
            return PostWalk {
                records: Vec::new(),
                reached_iend: false,
                iend_offset: None,
                probes,
            };
        };
        if chunk.kind == kind(b"IDAT") {
            probes = probes.saturating_add(1);
            if probes > limit {
                return PostWalk {
                    records: Vec::new(),
                    reached_iend: false,
                    iend_offset: None,
                    probes,
                };
            }
            pending.clear();
            cursor = chunk.end;
            continue;
        }
        collect_chunk_records(bytes, chunk, &mut pending);
        if chunk.kind == kind(b"IEND") {
            if expected_iend.is_some_and(|expected| expected != chunk.offset) {
                return PostWalk {
                    records: Vec::new(),
                    reached_iend: false,
                    iend_offset: Some(chunk.offset),
                    probes,
                };
            }
            return PostWalk {
                records: pending,
                reached_iend: true,
                iend_offset: Some(chunk.offset),
                probes,
            };
        }
        cursor = chunk.end;
    }
}

fn fast_walk<'a>(
    bytes: &'a [u8],
    first_idat: usize,
    expected_iend: Option<usize>,
) -> Option<PostWalk<'a>> {
    let first = chunk_at(bytes, first_idat)?;
    if first.kind != kind(b"IDAT") {
        return None;
    }
    let length = first.data_end.checked_sub(first.data_start)?;
    let second = chunk_at(bytes, first.end)?;
    let third = chunk_at(bytes, second.end)?;
    if second.kind != kind(b"IDAT")
        || third.kind != kind(b"IDAT")
        || second.data_end - second.data_start != length
        || third.data_end - third.data_start != length
    {
        return None;
    }
    let stride = length.checked_add(12)?;
    let max_k = bytes.len().saturating_sub(first_idat) / stride;
    let predicate = |k: usize| -> bool {
        let Some(offset) = k
            .checked_mul(stride)
            .and_then(|n| first_idat.checked_add(n))
        else {
            return false;
        };
        chunk_at(bytes, offset).is_some_and(|chunk| {
            chunk.kind == kind(b"IDAT") && chunk.data_end - chunk.data_start == length
        })
    };
    let mut low = 0usize;
    let mut high = max_k.saturating_add(1);
    while low + 1 < high {
        let mid = low + (high - low) / 2;
        if predicate(mid) {
            low = mid;
        } else {
            high = mid;
        }
    }
    if !predicate(low) || low < 2 {
        return None;
    }
    let mut candidate = low
        .checked_add(1)?
        .checked_mul(stride)?
        .checked_add(first_idat)?;
    let mut chunk = chunk_at(bytes, candidate)?;
    if chunk.kind == kind(b"IDAT") {
        candidate = chunk.end;
        chunk = chunk_at(bytes, candidate)?;
    }
    if chunk.kind == kind(b"IDAT") {
        return None;
    }
    let mut records = Vec::new();
    let mut cursor = candidate;
    loop {
        let current = chunk_at(bytes, cursor)?;
        if current.kind == kind(b"IDAT") {
            return None;
        }
        collect_chunk_records(bytes, current, &mut records);
        if current.kind == kind(b"IEND") {
            if expected_iend.is_some_and(|expected| expected != current.offset) {
                return None;
            }
            return Some(PostWalk {
                records,
                reached_iend: true,
                iend_offset: Some(current.offset),
                probes: 0,
            });
        }
        cursor = current.end;
    }
}

fn full_post_idat_walk<'a>(bytes: &'a [u8], first_idat: usize) -> PostWalk<'a> {
    let mut cursor = first_idat;
    let mut pending = Vec::new();
    loop {
        let Some(chunk) = chunk_at(bytes, cursor) else {
            return PostWalk {
                records: pending,
                reached_iend: false,
                iend_offset: None,
                probes: 0,
            };
        };
        if chunk.kind == kind(b"IDAT") {
            pending.clear();
        } else {
            collect_chunk_records(bytes, chunk, &mut pending);
            if chunk.kind == kind(b"IEND") {
                return PostWalk {
                    records: pending,
                    reached_iend: true,
                    iend_offset: Some(chunk.offset),
                    probes: 0,
                };
            }
        }
        cursor = chunk.end;
    }
}

fn append_trailing<'a>(
    bytes: &'a [u8],
    iend_end: Option<usize>,
    records: &mut Vec<LocatedRecord<'a>>,
) {
    let Some(start) = iend_end else {
        return;
    };
    if start >= bytes.len()
        || start
            .checked_add(SIGNATURE.len())
            .and_then(|end| bytes.get(start..end))
            == Some(SIGNATURE.as_slice())
    {
        return;
    }
    let Some(end) = start
        .checked_add(MAX_TRAILING_BYTES)
        .map(|end| end.min(bytes.len()))
    else {
        return;
    };
    let Some(trailing) = bytes.get(start..end) else {
        return;
    };
    let leading = trailing
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(trailing.len());
    if trailing.get(leading) == Some(&b'{') {
        let input = trailing.get(leading..).unwrap_or_default();
        let mut stream = serde_json::Deserializer::from_slice(input).into_iter::<Value>();
        if stream.next().is_some_and(|result| result.is_ok()) {
            let consumed = stream.byte_offset().min(input.len());
            if consumed > 0 {
                let body = match std::str::from_utf8(&input[..consumed]) {
                    Ok(text) => Cow::Borrowed(text),
                    Err(_) => Cow::Owned(String::from_utf8_lossy(&input[..consumed]).into_owned()),
                };
                records.push(LocatedRecord {
                    offset: start,
                    record_index: 0,
                    record: RecordView {
                        source: Source::Trailing,
                        label: Cow::Borrowed("Trailing Data (after IEND)"),
                        body,
                        keyword: None,
                        detail: None,
                    },
                });
                return;
            }
        }
    }
    let mut trailing_records = Vec::new();
    for keyword in keywords::registered() {
        for found in memmem::find_iter(trailing, keyword) {
            let Some(body_start) = found.checked_add(keyword.len()) else {
                continue;
            };
            let mut body_start = body_start;
            if trailing
                .get(body_start)
                .is_some_and(|byte| matches!(byte, 0 | b'=' | b':'))
            {
                let Some(next) = body_start.checked_add(1) else {
                    continue;
                };
                body_start = next;
            }
            let body_end = trailing
                .get(body_start..)
                .and_then(|tail| {
                    tail.iter()
                        .position(|&byte| byte == 0)
                        .and_then(|n| body_start.checked_add(n))
                })
                .unwrap_or(trailing.len());
            let body = decode_text(&trailing[body_start..body_end]).unwrap_or(Cow::Borrowed(""));
            let keyword_text = String::from_utf8_lossy(keyword).into_owned();
            let label = keywords::lookup(keyword)
                .map(|definition| definition.label)
                .unwrap_or("Trailing Data (after IEND)");
            trailing_records.push(LocatedRecord {
                offset: start.checked_add(found).unwrap_or(start),
                record_index: 0,
                record: RecordView {
                    source: Source::Trailing,
                    label: Cow::Borrowed("Trailing Data (after IEND)"),
                    body,
                    keyword: Some(Cow::Owned(keyword_text)),
                    detail: Some(Cow::Owned(label.to_owned())),
                },
            });
        }
    }
    trailing_records.sort_by_key(|entry| entry.offset);
    records.extend(trailing_records);
}

fn dedupe_chunk_records(records: &mut Vec<LocatedRecord<'_>>) {
    let mut seen = Vec::new();
    records.retain(|entry| {
        if entry.record.source == Source::Trailing {
            return true;
        }
        let key = (entry.offset, entry.record_index);
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::{DeflateEncoder, GzEncoder, ZlibEncoder};
    use flate2::Compression;
    use std::io::Write;

    fn chunk(kind: &[u8; 4], data: &[u8], valid_crc: bool) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(&(data.len() as u32).to_be_bytes());
        output.extend_from_slice(kind);
        output.extend_from_slice(data);
        let crc = crc32fast::hash(&output[4..]);
        output.extend_from_slice(&(if valid_crc { crc } else { crc ^ 1 }).to_be_bytes());
        output
    }

    fn png(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut output = SIGNATURE.to_vec();
        output.extend_from_slice(&chunk(b"IHDR", &[0; 13], true));
        for item in chunks {
            output.extend_from_slice(item);
        }
        output.extend_from_slice(&chunk(b"IEND", &[], true));
        output
    }

    fn text(keyword: &[u8], body: &[u8]) -> Vec<u8> {
        let mut data = keyword.to_vec();
        data.push(0);
        data.extend_from_slice(body);
        chunk(b"tEXt", &data, true)
    }

    fn exif_user_comment(comment: &[u8]) -> Vec<u8> {
        let mut value = b"ASCII\0\0\0".to_vec();
        value.extend_from_slice(comment);
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x9286u16.to_le_bytes());
        tiff.extend_from_slice(&7u16.to_le_bytes());
        tiff.extend_from_slice(&(value.len() as u32).to_le_bytes());
        tiff.extend_from_slice(&26u32.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(&value);
        chunk(b"eXIf", &tiff, true)
    }

    fn exif_with_software_and_user_comment() -> Vec<u8> {
        let software = b"Generator\0";
        let mut user_comment = b"ASCII\0\0\0".to_vec();
        user_comment.extend_from_slice(b"parameters=from-exif-ifd");
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&0x0131u16.to_le_bytes());
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&(software.len() as u32).to_le_bytes());
        tiff.extend_from_slice(&((56 + user_comment.len()) as u32).to_le_bytes());
        tiff.extend_from_slice(&0x8769u16.to_le_bytes());
        tiff.extend_from_slice(&4u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&38u32.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());

        // ExifIFD is at offset 38 and its UserComment bytes begin at offset 56.
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x9286u16.to_le_bytes());
        tiff.extend_from_slice(&7u16.to_le_bytes());
        tiff.extend_from_slice(&(user_comment.len() as u32).to_le_bytes());
        tiff.extend_from_slice(&56u32.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(&user_comment);
        tiff.extend_from_slice(software);
        chunk(b"eXIf", &tiff, true)
    }

    fn bodies(bytes: &[u8], strict: bool) -> Vec<String> {
        scan(bytes, strict)
            .records
            .into_iter()
            .map(|record| record.body.into_owned())
            .collect()
    }

    #[test]
    fn phase1_text_decoding_and_registered_keywords() {
        let utf8 = text(b"parameters", "日本語".as_bytes());
        let latin1 = text(b"prompt", &[b'c', b'a', b'f', 0xe9]);
        let utf16 = text(b"workflow", &[0xff, 0xfe, b'h', 0, b'i', 0]);
        let result = bodies(&png(&[utf8, latin1, utf16]), false);
        assert_eq!(result, ["日本語", "café", "hi"]);
    }

    #[test]
    fn latin1_fallback_uses_fallible_allocation_and_preserves_bytes() {
        let bytes: Vec<u8> = (0x80..=0xff).collect();
        let decoded = decode_latin1(&bytes).unwrap();
        let expected: String = bytes.iter().map(|&byte| char::from(byte)).collect();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn p09_utf8_text_bodies_borrow_the_input_buffer() {
        let image = png(&[text(b"parameters", b"borrowed workflow")]);
        let result = scan(&image, false);
        assert!(matches!(
            result.records[0].body,
            Cow::Borrowed("borrowed workflow")
        ));
    }

    #[test]
    fn fixture_7_reads_exif_user_comment_from_png() {
        let image = png(&[exif_user_comment(b"parameters=from-png-exif")]);
        let result = scan(&image, false);
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].source, Source::PngExif);
        assert_eq!(result.records[0].label, "EXIF UserComment");
        assert_eq!(result.records[0].body, "parameters=from-png-exif");
    }

    #[test]
    fn png_exif_keeps_distinct_tags_from_one_chunk_after_chunk_deduplication() {
        let image = png(&[
            exif_with_software_and_user_comment(),
            chunk(b"IDAT", &[1, 2, 3], true),
        ]);
        let result = scan(&image, false);
        assert_eq!(result.records.len(), 2);
        assert_eq!(result.records[0].label, "EXIF Software");
        assert_eq!(result.records[0].body, "Generator");
        assert_eq!(result.records[1].label, "EXIF UserComment");
        assert_eq!(result.records[1].body, "parameters=from-exif-ifd");
    }

    #[test]
    fn fixture_40_finds_post_idat_exif_user_comment() {
        let image = png(&[
            chunk(b"IDAT", &[1, 2, 3], true),
            exif_user_comment(b"parameters=post-idat-exif"),
        ]);
        let result = scan(&image, false);
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].body, "parameters=post-idat-exif");
    }

    #[test]
    fn fixture_36_deduplicates_matching_text_and_exif_records() {
        let image = png(&[
            text(b"parameters", b"same parameters"),
            text(b"Description", b"intervening record"),
            exif_user_comment(b"same parameters"),
        ]);
        let result = scan(&image, false);
        let mut sink = crate::output::TextSink::default();
        for record in result.records {
            sink.push_record(record);
        }
        let mut output = Vec::new();
        sink.flush(&mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "--- Stable Diffusion (A1111) ---\nsame parameters\n(also in: png_exif UserComment)\n--- NovelAI [Prompt] ---\nintervening record\n"
        );
    }

    #[test]
    fn phase1_strict_is_the_only_path_that_keeps_idat_interstitial_text() {
        let first = chunk(b"IDAT", &[1], true);
        let middle = text(b"parameters", b"between");
        let second = chunk(b"IDAT", &[2], true);
        let image = png(&[first, middle, second]);
        assert!(bodies(&image, false).is_empty());
        assert_eq!(bodies(&image, true), ["between"]);
    }

    #[test]
    fn phase1_reads_post_idat_text_even_when_an_earlier_record_exists() {
        let before = text(b"Description", b"before");
        let idat = chunk(b"IDAT", &[1], true);
        let after = text(b"workflow", b"after");
        let result = bodies(&png(&[before, idat, after]), false);
        assert_eq!(result, ["before", "after"]);
    }

    #[test]
    fn fixture_3_finds_text_before_the_end_window() {
        let idat = chunk(b"IDAT", &[1], true);
        let workflow = text(b"workflow", b"middle");
        let large_ancillary = chunk(b"iCCP", &vec![0x55; 160 * 1024], true);
        let image = png(&[idat, workflow, large_ancillary]);
        let result = scan(&image, false);
        assert_eq!(
            result
                .records
                .iter()
                .map(|r| r.body.as_ref())
                .collect::<Vec<_>>(),
            ["middle"]
        );
        assert_eq!(result.stats.stage_c_step, Some(1));
    }

    #[test]
    fn fixture_4_reads_a_200k_text_chunk() {
        let idat = chunk(b"IDAT", &[1], true);
        let workflow = text(b"workflow", &vec![b'x'; 200 * 1024]);
        let image = png(&[idat, workflow]);
        let result = scan(&image, false);
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].body.len(), 200 * 1024);
    }

    #[test]
    fn fixture_5_finds_text_after_a_large_iccp_chunk() {
        let idat = chunk(b"IDAT", &[1], true);
        let profile = chunk(b"iCCP", &vec![0x31; 180 * 1024], true);
        let prompt = text(b"prompt", b"after profile");
        assert_eq!(
            bodies(&png(&[idat, profile, prompt]), false),
            ["after profile"]
        );
    }

    #[test]
    fn fixture_6_keeps_prefix_and_post_idat_records_without_comfy_fingerprint() {
        let description = text(b"Description", b"prefix");
        let idat = chunk(b"IDAT", &[1, 2, 3], true);
        let workflow = text(b"workflow", b"suffix");
        let image = png(&[description, idat, workflow]);
        let result = scan(&image, false);
        assert_eq!(
            result
                .records
                .iter()
                .map(|r| r.body.as_ref())
                .collect::<Vec<_>>(),
            ["prefix", "suffix"]
        );
        assert!(!result.stats.comfy_tail_notice);
    }

    #[test]
    fn negative_first_absent_fixtures_finish_in_stage_b() {
        let many_idats: Vec<Vec<u8>> = (0..1024)
            .map(|byte| chunk(b"IDAT", &[byte as u8], true))
            .collect();
        let many_image = png(&many_idats);
        let many = scan(&many_image, false);
        assert!(many.stats.stage_b_resolved);
        assert_eq!(many.stats.stage_c_step, None);
        assert_eq!(many.stats.idat_header_probes, 1024);

        let canvas_image = png(&[chunk(b"IDAT", &[0; 1024], true)]);
        let canvas = scan(&canvas_image, false);
        assert!(canvas.stats.stage_b_resolved);
        assert_eq!(canvas.stats.stage_c_step, None);
        assert_eq!(canvas.stats.idat_header_probes, 1);

        let photo_image = png(&[chunk(b"IDAT", &vec![0x44; 3 * 1024 * 1024], true)]);
        let large_photo = scan(&photo_image, false);
        assert!(large_photo.stats.stage_b_resolved);
        assert_eq!(large_photo.stats.stage_c_step, None);
        assert_eq!(large_photo.stats.idat_header_probes, 1);
        assert!(large_photo.records.is_empty());
    }

    #[test]
    fn directly_connected_iend_skips_reverse_chain_construction() {
        let image = png(&[chunk(b"IDAT", &[1], true)]);
        let stage = stage_a(&image);
        let first_idat = stage.first_idat.unwrap();
        let (iend, _, directly_after_idat) = find_connected_iend(&image, stage.stop);

        assert!(directly_after_idat);
        assert!(reverse_chain_if_needed(&image, first_idat, iend, directly_after_idat).is_none());
    }

    #[test]
    fn fixture_27_corrupt_post_idat_crc_escalates_but_keeps_metadata() {
        let idat = chunk(b"IDAT", &[1], true);
        let mut data = b"prompt\0found".to_vec();
        let broken = chunk(b"tEXt", &data, false);
        data.clear();
        let image = png(&[idat, broken]);
        let result = scan(&image, false);
        assert_eq!(
            result
                .records
                .iter()
                .map(|r| r.body.as_ref())
                .collect::<Vec<_>>(),
            ["found"]
        );
        assert_eq!(result.stats.stage_c_step, Some(1));
    }

    #[test]
    fn stage_b_partial_records_are_available_for_later_escalation() {
        let idat = chunk(b"IDAT", &[1], true);
        let profile = chunk(b"iCCP", &vec![0x35; 160 * 1024], true);
        let workflow = text(b"workflow", b"retained");
        let image = png(&[idat, profile, workflow]);
        let first_idat = 33;
        let iend = image.len() - 12;
        let progress = reverse_chain(&image, first_idat, iend, INITIAL_WINDOW);

        assert!(!progress.reached_idat);
        let records = progress.partial_records(&image);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record.body, "retained");
    }

    #[test]
    fn reverse_candidates_are_indexed_by_chunk_end() {
        let idat = chunk(b"IDAT", &[1], true);
        let workflow = text(b"workflow", b"indexed");
        let profile = chunk(b"iCCP", &[0x35; 8], true);
        let image = png(&[idat, workflow.clone(), profile.clone()]);
        let first_idat = 33;
        let workflow_offset = first_idat + chunk(b"IDAT", &[1], true).len();
        let workflow_end = workflow_offset + workflow.len();
        let iend = image.len() - 12;

        let candidates = reverse_candidates(&image, first_idat, iend);
        let workflow_candidates = candidates.by_end.get(&workflow_end).unwrap();

        assert!(workflow_candidates
            .iter()
            .any(|candidate| candidate.offset == workflow_offset));
        assert!(!candidates.by_end.contains_key(&(workflow_end + 1)));
    }

    #[test]
    fn fixtures_29_and_39_ztext_try_gzip_and_keep_non_utf8_latin1_success() {
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        gzip.write_all("compressed".as_bytes()).unwrap();
        let mut data = b"parameters\0\0".to_vec();
        data.extend_from_slice(&gzip.finish().unwrap());
        let ztxt = chunk(b"zTXt", &data, true);

        let mut latin = ZlibEncoder::new(Vec::new(), Compression::default());
        latin.write_all(&[b'c', b'a', b'f', 0xe9]).unwrap();
        let mut latin_data = b"prompt\0\0".to_vec();
        latin_data.extend_from_slice(&latin.finish().unwrap());
        let latin_ztxt = chunk(b"zTXt", &latin_data, true);
        assert_eq!(
            bodies(&png(&[ztxt, latin_ztxt]), false),
            ["compressed", "café"]
        );
    }

    #[test]
    fn fixture_2_ztext_accepts_the_standard_zlib_wrapper() {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"zlib text").unwrap();
        let mut data = b"parameters\0\0".to_vec();
        data.extend_from_slice(&encoder.finish().unwrap());
        assert_eq!(
            bodies(&png(&[chunk(b"zTXt", &data, true)]), false),
            ["zlib text"]
        );
    }

    #[test]
    fn fixture_30_decodes_utf16le_and_utf16be_boms() {
        let le = text(b"prompt", &[0xff, 0xfe, b'h', 0, b'i', 0]);
        let be = text(b"workflow", &[0xfe, 0xff, 0, b'h', 0, b'i']);
        assert_eq!(bodies(&png(&[le, be]), false), ["hi", "hi"]);
    }

    #[test]
    fn phase1_itxt_reads_fields_in_order_and_skips_incomplete_fields() {
        let valid = b"prompt\0\0\0en\0translated\0itxt body".to_vec();
        let itxt = chunk(b"iTXt", &valid, true);
        let malformed = chunk(b"iTXt", b"prompt\0", true);
        assert_eq!(bodies(&png(&[itxt, malformed]), false), ["itxt body"]);
    }

    #[test]
    fn phase1_itxt_decompresses_only_supported_methods() {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"compressed iTXt").unwrap();
        let mut valid = b"workflow\0\x01\0\0\0".to_vec();
        valid.extend_from_slice(&encoder.finish().unwrap());
        let compressed = chunk(b"iTXt", &valid, true);

        let mut invalid = b"prompt\0\x01\x01\0\0\0".to_vec();
        invalid.extend_from_slice(b"not decoded");
        let unsupported_method = chunk(b"iTXt", &invalid, true);
        assert_eq!(
            bodies(&png(&[compressed, unsupported_method]), false),
            ["compressed iTXt"]
        );
    }

    #[test]
    fn phase1_ztxt_skips_nonzero_compression_method() {
        let mut data = b"parameters\0\x01".to_vec();
        data.extend_from_slice(b"not a supported method");
        assert!(bodies(&png(&[chunk(b"zTXt", &data, true)]), false).is_empty());
    }

    #[test]
    fn fixture_28_trailing_keywords_are_referenced_under_the_trailing_label() {
        let idat = chunk(b"IDAT", &[1], true);
        let mut image = png(&[idat]);
        image.extend_from_slice(b"workflow=trailing\0");
        let result = scan(&image, false);
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].label, "Trailing Data (after IEND)");
        assert_eq!(result.records[0].keyword.as_deref(), Some("workflow"));
        assert_eq!(result.records[0].body, "trailing");
    }

    #[test]
    fn trailing_records_follow_their_file_order_and_json_takes_precedence() {
        let image = png(&[chunk(b"IDAT", &[1], true)]);
        let mut with_keywords = image.clone();
        with_keywords.extend_from_slice(b"workflow=first prompt=second");
        let keywords = scan(&with_keywords, false);
        assert_eq!(
            keywords
                .records
                .iter()
                .map(|r| r.body.as_ref())
                .collect::<Vec<_>>(),
            ["first prompt=second", "second"]
        );

        let mut with_json = image;
        with_json.extend_from_slice(br#"{"prompt":"json payload"} prompt=tail"#);
        let json = scan(&with_json, false);
        assert_eq!(json.records.len(), 1);
        assert_eq!(json.records[0].keyword, None);
        assert_eq!(json.records[0].body, r#"{"prompt":"json payload"}"#);
    }

    #[test]
    fn fixture_38_uses_the_primary_iend_and_ignores_appended_png_keywords() {
        let idat = chunk(b"IDAT", &[1], true);
        let main_tail = text(b"workflow", b"main png");
        let mut image = png(&[idat, main_tail]);
        let appended = png(&[chunk(b"IDAT", &[2], true), text(b"prompt", b"appended png")]);
        let primary_iend = image.len() - 12;
        image.extend_from_slice(&appended);
        let result = scan(&image, false);
        assert_eq!(
            result
                .records
                .iter()
                .map(|r| r.body.as_ref())
                .collect::<Vec<_>>(),
            ["main png"]
        );
        assert_eq!(result.stats.iend_offset, Some(primary_iend));
    }

    #[test]
    fn fixture_25_malformed_lengths_and_random_bytes_do_not_panic() {
        let mut image = SIGNATURE.to_vec();
        image.extend_from_slice(&u32::MAX.to_be_bytes());
        image.extend_from_slice(b"tEXt");
        assert!(scan(&image, false).records.is_empty());
        assert!(scan(&image, true).records.is_empty());
        assert!(scan(&[], false).records.is_empty());
        assert!(scan(b"random bytes", true).records.is_empty());
    }

    #[test]
    fn fixture_13_preserves_records_before_a_truncated_chunk() {
        let before = text(b"parameters", b"kept");
        let mut image = SIGNATURE.to_vec();
        image.extend_from_slice(&chunk(b"IHDR", &[0; 13], true));
        image.extend_from_slice(&before);
        image.extend_from_slice(&u32::MAX.to_be_bytes());
        image.extend_from_slice(b"tEXt");
        assert_eq!(bodies(&image, false), ["kept"]);
        assert_eq!(bodies(&image, true), ["kept"]);
    }

    #[test]
    fn phase1_compression_helpers_accept_all_specified_wrappers() {
        let plain = b"workflow\0all wrappers";
        let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
        z.write_all(plain).unwrap();
        let mut g = GzEncoder::new(Vec::new(), Compression::default());
        g.write_all(plain).unwrap();
        let mut d = DeflateEncoder::new(Vec::new(), Compression::default());
        d.write_all(plain).unwrap();
        assert_eq!(decompress_text(&z.finish().unwrap()).unwrap(), plain);
        assert_eq!(decompress_text(&g.finish().unwrap()).unwrap(), plain);
        assert_eq!(decompress_text(&d.finish().unwrap()).unwrap(), plain);
    }
}
