use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::{borrow::Cow, collections::hash_map::DefaultHasher};

use crate::record::{MetadataSink, RecordView, Source};

#[derive(Default)]
pub struct TextSink<'a> {
    records: Vec<RecordView<'a>>,
}

impl<'a> TextSink<'a> {
    pub fn push_record(&mut self, record: RecordView<'a>) {
        self.records.push(record);
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn flush<W: Write>(&mut self, writer: &mut W) -> io::Result<()> {
        let mut merged: Vec<MergedRecord<'a>> = Vec::new();
        let mut candidates: HashMap<(usize, u64), Vec<usize>> = HashMap::new();
        for record in self.records.drain(..) {
            let mut hasher = DefaultHasher::new();
            record.body.hash(&mut hasher);
            let key = (record.body.len(), hasher.finish());
            let origin = RecordOrigin::from_record(&record);
            let match_index = candidates.get(&key).and_then(|indices| {
                indices.iter().copied().find(|&index| {
                    let existing = &merged[index];
                    existing.record.body.as_ref() == record.body.as_ref()
                        && existing.origins.iter().all(|existing_origin| {
                            existing_origin.source != origin.source
                                || existing_origin.detail != origin.detail
                        })
                })
            });
            if let Some(index) = match_index {
                merged[index].origins.push(origin);
            } else {
                let index = merged.len();
                merged.push(MergedRecord {
                    record,
                    origins: vec![origin],
                });
                candidates.entry(key).or_default().push(index);
            }
        }

        for item in merged {
            let record = item.record;
            writer.write_all(b"--- ")?;
            writer.write_all(record.label.as_bytes())?;
            writer.write_all(b" ---\n")?;
            writer.write_all(record.body.as_bytes())?;
            for origin in item.origins.iter().skip(1) {
                writer.write_all(b"\n(also in: ")?;
                writer.write_all(origin.display_name().as_bytes())?;
                writer.write_all(b")")?;
            }
            writer.write_all(b"\n")?;
        }
        Ok(())
    }
}

struct MergedRecord<'a> {
    record: RecordView<'a>,
    origins: Vec<RecordOrigin<'a>>,
}

struct RecordOrigin<'a> {
    source: Source,
    keyword: Option<Cow<'a, str>>,
    detail: Option<Cow<'a, str>>,
}

impl<'a> RecordOrigin<'a> {
    fn from_record(record: &RecordView<'a>) -> Self {
        Self {
            source: record.source,
            keyword: record.keyword.clone(),
            detail: record.detail.clone(),
        }
    }

    fn display_name(&self) -> String {
        let source = if self.source == Source::PngText {
            self.detail.as_deref().unwrap_or("png_text")
        } else {
            source_name(self.source)
        };
        match (&self.keyword, &self.detail, self.source) {
            (Some(keyword), _, _) => format!("{source} {keyword}"),
            (None, Some(detail), Source::PngText) => format!("{detail}"),
            _ => source.to_owned(),
        }
    }
}

fn source_name(source: Source) -> &'static str {
    match source {
        Source::PngText => "png_text",
        Source::PngExif => "png_exif",
        Source::PngXmp => "png_xmp",
        Source::PngStealth => "png_stealth",
        Source::JpegExif => "jpeg_exif",
        Source::JpegXmp => "jpeg_xmp",
        Source::JpegComment => "jpeg_comment",
        Source::JpegIptc => "jpeg_iptc",
        Source::WebpExif => "webp_exif",
        Source::WebpXmp => "webp_xmp",
        Source::TiffXmp => "tiff_xmp",
        Source::C2pa => "c2pa",
        Source::Trailing => "trailing",
    }
}

impl<'a> MetadataSink<'a> for TextSink<'a> {
    fn on_record(&mut self, record: &RecordView<'a>) {
        // Cow::Borrowed copies only the reference. Decoded/owned values stay owned.
        self.records.push(record.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::TextSink;
    use crate::record::{MetadataSink, RecordView, Source};

    #[test]
    fn text_sink_preserves_heading_body_and_line_breaks() {
        let record = RecordView {
            source: Source::PngText,
            label: Cow::Borrowed("Stable Diffusion (A1111)"),
            body: Cow::Borrowed("positive prompt\nNegative prompt: low quality"),
            keyword: Some(Cow::Borrowed("parameters")),
            detail: Some(Cow::Borrowed("tEXt")),
        };
        let mut sink = TextSink::default();
        sink.on_record(&record);

        assert_eq!(sink.len(), 1);
        assert!(matches!(sink.records[0].body, Cow::Borrowed(_)));

        let mut output = Vec::new();
        sink.flush(&mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "--- Stable Diffusion (A1111) ---\npositive prompt\nNegative prompt: low quality\n"
        );
        assert!(sink.is_empty());
    }

    #[test]
    fn text_sink_can_take_owned_records_without_cloning_the_body() {
        let record = RecordView {
            source: Source::PngText,
            label: Cow::Borrowed("workflow"),
            body: Cow::Owned("decoded body".to_owned()),
            keyword: Some(Cow::Borrowed("workflow")),
            detail: None,
        };
        let mut sink = TextSink::default();
        sink.push_record(record);
        assert!(matches!(sink.records[0].body, Cow::Owned(_)));
    }

    fn record(
        source: Source,
        label: &'static str,
        body: &'static str,
        keyword: Option<&'static str>,
        detail: Option<&'static str>,
    ) -> RecordView<'static> {
        RecordView {
            source,
            label: Cow::Borrowed(label),
            body: Cow::Borrowed(body),
            keyword: keyword.map(Cow::Borrowed),
            detail: detail.map(Cow::Borrowed),
        }
    }

    #[test]
    fn text_sink_deduplicates_cross_source_body_across_intervening_records() {
        let mut sink = TextSink::default();
        sink.push_record(record(
            Source::PngText,
            "Stable Diffusion (A1111)",
            "shared parameters",
            Some("parameters"),
            Some("tEXt"),
        ));
        sink.push_record(record(
            Source::PngText,
            "Description",
            "intervening record",
            Some("Description"),
            Some("tEXt"),
        ));
        sink.push_record(record(
            Source::PngExif,
            "EXIF UserComment",
            "shared parameters",
            Some("UserComment"),
            None,
        ));

        let mut output = Vec::new();
        sink.flush(&mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "--- Stable Diffusion (A1111) ---\nshared parameters\n(also in: png_exif UserComment)\n--- Description ---\nintervening record\n"
        );
    }

    #[test]
    fn text_sink_keeps_repeated_chunks_of_the_same_source_and_kind() {
        let mut sink = TextSink::default();
        for _ in 0..2 {
            sink.push_record(record(
                Source::PngText,
                "Stable Diffusion (A1111)",
                "same text",
                Some("parameters"),
                Some("tEXt"),
            ));
        }
        let mut output = Vec::new();
        sink.flush(&mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "--- Stable Diffusion (A1111) ---\nsame text\n--- Stable Diffusion (A1111) ---\nsame text\n"
        );
    }

    #[test]
    fn text_sink_names_png_chunk_type_when_merging_text_chunk_sources() {
        let mut sink = TextSink::default();
        sink.push_record(record(
            Source::PngText,
            "Stable Diffusion (A1111)",
            "same text",
            Some("parameters"),
            Some("zTXt"),
        ));
        sink.push_record(record(
            Source::PngText,
            "Stable Diffusion (A1111)",
            "same text",
            Some("parameters"),
            Some("tEXt"),
        ));
        let mut output = Vec::new();
        sink.flush(&mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "--- Stable Diffusion (A1111) ---\nsame text\n(also in: tEXt parameters)\n"
        );
    }
}
