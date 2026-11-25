# fast_meta

PNG、JPEG、WebP から AI 画像生成のプロンプトや生成情報を抽出する CLI です。入力は1ファイルで、メタデータを人間可読テキストとして標準出力に表示します。

## 対応形式

- **PNG**: `tEXt`、`zTXt`、`iTXt`、`eXIf`、XMP、IEND 後の trailing data、Stealth PNG Info
- **JPEG**: EXIF/TIFF、XMP と Extended XMP、COM コメント、APP13/IPTC Caption
- **WebP**: EXIF/TIFF、XMP

PNG の登録対象キーワードには A1111/Forge (`parameters`)、ComfyUI (`prompt`, `workflow`, `generation_data`)、NovelAI (`Description`, `Comment`)、SwarmUI、InvokeAI が含まれます。XMP から Draw Things、Midjourney、IPTC `AIPromptInformation` などを抽出します。

## 使い方

```text
fast_meta [--strict] [--stealth <auto|always|never>] <image_file>
```

```powershell
.\fast_meta.exe image.png
.\fast_meta.exe --strict image.png
.\fast_meta.exe --stealth always image.png
```

- `--stealth auto` は既定値です。通常のメタデータがない場合に Stealth PNG Info を調べます。
- `--stealth always` は通常メタデータと Stealth の両方を調べます。
- `--stealth never` は Stealth の検査を行いません。
- `--strict` は PNG の IDAT 列を含む全チャンクを走査します。

既定の高速 PNG 走査では、連続する IDAT チャンクの間に置かれたテキストチャンクは検出しません（既知の制約 D-17）。この配置のテキストを調べるには `--strict` を指定してください。

Stealth PNG は非インターレースのカラータイプ 2 / 4 / 6、8bit / 16bit を対象とします。Adam7 インターレースには対応していません。

## サイズ上限

- `tEXt` 本文: 明示的な上限なし
- `zTXt` / 圧縮 `iTXt`: 解凍後 64 MB / チャンク
- XMP パケット: 16 MB
- EXIF ブロック: 16 MB
- Stealth PNG: 最大寸法 16,384、解凍後 512 MB、ペイロード 8 MB、IDAT 合計 512 MB
- IEND 後の trailing data: 先頭 1 MB を走査

## ビルド

```powershell
cargo build --release
```

既定では高速な `zlib-ng` backend を使用します。ビルド環境に C コンパイラがなく `zlib-ng` をビルドできない場合は、純 Rust の `miniz_oxide` backend を選択できます。

```powershell
cargo build --release --no-default-features --features portable-zlib
```

## 外部ツールとの照合

ExifTool が利用できる場合は、同じ画像に対して次を実行して出力を比較できます。

```text
exiftool -G1 -a -s <image_file>
```
