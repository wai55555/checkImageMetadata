# fast_meta v2.3.0

**v2.1.0 からの累積リリースです。** v2.2.0 はタグのみ作成しリリースしていないため、
本リリースには v2.2.0 の内容もすべて含まれます。

抽出の「壊れた1単位が後ろの本物のメタデータを落とす」経路を塞ぎ（fail-soft）、担体間で
食い違っていたデコードを揃え、分類側が表示ラベルに依存しないよう構造化された印を追加しました。
あわせて Stealth 自動検査の意味を見直し、抽出層が本文を合成しないことを明確にしています。

## ハイライト

### 1. 構造化された印（ライブラリ API 追加）

- **JPEG COM の A1111 形式**を `RecordView.detail` で示します（REQ-API-01）。
  - `JPEG_COM_A1111_BOTH_MARKERS` = `"a1111-both-markers"`（`Negative prompt:` と `Steps:` の両方）
  - `JPEG_COM_A1111_ONE_MARKER` = `"a1111-one-marker"`（片方のみ）
  - `label` は表示専用のまま。sorter は label ではなくこの値で判定できます。
- **未証明レコードの隔離**: PNG の最終手段（Stage C step 4）で、IEND 到達も CRC の接続証明も
  無いまま得たレコードを **`Source::PngUnproven`** として返します（REQ-EXT-03(b)）。
  分類側では `Source::Trailing` と同じ「弱い証拠」として扱ってください。
- **診断文の分離**: `extract_to_writer_with_diagnostics` を追加。レコードと見出しは従来どおり
  `out`、案内文言（tail を走査した旨など）は `diagnostics` へ出せます。
  既存の `extract_to_writer` は従来どおり同じ writer へ両方を出します。

### 2. Stealth 自動検査（`--stealth auto`）の意味変更

- 旧: 「通常のメタデータが1件でもあれば検査しない」
- 新: **「生成内容／生成パラメータを担うレコード（実質レコード）が1件も無いときだけ検査する」**
  - 閉じる側（実質）: `parameters` / `prompt` / `workflow` / `generation_data` / `Comment` /
    `Description` / `sui_*` / `invokeai_metadata` / `sd-metadata` / `Dream` /
    `use_stable_diffusion_model`、EXIF・XMP の `UserComment`、EXIF `ImageDescription`、
    JPEG COM、XMP の `description` / `AIPromptInformation` / `DigitalSourceType`
  - 閉じない側（汎用）: `Software` / `Title` / `Source`、EXIF `Software`/`Make`/`Model`/`Artist`/
    `XPComment`、XMP `CreatorTool` / `title`、IPTC Caption、Trailing
- 検査コストは増えません。Stealth 検査は IDAT 全解凍ではなく、シグネチャ判定に必要な先頭行だけを
  解凍し、不一致なら打ち切ります（4096×4096 でも測定差なし）。

### 3. 抽出層は本文を合成しない（REQ-EXT-07）

- Draw Things 形式（XMP `UserComment` の JSON）でも、本文を整形テキストへ置換せず**生 JSON** を
  返します。`dc:description` を抑制する副作用と `Draw Things (XMP)` ラベルも廃止しました。
- Draw Things かどうかの判定は利用側（値検証）へ移しました。

### 4. fail-soft（壊れた1単位で全体を失わない）

| 対象 | 変更 |
|---|---|
| TIFF | エントリ1件が読めなくても IFD 全体を捨てず、残りを解析する |
| PNG Stage A | 壊れたチャンクで止まっても、後方から最初の IDAT を再同期探索して Stage B/C を試す |
| PNG Stage C step 4 | 接続証明の無いレコードは `Source::PngUnproven` として隔離（通常レコードにしない） |
| XMP | パケット単位の all-or-nothing をやめ、局所破損より前のフィールドを返す |
| JPEG / WebP | セグメント長・チャンクサイズ不正では歩行を止めるが、停止までに得たレコードは返す |

### 5. デコードとフィールド選択の是正

- **XMP**: `tEXt` / `zTXt` に格納されたパケットも iTXt と同じく XMP として解釈（従来は 0 件）。
  UTF-16（BOM 付き BE/LE）のパケットも読む（UTF-32 は対象外）。
  複数の `rdf:Description` がある場合、主対象（`rdf:about=""`）のフィールドを優先。
  JPEG 拡張 XMP は GUID／total_length ごとに組を分け、完全な組を採用（混在で全体を捨てない）。
- **PNG テキスト**: 先頭の UTF-8 BOM を除去。iTXt の非 UTF-8 テキストも `tEXt`/`zTXt` と
  同じ Latin-1 フォールバックでレコード化（キーワードごと落とさない）。
- **TIFF**: ASCII 系タグ（`Software` / `ImageDescription` 等）に Latin-1 フォールバックを追加。
  次 IFD の追跡を IFD1 以降へ拡張（上限 4）し、IFD1 以降の ExifIFD も辿る（IFD2+ も可）。
  BOM なし UNICODE `UserComment` は **BE を既定**とし、LE は「BE が明確に破綻し LE が成立する」
  場合に限る。
- **WebP**: RIFF サイズの範囲内だけを歩く（範囲外の EXIF/XMP 形状バイト列は拾わない）。
  EXIF チャンク先頭の `Exif\0\0` を読み飛ばす。
- **表示**: XMP Description を `Midjourney (XMP)` へ再ラベルしない（常に `XMP Description`）。

### 6. 仕様の明文化（挙動変更なし）

- **16MB 上限**（EXIF / XMP パケット）は資源保護として維持します。上限超過は「その担体が 0 件」に
  なります。切り捨てて部分的に読む方式は、構造検証ができず fail-open を招くため採用しません。
- **C2PA は未実装**です。`Source::C2pa` は enum の予約に留まり、C2PA の生成宣言だけを持つ画像は
  現行ではどのレコードも生成されません（既知の制限）。対応済みと誤認させる表示は行いません。

## 互換性・注意（v2.1.0 からの差分）

**ライブラリ利用者向け**

- `Source` に `PngUnproven` を追加しました。`Source` を網羅 `match` しているコードは追加が必要です。
- JPEG COM レコードの `detail` に値が入るようになりました（従来は `None`）。
- XMP `UserComment`（Draw Things）の本文が、合成テキストから**生 JSON** に変わります。
- XMP のフィールド値が、`rdf:about=""` 優先の規則で変わることがあります。
- PNG テキストの本文から UTF-8 BOM が除かれます。TIFF の非 UTF-8 ASCII タグは Latin-1 として
  読めるようになります（従来はレコードごと欠落）。
- `--stealth auto` の意味が変わり、汎用レコード（`Software` / `Title` / `CreatorTool` 等）だけの
  画像でも Stealth 検査が走ります（Stealth レコードが出る範囲が広がります）。
- 従来 0 件だったレコードが新たに出ます（UTF-16 XMP、tEXt/zTXt 格納 XMP、非 UTF-8 iTXt、
  Latin-1 TIFF、IFD1 以降の ExifIFD/IFD2+）。

**CLI 利用者向け**

- 引数は不変です（`fast_meta [--strict] [--stealth <auto|always|never>] <image_file>`）。
- 案内文言（`(Large file or ComfyUI detected. ...)`）は **stdout から stderr** へ移りました。
  ファイル見出し行（`=== PNG File: <path> ===`）とレコードは stdout のままです。
- XMP Description のラベルが `Midjourney (XMP)` から `XMP Description` に変わりました。

## アップグレード

```toml
[dependencies]
fast_meta = { git = "https://github.com/wai55555/checkImageMetadata", tag = "v2.3.0" }
```

## 検証

- `cargo build --release --locked` / `cargo test --locked`: 緑
  - ライブラリ単体テスト 114
  - CLI ゴールデン 9、キーワード 4、WebP 4、Stealth 10
- 既存のゴールデン期待値は、意図した変更（上記の表示・診断文の移動）以外では変更していません。
- 破損・切り詰め・非 UTF-8・UTF-16・16MB 超の各フィクスチャで、panic せず・レコード欠落しない
  ことをテストで固定しています。

タグ: `v2.2.0`（未リリース）→ **`v2.3.0`**
