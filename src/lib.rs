//! fast_meta のライブラリ API。
//!
//! 抽出結果は [`RecordSink`] へコールバックで渡される。レコードは入力ファイルの
//! mmap からの借用を含み得るため、借用はコールバック呼び出し中のみ有効である。

pub mod extract;
pub mod jpeg;
pub mod keywords;
pub mod output;
pub mod png;
pub mod record;
pub mod tiff;
pub mod xmp;

pub use extract::{
    extract_to_writer, scan_file, Format, RecordSink, ScanError, ScanOptions, ScanOutcome,
    StealthPolicy,
};
pub use record::{RecordView, Source};
