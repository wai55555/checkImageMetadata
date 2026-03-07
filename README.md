# fast_meta

高速な画像メタデータ抽出ツール

## 概要

PNG、WebP、JPEG、AVIFファイルからメタデータを高速に抽出するコマンドラインツールです。
AI画像生成ツール（Stable Diffusion、ComfyUI、NovelAIなど）で生成された画像のプロンプトやパラメータを表示できます。
PNGのチャンクデータ読み込みは、通常のテキストに加え圧縮テキスト (zlib) にも対応しています。また、試験的にαチャンネル等にメタデータを埋め込む「Stealth PNG Info」の検知機能も追加されました。

## 特徴

- **極めて高速**: メモリマップドファイルを使用したゼロコピー読み込みに加え、段階的なスキャンフロー（初回64KB + 必要な場合のみ追加シーク）を採用
- **効率的**: チャンク指紋（Fingerprint）判定により、ComfyUI等の巨大なワークフローを持つ画像に対しても最小限のIOで解析
- **対応フォーマット**:
  - PNG (tEXtチャンク / 圧縮iTXtチャンク / Stealth Info)
  - WebP (EXIFチャンク / XMPチャンク)
  - JPEG (EXIFメタデータ)
  - AVIF (EXIFメタデータ)

## 対応メタデータ

- **Stable Diffusion (A1111/Forge)**: `parameters`
- **ComfyUI**: `prompt`, `workflow` (末尾データの検知に対応)
- **NovelAI**: `Description` (Prompt), `Comment` (Settings JSON, 圧縮形式に対応)
- **EXIF / XMP**: `UserComment`, `ImageDescription`, `parameters`

## ビルド

```powershell
cargo build --release
```

実行ファイルは `target\release\fast_meta.exe` に生成されます。

## 使い方

```powershell
fast_meta.exe <画像ファイル>
```

### 例

```powershell
# PNGファイルのメタデータを表示 (圧縮されたNovelAI画像や巨大なComfyUI画像に対応)
.\fast_meta.exe image.png

# WebPファイルのメタデータを表示 (EXIFおよびXMPに対応)
.\fast_meta.exe image.webp
```

## 技術詳細

- **段階的スキャンフロー**:
  1. **Fast Scan (64KB)**: 先頭64KBを読み込み、標準的なメタデータを抽出。
  2. **Fingerprint判定**: IHDR直後のチャンク構造からComfyUI等の特定ライブラリを判別。
  3. **Tail Scan**: 必要な場合のみ、ファイル末尾128KBをシークして巨大ワークフローを抽出。
- **メモリマップドI/O**: `memmap2` を使用した高速ファイル読み込み。
- **圧縮解凍**: `flate2` を使用した zlib 解凍 (iTXt対応)。
- **LSB解析**: `image` クレートによるピクセルサンプリングを用いた Stealth PNG 検知。

## 依存関係とライブラリ

本プロジェクトでは以下のライブラリおよび技術を使用しています。

- **zlib**: データの圧縮・解凍に使用。
  - 入手先: [zlib Home Site](https://zlib.net/) / [GitHub (madler/zlib)](https://github.com/madler/zlib)
- **Rust Crates**:
  - `flate2`: zlib (Deflate) のデコード
  - `image`: 画像のピクセルデータへのアクセス (Stealth PNG用)
  - `memmap2`: 高速なファイルアクセス
  - `serde_json`: JSONのパースと整形
  - `kamadak-exif`: EXIF情報の定義参照

## ライセンス

MIT
