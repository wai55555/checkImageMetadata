//! Lightweight XMP/RDF value extraction. This intentionally scans the XML
//! packet without constructing a general-purpose XML tree.

use std::borrow::Cow;


use crate::record::{RecordView, Source};

/// REQ-EXT-12: XMP パケットのハード上限。資源保護のため維持する。上限超過は 0 件
/// （切り捨てて部分的に読むと構造検証ができず fail-open になるため）。
pub const XMP_PACKET_LIMIT: usize = 16 * 1024 * 1024;

const NS_DC: &str = "http://purl.org/dc/elements/1.1/";
const NS_EXIF: &str = "http://ns.adobe.com/exif/1.0/";
const NS_XMP: &str = "http://ns.adobe.com/xap/1.0/";
const NS_XMP_NOTE: &str = "http://ns.adobe.com/xap/1.0/note/";
const NS_IPTC_EXT: &str = "http://iptc.org/std/Iptc4xmpExt/2008-02-29/";
const NS_RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const NS_XML: &str = "http://www.w3.org/XML/1998/namespace";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExpandedName<'a> {
    namespace: Option<&'a str>,
    local: &'a str,
}

#[derive(Clone, Copy, Debug)]
struct Attribute<'a> {
    name: ExpandedName<'a>,
    value: &'a str,
}

#[derive(Clone, Debug)]
enum Event<'a> {
    Start {
        name: ExpandedName<'a>,
        attributes: Vec<Attribute<'a>>,
        empty: bool,
    },
    End,
    Text {
        value: &'a str,
        cdata: bool,
    },
}

#[derive(Clone, Copy)]
struct RawAttribute<'a> {
    name: &'a str,
    value: &'a str,
}

#[derive(Clone, Copy)]
struct NamespaceFrame<'a> {
    raw_name: &'a str,
    old_len: usize,
}

#[derive(Clone, Copy, Debug)]
enum Field {
    Description,
    Title,
    UserComment,
    CreatorTool,
    HasExtendedXmp,
    DigitalSourceType,
    AiPromptInformation,
}

const FIELD_COUNT: usize = 7;

impl Field {
    const ALL: [Self; FIELD_COUNT] = [
        Self::Description,
        Self::Title,
        Self::UserComment,
        Self::CreatorTool,
        Self::HasExtendedXmp,
        Self::DigitalSourceType,
        Self::AiPromptInformation,
    ];

    fn index(self) -> usize {
        match self {
            Self::Description => 0,
            Self::Title => 1,
            Self::UserComment => 2,
            Self::CreatorTool => 3,
            Self::HasExtendedXmp => 4,
            Self::DigitalSourceType => 5,
            Self::AiPromptInformation => 6,
        }
    }

    fn matches(self, name: ExpandedName<'_>) -> bool {
        let (namespace, local) = match self {
            Self::Description => (NS_DC, "description"),
            Self::Title => (NS_DC, "title"),
            Self::UserComment => (NS_EXIF, "UserComment"),
            Self::CreatorTool => (NS_XMP, "CreatorTool"),
            Self::HasExtendedXmp => (NS_XMP_NOTE, "HasExtendedXMP"),
            Self::DigitalSourceType => (NS_IPTC_EXT, "DigitalSourceType"),
            Self::AiPromptInformation => (NS_IPTC_EXT, "AIPromptInformation"),
        };
        name.namespace == Some(namespace) && name.local == local
    }

    fn local(self) -> &'static str {
        match self {
            Self::Description => "description",
            Self::Title => "title",
            Self::UserComment => "UserComment",
            Self::CreatorTool => "CreatorTool",
            Self::HasExtendedXmp => "HasExtendedXMP",
            Self::DigitalSourceType => "DigitalSourceType",
            Self::AiPromptInformation => "AIPromptInformation",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Description => "XMP Description",
            Self::Title => "XMP Title",
            Self::UserComment => "XMP UserComment",
            Self::CreatorTool => "XMP CreatorTool",
            Self::HasExtendedXmp => "",
            Self::DigitalSourceType => "XMP DigitalSourceType",
            Self::AiPromptInformation => "XMP AIPromptInformation",
        }
    }
}

struct Candidate<'a> {
    value: Cow<'a, str>,
    offset: usize,
}

#[derive(Default)]
struct FieldCandidates<'a> {
    element: Option<Candidate<'a>>,
    attribute: Option<Candidate<'a>>,
    resource: Option<Candidate<'a>>,
    // REQ-EXT-04(a): 各候補が「主対象」（rdf:about="" の rdf:Description 由来）か。
    element_main: Option<bool>,
    attribute_main: Option<bool>,
    resource_main: Option<bool>,
}

#[derive(Default)]
struct XmpValues<'a> {
    fields: [FieldCandidates<'a>; FIELD_COUNT],
}

#[derive(Default)]
struct TextBuffer<'a> {
    borrowed: Option<&'a str>,
    owned: Option<String>,
}

impl<'a> TextBuffer<'a> {
    fn push(&mut self, text: &'a str, cdata: bool) {
        let decoded = if cdata {
            Cow::Borrowed(text)
        } else {
            decode_entities(text)
        };
        if let Some(owned) = self.owned.as_mut() {
            owned.push_str(decoded.as_ref());
            return;
        }
        match decoded {
            Cow::Borrowed(part) if self.borrowed.is_none() => self.borrowed = Some(part),
            part => {
                let mut owned = String::new();
                if let Some(previous) = self.borrowed.take() {
                    owned.push_str(previous);
                }
                owned.push_str(part.as_ref());
                self.owned = Some(owned);
            }
        }
    }

    fn finish(self) -> Cow<'a, str> {
        if let Some(value) = self.owned {
            Cow::Owned(value)
        } else {
            Cow::Borrowed(self.borrowed.unwrap_or(""))
        }
    }
}

struct ListItem<'a> {
    depth: usize,
    is_default: bool,
    text: TextBuffer<'a>,
}

#[derive(Default)]
struct AltValue<'a> {
    first: Option<Cow<'a, str>>,
    default: Option<Cow<'a, str>>,
}

pub struct ParsedPacket<'a> {
    pub records: Vec<RecordView<'a>>,
    pub has_extended_xmp: Option<String>,
}

pub fn parse<'a>(bytes: &'a [u8], source: Source) -> Vec<RecordView<'a>> {
    parse_packet(bytes, source).records
}

pub fn parse_owned(bytes: Vec<u8>, source: Source) -> Vec<RecordView<'static>> {
    parse_packet(&bytes, source)
        .records
        .into_iter()
        .map(|record| RecordView {
            source: record.source,
            label: Cow::Owned(record.label.into_owned()),
            body: Cow::Owned(record.body.into_owned()),
            keyword: record
                .keyword
                .map(|keyword| Cow::Owned(keyword.into_owned())),
            detail: record.detail.map(|detail| Cow::Owned(detail.into_owned())),
        })
        .collect()
}

fn empty_packet<'a>() -> ParsedPacket<'a> {
    ParsedPacket {
        records: Vec::new(),
        has_extended_xmp: None,
    }
}

pub fn parse_packet<'a>(bytes: &'a [u8], source: Source) -> ParsedPacket<'a> {
    if bytes.len() > XMP_PACKET_LIMIT {
        return empty_packet();
    }
    // REQ-EXT-15: UTF-32（BOM は UTF-16LE と共通の FF FE 00 00 で始まる）は対象外。
    if bytes.starts_with(&[0xff, 0xfe, 0x00, 0x00]) || bytes.starts_with(&[0x00, 0x00, 0xfe, 0xff])
    {
        return empty_packet();
    }
    if let Some((little_endian, body)) = utf16_bom(bytes) {
        // 借用できないため、デコードした所有文字列から値をすべて所有化して返す。
        let Some(xml) = decode_utf16(body, little_endian) else {
            return empty_packet();
        };
        let packet = build_packet(&xml, source, true);
        return ParsedPacket {
            records: packet
                .records
                .into_iter()
                .map(|record| RecordView {
                    source: record.source,
                    label: Cow::Owned(record.label.into_owned()),
                    body: Cow::Owned(record.body.into_owned()),
                    keyword: record.keyword.map(|keyword| Cow::Owned(keyword.into_owned())),
                    detail: record.detail.map(|detail| Cow::Owned(detail.into_owned())),
                })
                .collect(),
            has_extended_xmp: packet.has_extended_xmp,
        };
    }
    let Ok(xml) = std::str::from_utf8(bytes) else {
        return empty_packet();
    };
    build_packet(xml, source, false)
}

fn utf16_bom(bytes: &[u8]) -> Option<(bool, &[u8])> {
    match bytes.get(..2)? {
        [0xff, 0xfe] => Some((true, bytes.get(2..)?)),
        [0xfe, 0xff] => Some((false, bytes.get(2..)?)),
        _ => None,
    }
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

/// `own` が true のとき、値は入力文字列からコピーして所有化する（UTF-16 経路用）。
fn build_packet<'a>(xml: &'a str, source: Source, own: bool) -> ParsedPacket<'a> {
    let events = tokenize(xml);
    let values = collect_values(&events);
    let selected: Vec<_> = Field::ALL
        .into_iter()
        .filter_map(|field| {
            let candidates = &values.fields[field.index()];
            candidates
                .element
                .as_ref()
                .or(candidates.attribute.as_ref())
                .or(candidates.resource.as_ref())
                .map(|candidate| (field, candidate))
        })
        .collect();

    // REQ-EXT-07: 抽出層は本文を合成・置換しない。Draw Things の判定は sorter 側の
    // 値検証（REQ-CLS-07）へ移し、ここでは生の JSON 本文をそのままレコード化する。
    // dc:description を抑制する副作用も廃止する。
    let mut has_extended_xmp = None;
    for (field, candidate) in &selected {
        if candidate.value.is_empty() {
            continue;
        }
        if matches!(field, Field::HasExtendedXmp) {
            has_extended_xmp = Some(candidate.value.to_string());
        }
    }

    let mut records_with_position = Vec::new();
    for (field, candidate) in selected {
        if matches!(field, Field::HasExtendedXmp) || candidate.value.is_empty() {
            continue;
        }
        let body = if own {
            Cow::Owned(candidate.value.to_string())
        } else {
            candidate.value.clone()
        };
        records_with_position.push((
            candidate.offset,
            RecordView {
                source,
                label: Cow::Borrowed(field.label()),
                body,
                keyword: Some(Cow::Borrowed(field.local())),
                detail: None,
            },
        ));
    }
    records_with_position.sort_by_key(|(offset, _)| *offset);

    ParsedPacket {
        records: records_with_position
            .into_iter()
            .map(|(_, record)| record)
            .collect(),
        has_extended_xmp,
    }
}

fn collect_values<'a>(events: &[Event<'a>]) -> XmpValues<'a> {
    let mut values = XmpValues::default();
    let mut element_covered_until: [Option<usize>; FIELD_COUNT] = [None; FIELD_COUNT];
    // REQ-EXT-04(a): rdf:Description の入れ子を追い、その配下のフィールドが
    // 「主対象」（rdf:about=""）由来かを判定する。
    let mut depth = 0usize;
    let mut descriptions: Vec<(usize, bool)> = Vec::new();
    for (index, event) in events.iter().enumerate() {
        match event {
            Event::Start { name, attributes, empty } => {
                if !*empty {
                    depth += 1;
                }
                if *name
                    == (ExpandedName {
                        namespace: Some(NS_RDF),
                        local: "Description",
                    })
                {
                    let is_main = attributes.iter().any(|attribute| {
                        attribute.name
                            == (ExpandedName {
                                namespace: Some(NS_RDF),
                                local: "about",
                            })
                            && attribute.value.trim().is_empty()
                    });
                    descriptions.push((depth, is_main));
                    for attribute in attributes {
                        let Some(field) = Field::ALL
                            .into_iter()
                            .find(|field| field.matches(attribute.name))
                        else {
                            continue;
                        };
                        store_candidate(
                            &mut values.fields[field.index()].attribute,
                            &mut values.fields[field.index()].attribute_main,
                            Candidate {
                                value: decode_entities(attribute.value),
                                offset: index,
                            },
                            is_main,
                        );
                    }
                }
            }
            Event::End => {
                while descriptions.last().is_some_and(|(at, _)| *at >= depth) {
                    descriptions.pop();
                }
                depth = depth.saturating_sub(1);
            }
            Event::Text { .. } => {}
        }

        let Event::Start {
            name,
            attributes,
            empty,
        } = event
        else {
            continue;
        };
        let in_main_subject = descriptions.last().is_some_and(|(_, is_main)| *is_main);

        let Some(field) = Field::ALL.into_iter().find(|field| field.matches(*name)) else {
            continue;
        };
        if let Some(resource) = attributes.iter().find(|attribute| {
            attribute.name
                == (ExpandedName {
                    namespace: Some(NS_RDF),
                    local: "resource",
                })
        }) {
            store_candidate(
                &mut values.fields[field.index()].resource,
                &mut values.fields[field.index()].resource_main,
                Candidate {
                    value: decode_entities(resource.value),
                    offset: index,
                },
                in_main_subject,
            );
        }
        let field_index = field.index();
        let not_covered = element_covered_until[field_index].map_or(true, |end| index > end);
        // REQ-EXT-04(a): 主対象由来の候補は、既に非主対象の値が入っていても拾い直す
        // （同じ優先度の中で最初の候補が勝つ、という規則は store_candidate が守る）。
        let need_element = values.fields[field_index].element.is_none()
            || (in_main_subject && values.fields[field_index].element_main == Some(false));
        if !*empty && need_element && not_covered {
            let (value, end_index) = extract_element(events, index);
            element_covered_until[field_index] = Some(end_index);
            if let Some(value) = value {
                store_candidate(
                    &mut values.fields[field_index].element,
                    &mut values.fields[field_index].element_main,
                    Candidate {
                        value,
                        offset: index,
                    },
                    in_main_subject,
                );
            }
        }
    }
    values
}

/// REQ-EXT-04(a): 主対象（rdf:about=""）由来の候補を優先し、無い場合のみ従来どおり
/// 先勝ちにする。同じ優先度の中では最初の候補を採る。
fn store_candidate<'a>(
    slot: &mut Option<Candidate<'a>>,
    main_slot: &mut Option<bool>,
    candidate: Candidate<'a>,
    is_main: bool,
) {
    if candidate.value.is_empty() {
        return;
    }
    match *main_slot {
        None => {
            *slot = Some(candidate);
            *main_slot = Some(is_main);
        }
        Some(false) if is_main => {
            *slot = Some(candidate);
            *main_slot = Some(true);
        }
        _ => {}
    }
}

fn extract_element<'a>(events: &[Event<'a>], start_index: usize) -> (Option<Cow<'a, str>>, usize) {
    let Some(Event::Start { empty: false, .. }) = events.get(start_index) else {
        return (None, start_index);
    };

    let mut depth = 0usize;
    let mut plain = TextBuffer::default();
    let mut alt_seen = false;
    let mut alt_depth = None;
    let mut item: Option<ListItem<'a>> = None;
    let mut alt = AltValue::default();
    let mut end_index = events.len();

    for (event_index, event) in events.iter().enumerate().skip(start_index + 1) {
        match event {
            Event::Start {
                name,
                attributes,
                empty,
                ..
            } => {
                depth += 1;
                if name.namespace == Some(NS_RDF) && name.local == "Alt" {
                    alt_seen = true;
                    alt_depth.get_or_insert(depth);
                }
                if alt_depth.is_some()
                    && depth == alt_depth.unwrap_or(usize::MAX) + 1
                    && name.namespace == Some(NS_RDF)
                    && name.local == "li"
                    && item.is_none()
                {
                    let is_default = attributes.iter().any(|attribute| {
                        attribute.name
                            == (ExpandedName {
                                namespace: Some(NS_XML),
                                local: "lang",
                            })
                            && attribute.value.eq_ignore_ascii_case("x-default")
                    });
                    item = Some(ListItem {
                        depth,
                        is_default,
                        text: TextBuffer::default(),
                    });
                }
                if *empty {
                    if item.as_ref().is_some_and(|li| li.depth == depth) {
                        finish_list_item(&mut item, &mut alt);
                    }
                    if alt_depth == Some(depth) {
                        alt_depth = None;
                    }
                    depth -= 1;
                }
            }
            Event::End { .. } => {
                if item.as_ref().is_some_and(|li| li.depth == depth) {
                    finish_list_item(&mut item, &mut alt);
                }
                if alt_depth == Some(depth) {
                    alt_depth = None;
                }
                if depth == 0 {
                    // The end tag for this property was reached.
                    end_index = event_index;
                    break;
                }
                depth -= 1;
            }
            Event::Text { value, cdata } => {
                if let Some(li) = item.as_mut() {
                    li.text.push(value, *cdata);
                } else {
                    plain.push(value, *cdata);
                }
            }
        }
    }

    let value = if alt_seen {
        alt.default.or(alt.first)
    } else {
        Some(plain.finish())
    };
    (value, end_index)
}

fn finish_list_item<'a>(item: &mut Option<ListItem<'a>>, alt: &mut AltValue<'a>) {
    let Some(item) = item.take() else {
        return;
    };
    let text = item.text.finish();
    if !text.is_empty() && alt.first.is_none() {
        alt.first = Some(text.clone());
    }
    if item.is_default && !text.is_empty() && alt.default.is_none() {
        alt.default = Some(text);
    }
}

fn decode_entities(input: &str) -> Cow<'_, str> {
    let bytes = input.as_bytes();
    let mut cursor = 0usize;
    let mut copied = 0usize;
    let mut output = String::new();
    while let Some(relative) = bytes
        .get(cursor..)
        .and_then(|rest| rest.iter().position(|&byte| byte == b'&'))
    {
        let amp = cursor + relative;
        let Some(semi_rel) = bytes[amp + 1..].iter().position(|&byte| byte == b';') else {
            break;
        };
        let semi = amp + 1 + semi_rel;
        let entity = &input[amp + 1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => decode_numeric_entity(entity),
        };
        let Some(decoded) = decoded else {
            cursor = semi + 1;
            continue;
        };
        output.push_str(&input[copied..amp]);
        output.push(decoded);
        copied = semi + 1;
        cursor = copied;
    }
    if copied == 0 {
        Cow::Borrowed(input)
    } else {
        output.push_str(&input[copied..]);
        Cow::Owned(output)
    }
}

fn decode_numeric_entity(entity: &str) -> Option<char> {
    let number = if let Some(hex) = entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"))
    {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        u32::from_str_radix(entity.strip_prefix('#')?, 10).ok()?
    };
    char::from_u32(number)
}

/// REQ-EXT-11: 局所破損があっても、破損箇所より前のイベントは返す（all-or-nothing をやめる）。
/// 再同期できない範囲（終端の無いコメント／CDATA／タグ等）はそこで打ち切る。
fn tokenize<'a>(xml: &'a str) -> Vec<Event<'a>> {
    let bytes = xml.as_bytes();
    let mut events = Vec::new();
    let mut namespaces = vec![("xml", NS_XML)];
    let mut frames: Vec<NamespaceFrame<'a>> = Vec::new();
    let mut cursor = 0usize;

    while cursor < bytes.len() {
        if bytes[cursor] != b'<' {
            let relative_end = bytes[cursor..].iter().position(|&byte| byte == b'<');
            let end = relative_end.map_or(bytes.len(), |relative| cursor + relative);
            let Ok(value) = std::str::from_utf8(&bytes[cursor..end]) else {
                break;
            };
            events.push(Event::Text { value, cdata: false });
            cursor = end;
            continue;
        }
        if bytes[cursor..].starts_with(b"<!--") {
            let Some(end) = find_bytes(bytes, cursor + 4, b"-->") else {
                break;
            };
            cursor = end + 3;
            continue;
        }
        if bytes[cursor..].starts_with(b"<![CDATA[") {
            let content_start = cursor + 9;
            let Some(end) = find_bytes(bytes, content_start, b"]]>") else {
                break;
            };
            let Ok(value) = std::str::from_utf8(&bytes[content_start..end]) else {
                break;
            };
            events.push(Event::Text { value, cdata: true });
            cursor = end + 3;
            continue;
        }
        if bytes[cursor..].starts_with(b"<?") {
            let Some(end) = find_bytes(bytes, cursor + 2, b"?>") else {
                break;
            };
            cursor = end + 2;
            continue;
        }
        if bytes[cursor..].starts_with(b"<!") {
            let Some(relative) = bytes[cursor..].iter().position(|&byte| byte == b'>') else {
                break;
            };
            cursor += relative + 1;
            continue;
        }

        let Some(end) = find_tag_end(bytes, cursor + 1) else {
            break;
        };
        let Ok(inside) = std::str::from_utf8(&bytes[cursor + 1..end]) else {
            break;
        };
        if let Some(close) = inside.strip_prefix('/') {
            let Some(raw_name) = first_name(close.trim_start()) else {
                break;
            };
            events.push(Event::End);
            if let Some(frame_index) = frames.iter().rposition(|frame| frame.raw_name == raw_name) {
                let frame = frames.remove(frame_index);
                namespaces.truncate(frame.old_len);
                frames.truncate(frame_index);
            }
        } else {
            let empty = inside.trim_end().ends_with('/');
            let content = if empty {
                let Some(stripped) = inside.trim_end().strip_suffix('/') else {
                    break;
                };
                stripped
            } else {
                inside
            };
            let Some((raw_name, raw_attributes)) = parse_start_tag(content) else {
                break;
            };
            let old_len = namespaces.len();
            for attribute in &raw_attributes {
                if let Some(prefix) = attribute.name.strip_prefix("xmlns:") {
                    namespaces.push((prefix, attribute.value));
                } else if attribute.name == "xmlns" {
                    namespaces.push(("", attribute.value));
                }
            }
            let name = resolve_name(raw_name, &namespaces, true);
            let attributes = raw_attributes
                .iter()
                .filter(|attribute| {
                    attribute.name != "xmlns" && !attribute.name.starts_with("xmlns:")
                })
                .map(|attribute| Attribute {
                    name: resolve_name(attribute.name, &namespaces, false),
                    value: attribute.value,
                })
                .collect();
            events.push(Event::Start {
                name,
                attributes,
                empty,
            });
            if empty {
                namespaces.truncate(old_len);
            } else {
                frames.push(NamespaceFrame { raw_name, old_len });
            }
        }
        cursor = end + 1;
    }
    events
}

fn find_bytes(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

fn find_tag_end(bytes: &[u8], mut cursor: usize) -> Option<usize> {
    let mut quote = None;
    while let Some(&byte) = bytes.get(cursor) {
        match (quote, byte) {
            (Some(active), current) if active == current => quote = None,
            (Some(_), _) => {}
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => return Some(cursor),
            _ => {}
        }
        cursor += 1;
    }
    None
}

fn parse_start_tag<'a>(input: &'a str) -> Option<(&'a str, Vec<RawAttribute<'a>>)> {
    let bytes = input.as_bytes();
    let mut cursor = 0usize;
    skip_space(bytes, &mut cursor);
    let name_start = cursor;
    while let Some(&byte) = bytes.get(cursor) {
        if is_space(byte) || byte == b'=' || byte == b'/' {
            break;
        }
        cursor += 1;
    }
    let name = input.get(name_start..cursor)?;
    if name.is_empty() {
        return None;
    }
    let mut attributes = Vec::new();
    loop {
        skip_space(bytes, &mut cursor);
        if cursor >= bytes.len() || bytes[cursor] == b'/' {
            break;
        }
        let attr_start = cursor;
        while let Some(&byte) = bytes.get(cursor) {
            if is_space(byte) || byte == b'=' || byte == b'/' {
                break;
            }
            cursor += 1;
        }
        let attr_name = input.get(attr_start..cursor)?;
        if attr_name.is_empty() {
            return None;
        }
        skip_space(bytes, &mut cursor);
        if bytes.get(cursor) != Some(&b'=') {
            return None;
        }
        cursor += 1;
        skip_space(bytes, &mut cursor);
        let quote = *bytes.get(cursor)?;
        if quote != b'\'' && quote != b'"' {
            return None;
        }
        cursor += 1;
        let value_start = cursor;
        while bytes.get(cursor).is_some_and(|&byte| byte != quote) {
            cursor += 1;
        }
        let value = input.get(value_start..cursor)?;
        cursor = cursor.checked_add(1)?;
        attributes.push(RawAttribute {
            name: attr_name,
            value,
        });
    }
    Some((name, attributes))
}

fn first_name(input: &str) -> Option<&str> {
    input
        .split(|character: char| character.is_ascii_whitespace())
        .next()
        .filter(|name| !name.is_empty())
}

fn skip_space(bytes: &[u8], cursor: &mut usize) {
    while bytes.get(*cursor).is_some_and(|&byte| is_space(byte)) {
        *cursor += 1;
    }
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn resolve_name<'a>(
    raw: &'a str,
    namespaces: &[(&'a str, &'a str)],
    element: bool,
) -> ExpandedName<'a> {
    let (prefix, local) = raw.split_once(':').unwrap_or(("", raw));
    let namespace = if prefix.is_empty() && !element {
        None
    } else {
        namespaces
            .iter()
            .rev()
            .find_map(|(known_prefix, uri)| (*known_prefix == prefix).then_some(*uri))
    };
    ExpandedName { namespace, local }
}

#[cfg(test)]
mod tests {
    use crate::record::Source;

    use super::parse_packet;

    fn rdf(content: &str) -> String {
        format!(
            "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'>{content}</rdf:RDF></x:xmpmeta>"
        )
    }

    #[test]
    fn xmp_matches_namespace_uris_in_element_form_and_ignores_unknown_namespaces() {
        let packet = rdf(
            "<rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/' xmlns:other='urn:other'><d:description>description</d:description><other:title>ignore</other:title></rdf:Description>",
        );
        let parsed = parse_packet(packet.as_bytes(), Source::JpegXmp);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].label, "XMP Description");
        assert_eq!(parsed.records[0].body, "description");
    }

    #[test]
    fn xmp_extracts_attribute_and_resource_forms() {
        let packet = rdf(
            "<rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/' xmlns:i='http://iptc.org/std/Iptc4xmpExt/2008-02-29/' d:title='attribute title'><i:DigitalSourceType rdf:resource='https://example.test/trainedAlgorithmicMedia'/></rdf:Description>",
        );
        let parsed = parse_packet(packet.as_bytes(), Source::WebpXmp);
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.records[0].label, "XMP Title");
        assert_eq!(parsed.records[0].body, "attribute title");
        assert_eq!(parsed.records[1].label, "XMP DigitalSourceType");
        assert_eq!(
            parsed.records[1].body,
            "https://example.test/trainedAlgorithmicMedia"
        );
    }

    #[test]
    fn xmp_creator_tool_and_digital_source_coexist_with_primary_fields() {
        let packet = rdf(
            "<rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/' xmlns:x='http://ns.adobe.com/xap/1.0/' xmlns:i='http://iptc.org/std/Iptc4xmpExt/2008-02-29/' d:title='title' x:CreatorTool='Draw Things'><i:DigitalSourceType rdf:resource='https://example.test/trainedAlgorithmicMedia'/></rdf:Description>",
        );
        let parsed = parse_packet(packet.as_bytes(), Source::JpegXmp);
        assert_eq!(parsed.records.len(), 3);
        assert_eq!(parsed.records[0].label, "XMP Title");
        assert_eq!(parsed.records[1].label, "XMP CreatorTool");
        assert_eq!(parsed.records[2].label, "XMP DigitalSourceType");
    }

    #[test]
    fn xmp_element_form_precedes_attributes_and_duplicate_descriptions() {
        let packet = rdf(
            "<rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/' d:description='attribute'><d:description>element</d:description></rdf:Description><rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/'><d:description>later</d:description></rdf:Description>",
        );
        let parsed = parse_packet(packet.as_bytes(), Source::JpegXmp);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].body, "element");
    }

    #[test]
    fn xmp_rdf_alt_prefers_x_default_otherwise_first_li() {
        let default_packet = rdf(
            "<rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/'><d:title><rdf:Alt><rdf:li xml:lang='fr'>français</rdf:li><rdf:li xml:lang='x-default'>default</rdf:li></rdf:Alt></d:title></rdf:Description>",
        );
        let first_packet = rdf(
            "<rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/'><d:title><rdf:Alt><rdf:li xml:lang='fr'>premier</rdf:li><rdf:li xml:lang='ja'>二番目</rdf:li></rdf:Alt></d:title></rdf:Description>",
        );
        assert_eq!(
            parse_packet(default_packet.as_bytes(), Source::JpegXmp).records[0].body,
            "default"
        );
        assert_eq!(
            parse_packet(first_packet.as_bytes(), Source::JpegXmp).records[0].body,
            "premier"
        );
    }

    #[test]
    fn xmp_rdf_alt_ignores_an_empty_x_default_item() {
        let packet = rdf(
            "<rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/'><d:title><rdf:Alt><rdf:li xml:lang='x-default'/><rdf:li xml:lang='fr'>premier non vide</rdf:li></rdf:Alt></d:title></rdf:Description>",
        );
        let parsed = parse_packet(packet.as_bytes(), Source::JpegXmp);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].body, "premier non vide");
    }

    #[test]
    fn nested_repeated_properties_do_not_replace_or_rescan_the_outer_value() {
        let mut properties =
            String::from("<rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/'>");
        properties.push_str("<d:description>outer");
        for _ in 0..512 {
            properties.push_str("<d:description>");
        }
        properties.push_str("inner");
        for _ in 0..512 {
            properties.push_str("</d:description>");
        }
        properties.push_str("tail</d:description></rdf:Description>");
        let packet = rdf(&properties);
        let parsed = parse_packet(packet.as_bytes(), Source::JpegXmp);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].body, "outerinnertail");
    }

    #[test]
    fn xmp_decodes_entities_and_cdata() {
        let packet = rdf(
            "<rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/'><d:description>A&amp;B &lt; &#65; &#x1F600; <![CDATA[<raw>&copy;]]></d:description></rdf:Description>",
        );
        let parsed = parse_packet(packet.as_bytes(), Source::JpegXmp);
        assert_eq!(parsed.records[0].body, "A&B < A 😀 <raw>&copy;");
    }

    /// T-17（REQ-EXT-07）: Draw Things 風 JSON でも本文を合成せず、dc:description も
    /// 抑制しない。判定は sorter 側（REQ-CLS-07）に移した。
    #[test]
    fn t17_draw_things_json_is_not_synthesized_nor_suppresses_description() {
        let packet = rdf(
            "<rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/' xmlns:e='http://ns.adobe.com/exif/1.0/'><d:description>duplicate prompt</d:description><e:UserComment>{&quot;c&quot;:&quot;cat&quot;,&quot;uc&quot;:&quot;bad&quot;}</e:UserComment></rdf:Description>",
        );
        let parsed = parse_packet(packet.as_bytes(), Source::PngXmp);
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.records[0].label, "XMP Description");
        assert_eq!(parsed.records[0].body, "duplicate prompt");
        assert_eq!(parsed.records[1].label, "XMP UserComment");
        assert_eq!(parsed.records[1].body, "{\"c\":\"cat\",\"uc\":\"bad\"}");
        assert_eq!(
            parsed.records[1].keyword.as_deref(),
            Some("UserComment"),
            "sorter 側の値検証に生 JSON を渡す"
        );
    }

    /// `"c"` 単独（Draw Things の固有キー集合を満たさない）でも本文は素通しする。
    #[test]
    fn draw_things_c_only_json_is_passed_through_as_is() {
        let packet = rdf(
            "<rdf:Description xmlns:e='http://ns.adobe.com/exif/1.0/'><e:UserComment> \n\t{&quot;c&quot;:&quot;cat&quot;}</e:UserComment></rdf:Description>",
        );
        let parsed = parse_packet(packet.as_bytes(), Source::PngXmp);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].label, "XMP UserComment");
        assert_eq!(parsed.records[0].body, " \n\t{\"c\":\"cat\"}");
    }

    #[test]
    fn xmp_extracts_extended_guid_and_iptc_prompt() {
        let packet = rdf(
            "<rdf:Description xmlns:n='http://ns.adobe.com/xap/1.0/note/' xmlns:i='http://iptc.org/std/Iptc4xmpExt/2008-02-29/' n:HasExtendedXMP='0123456789abcdef0123456789ABCDEF'><i:AIPromptInformation><![CDATA[draw a tree]]></i:AIPromptInformation></rdf:Description>",
        );
        let parsed = parse_packet(packet.as_bytes(), Source::JpegXmp);
        assert_eq!(
            parsed.has_extended_xmp.as_deref(),
            Some("0123456789abcdef0123456789ABCDEF")
        );
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].label, "XMP AIPromptInformation");
        assert_eq!(parsed.records[0].body, "draw a tree");
    }

    /// T-37（REQ-EXT-04(a)）: 複数の rdf:Description がある場合、主対象
    /// （rdf:about=""）のフィールドを優先する。主対象に無いフィールドは従来どおり先勝ち。
    #[test]
    fn t37_main_subject_description_is_preferred_per_field() {
        let packet = rdf(
            "<rdf:Description rdf:about='other' xmlns:d='http://purl.org/dc/elements/1.1/' xmlns:x='http://ns.adobe.com/xap/1.0/'><d:description>secondary</d:description><x:CreatorTool>OtherTool</x:CreatorTool></rdf:Description><rdf:Description rdf:about='' xmlns:d='http://purl.org/dc/elements/1.1/'><d:description>main subject</d:description></rdf:Description>",
        );
        let parsed = parse_packet(packet.as_bytes(), Source::JpegXmp);
        let description = parsed
            .records
            .iter()
            .find(|record| record.keyword.as_deref() == Some("description"))
            .expect("description record");
        assert_eq!(description.body, "main subject");
        // 主対象に無いフィールドは先勝ちのまま残る。
        let creator = parsed
            .records
            .iter()
            .find(|record| record.keyword.as_deref() == Some("CreatorTool"))
            .expect("CreatorTool record");
        assert_eq!(creator.body, "OtherTool");
    }

    /// T-30（REQ-EXT-12）: 16MB を超える XMP パケットは 0 件（資源保護のための仕様）。
    #[test]
    fn t30_xmp_packet_over_the_16mb_limit_yields_no_records() {
        let mut packet = vec![b' '; super::XMP_PACKET_LIMIT + 1];
        packet[..5].copy_from_slice(b"<?xml");
        assert!(parse_packet(&packet, Source::PngXmp).records.is_empty());
    }

    /// T-29（REQ-EXT-11）: 局所破損（終端の無いコメント）があっても、
    /// 破損箇所より前の正常なフィールドはレコード化する。
    #[test]
    fn t29_local_corruption_keeps_fields_before_the_damage() {
        let packet = rdf(
            "<rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/'><d:description>before damage</d:description></rdf:Description><!-- unterminated comment",
        );
        let parsed = parse_packet(packet.as_bytes(), Source::JpegXmp);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].body, "before damage");
    }

    /// T-33（REQ-EXT-15）: UTF-16（BOM 付き BE／LE）の XMP パケットも読める。
    /// UTF-32 は対象外（0 件）。
    #[test]
    fn t33_utf16_packets_are_read_and_utf32_is_not() {
        let packet = rdf(
            "<rdf:Description xmlns:d='http://purl.org/dc/elements/1.1/'><d:description>utf16 body</d:description></rdf:Description>",
        );
        for little_endian in [true, false] {
            let mut bytes = if little_endian {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            };
            for unit in packet.encode_utf16() {
                let encoded = if little_endian {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                };
                bytes.extend_from_slice(&encoded);
            }
            let parsed = parse_packet(&bytes, Source::PngXmp);
            assert_eq!(parsed.records.len(), 1, "le={little_endian}");
            assert_eq!(parsed.records[0].body, "utf16 body");
        }
        // UTF-32 LE/BE は対象外。
        let utf32_le: Vec<u8> = [0xff, 0xfe, 0x00, 0x00]
            .into_iter()
            .chain(packet.encode_utf16().flat_map(|unit| {
                let bytes = (unit as u32).to_le_bytes();
                bytes.into_iter()
            }))
            .collect();
        assert!(parse_packet(&utf32_le, Source::PngXmp).records.is_empty());
    }

    #[test]
    fn xmp_creator_tool_is_a_fallback_when_no_primary_field_exists() {
        let packet = rdf(
            "<rdf:Description xmlns:x='http://ns.adobe.com/xap/1.0/' x:CreatorTool='Draw Things'/>",
        );
        let parsed = parse_packet(packet.as_bytes(), Source::JpegXmp);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].label, "XMP CreatorTool");
        assert_eq!(parsed.records[0].body, "Draw Things");
    }
}
