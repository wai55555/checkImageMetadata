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

/// REQ-API-01: JPEG COM が A1111 形式であることを示す構造化された印。
/// `RecordView.detail` に設定し、sorter は表示ラベルではなくこの値で判定する。
/// 「両マーカーあり」は中シグナル、「片方のみ」は弱シグナルとして扱う。
pub const JPEG_COM_A1111_BOTH_MARKERS: &str = "a1111-both-markers";
pub const JPEG_COM_A1111_ONE_MARKER: &str = "a1111-one-marker";
