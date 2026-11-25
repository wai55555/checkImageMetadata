use std::borrow::Cow;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Source {
    PngText,
    PngExif,
    PngXmp,
    PngStealth,
    JpegExif,
    JpegXmp,
    JpegComment,
    JpegIptc,
    WebpExif,
    WebpXmp,
    TiffXmp,
    C2pa,
    Trailing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordView<'a> {
    pub source: Source,
    pub label: Cow<'a, str>,
    pub body: Cow<'a, str>,
    pub keyword: Option<Cow<'a, str>>,
    pub detail: Option<Cow<'a, str>>,
}

pub trait MetadataSink<'a> {
    fn on_record(&mut self, record: &RecordView<'a>);
}
