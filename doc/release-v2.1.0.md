# fast_meta v2.1.0

PNG / JPEG / WebP のメタデータ抽出を、CLI からも Rust ライブラリからも利用できるようにした
リリースです。CLI の既存出力・引数・終了コードは v2.0.0 から変更していません。

## ハイライト

- **ライブラリ API を追加**: `fast_meta` を `lib` として公開しました。
  - `scan_file`: ファイルを mmap し、レコードを `RecordSink` へコールバックで渡します。
  - `extract_to_writer`: CLI と同じ見出し・本文・案内文言を Writer へ出力します。
  - `ScanOptions` / `ScanOutcome` / `ScanError` / `Format` / `StealthPolicy` を公開。
- **sink 方式の採用**: 大きな本文を複製せず、レコードの借用はコールバック呼び出し中だけ有効です。
- **PNG キーワードを2件追加**:
  - `Dream` → label `InvokeAI (Legacy)`
  - `use_stable_diffusion_model` → label `Easy Diffusion`
- README にライブラリとしての利用方法と最小サンプルを追記。

## 使い方（ライブラリ）

```toml
[dependencies]
fast_meta = { git = "https://github.com/wai55555/checkImageMetadata", tag = "v2.1.0" }
```

```rust
use fast_meta::{scan_file, RecordSink, RecordView, ScanOptions};

struct PrintSink;

impl RecordSink for PrintSink {
    fn on_record(&mut self, record: &RecordView<'_>) {
        println!("{}: {}", record.label, record.body);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let outcome = scan_file(
        std::path::Path::new("image.png"),
        &ScanOptions::default(),
        &mut PrintSink,
    )?;
    if outcome.format.is_none() {
        eprintln!("unsupported format");
    }
    Ok(())
}
```

- 未対応形式はエラーではなく `Ok(ScanOutcome { format: None, .. })` で返ります。
- 入出力エラーは `Err(ScanError::Io(..))` で返ります。
- 0バイトファイルは mmap エラーとして `ScanError::Io` になります。

## CLI

引数・出力・終了コードは v2.0.0 から不変です。

```text
fast_meta [--strict] [--stealth <auto|always|never>] <image_file>
```

- `--strict` / `--stealth` の挙動も従来どおりです。
- 未対応形式は標準エラーに `Unsupported file format` を出し、終了コード 0 です。
- `Dream` と `use_stable_diffusion_model` を持つ PNG は、新たにレコードとして表示されます。
- fast_meta の CLI に `--version` はありません（v2.0.0 と同じ）。

## ビルド

```bash
cargo build --release --locked
```

既定 feature の `zlib-ng` を利用するため、C コンパイラと CMake が必要です。
純 Rust の zlib backend を使う場合は次のようにビルドできます。

```bash
cargo build --release --no-default-features --features portable-zlib
```

## 検証

- `cargo build --release --locked` / `cargo test --locked`: 緑
- `tests/golden_cli.rs`: 既存ケースの期待値を変更せずに通過
  - 未対応形式、`Dream`、`use_stable_diffusion_model` のケースを追加
- ライブラリ単体テスト: PNG / JPEG / WebP のレコード順序、未対応形式、0バイトを検証
- キーワード登録表: 16件

詳細は README と `doc/phase0-baseline.md` を参照してください。
