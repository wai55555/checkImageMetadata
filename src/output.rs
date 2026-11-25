use std::io::{self, Write};

use crate::record::{MetadataSink, RecordView};

#[derive(Default)]
pub struct TextSink<'a> {
    records: Vec<RecordView<'a>>,
}

impl<'a> TextSink<'a> {
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn flush<W: Write>(&mut self, writer: &mut W) -> io::Result<()> {
        for record in self.records.drain(..) {
            writer.write_all(b"--- ")?;
            writer.write_all(record.label.as_bytes())?;
            writer.write_all(b" ---\n")?;
            writer.write_all(record.body.as_bytes())?;
            writer.write_all(b"\n")?;
        }
        Ok(())
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
}
