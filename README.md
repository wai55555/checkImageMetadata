# fast_meta

高速な画像メタデータ抽出ツール

## 概要

PNG、WebP、JPEG、AVIFファイルからメタデータを高速に抽出するコマンドラインツールです。
AI画像生成ツール（Stable Diffusion、ComfyUI、NovelAIなど）で生成された画像のプロンプトやパラメータを表示できます。

## 特徴

- **高速**: メモリマップドファイルを使用したゼロコピー読み込み
- **効率的**: 必要な部分のみを読み取り、バイナリ全体の検索を回避
- **対応フォーマット**:
  - PNG (tEXtチャンク)
  - WebP (EXIFチャンク)
  - JPEG (EXIFメタデータ)
  - AVIF (EXIFメタデータ)

## 対応メタデータ

- **Stable Diffusion (A1111)**: `parameters`
- **ComfyUI**: `prompt`, `workflow`
- **NovelAI**: `Description`, `Comment`
- **EXIF**: `UserComment`, `ImageDescription`

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
# PNGファイルのメタデータを表示
.\fast_meta.exe image.png

# JPEGファイルのメタデータを表示
.\fast_meta.exe photo.jpg

# WebPファイルのメタデータを表示
.\fast_meta.exe image.webp

# AVIFファイルのメタデータを表示
.\fast_meta.exe image.avif
```

## 出力例

```
=== JPEG File: test.jpg ===
--- EXIF UserComment ---
masterpiece, best quality, ultra-detailed, 1girl, smile
Negative prompt: worst quality, low quality
Steps: 25, Sampler: Euler a, CFG scale: 7, Seed: 3787625783
```

## 技術詳細

- **メモリマップドI/O**: `memmap2`を使用した高速ファイル読み込み
- **フォーマット別最適化**:
  - PNG: チャンク構造を直接パース
  - WebP: RIFFコンテナからEXIFチャンクを抽出
  - JPEG/AVIF: 先頭64KBからUserCommentタグを検索
- **UTF-16BE対応**: EXIFのUNICODEエンコーディングに対応

## 依存関係

- `memmap2`: メモリマップドファイルI/O
- `serde_json`: JSONメタデータのパース
- `kamadak-exif`: EXIFデータ構造の定義

## ライセンス

MIT
