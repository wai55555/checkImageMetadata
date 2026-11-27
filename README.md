# fast_meta

PNG、JPEG、WebP から AI 画像生成のプロンプトや生成情報を抽出する CLI です。入力は1ファイルで、メタデータを人間可読テキストとして標準出力に表示します。

## 対応形式

- **PNG**: `tEXt`、`zTXt`、`iTXt`、`eXIf`、XMP、IEND 後の trailing data、Stealth PNG Info
- **JPEG**: EXIF/TIFF、XMP と Extended XMP、COM コメント、APP13/IPTC Caption
- **WebP**: EXIF/TIFF、XMP

PNG の登録対象キーワードには A1111/Forge (`parameters`)、ComfyUI (`prompt`, `workflow`, `generation_data`)、NovelAI (`Description`, `Comment`)、SwarmUI、InvokeAI が含まれます。XMP から dc:description、exif:UserComment、xmp:CreatorTool、IPTC `AIPromptInformation`／`DigitalSourceType` を抽出します。

抽出層は本文を合成・置換しません（Draw Things 形式かどうかの判定は利用側で行います）。JPEG COM には A1111 形式であることを示す構造化された印を `RecordView.detail`（`a1111-both-markers`／`a1111-one-marker`）で付けます。表示層も生成元を推定したラベル付け直しは行いません。

## 使い方

```text
fast_meta [--strict] [--stealth <auto|always|never>] <image_file>
```

```powershell
.\fast_meta.exe image.png
.\fast_meta.exe --strict image.png
.\fast_meta.exe --stealth always image.png
```

- `--stealth auto` は既定値です。生成内容／生成パラメータを担うレコード（`parameters`、`prompt`、`workflow`、`Comment`、`Description`、`UserComment`、`ImageDescription`、COM、XMP の `description`／`AIPromptInformation`／`DigitalSourceType` 等）が1件も無いときだけ Stealth PNG Info を調べます。`Software`／`Title`／`Source`／XMP `CreatorTool`、IPTC Caption のような付随情報だけの画像では検査します。
- `--stealth always` は通常メタデータと Stealth の両方を調べます。
- `--stealth never` は Stealth の検査を行いません。
- `--strict` は PNG の IDAT 列を含む全チャンクを走査します。

既定の高速 PNG 走査では、連続する IDAT チャンクの間に置かれたテキストチャンクは検出しません（既知の制約 D-17）。この配置のテキストを調べるには `--strict` を指定してください。

Stealth PNG は非インターレースのカラータイプ 2 / 4 / 6、8bit / 16bit を対象とします。Adam7 インターレースには対応していません。

## 抽出の堅牢性（2.3.0）

- 壊れた1単位（TIFF のエントリ、PNG のチャンク、XMP の局所破損）が、後ろにある本物の
  メタデータを落とさない（fail-soft）。
- PNG の最終手段（Stage C step 4）で、IEND 到達も CRC の接続証明も無いまま得たレコードは
  `Source::PngUnproven` として隔離する。分類側では「弱い証拠」として扱い、単独では
  確定に使わないこと（`Source::Trailing` と同じ扱い）。
- XMP は UTF-16（BOM 付き BE／LE）のパケットも読む（UTF-32 は対象外）。局所破損があっても
  破損前のフィールドは返し、複数の `rdf:Description` がある場合は主対象
  （`rdf:about=""`）のフィールドを優先する。
- WebP は RIFF サイズの範囲内だけを歩き、`Exif\0\0` プレフィクス付きの EXIF も読む。

## ライブラリとして使う

`fast_meta` は CLI とライブラリの両方を提供します。レコードは mmap の生存期間内に
コールバックで渡されるため、保持する場合は `on_record` の中で所有化してください。

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

`scan_file` の戻り値が `Ok` で `format == None` の場合は未対応形式です。
入出力エラーは `Err(ScanError::Io(..))` で返ります。

## サイズ上限

- `tEXt` 本文: 明示的な上限なし
- `zTXt` / 圧縮 `iTXt`: 解凍後 64 MB / チャンク
- XMP パケット: 16 MB（REQ-EXT-12: 資源保護のため維持する仕様。上限超過のパケットは
  「その担体が 0 件」になる。切り捨てて部分的に読む方式は、構造検証ができず
  fail-open を招くため採用しない）
- EXIF ブロック: 16 MB（同上）
- Stealth PNG: 最大寸法 16,384、解凍後 512 MB、ペイロード 8 MB、IDAT 合計 512 MB
- IEND 後の trailing data: 先頭 1 MB を走査

## 未実装（既知の制限）

- **C2PA**: 実装していません。`Source::C2pa` は enum の予約に留まり、C2PA の生成宣言
  （`trainedAlgorithmicMedia` 等）だけを持つ画像は、現行ではどのレコードも生成されません
  （分類は空振りする）。対応済みと誤認させる表示は行いません。
- **PNG の 16 MB 超 XMP／EXIF**: 上記のとおり 0 件（仕様）。

## ビルド

```powershell
cargo build --release
```

既定では高速な `zlib-ng` backend を使用します。ビルド環境に C コンパイラがなく `zlib-ng` をビルドできない場合は、純 Rust の `miniz_oxide` backend を選択できます。

```powershell
cargo build --release --no-default-features --features portable-zlib
```

## ライセンス

MIT

## 外部ツールとの照合

ExifTool が利用できる場合は、同じ画像に対して次を実行して出力を比較できます。

```text
exiftool -G1 -a -s <image_file>
```
