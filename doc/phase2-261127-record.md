# Phase 2 記録（261127 誤分類改修）— fast_meta の構造化 API・抽出・表示

対象仕様書: `doc/261127_plan-false-classification-fix.md` §5／§9.2／§9.3／§11.1 Phase 2。
対象リポジトリ: checkImageMetadata（`fast_meta` 2.1.0 → **2.2.0**）。

## 1. 実装した要件

| 要件 | 実装 | 検証 |
|---|---|---|
| REQ-API-01 | JPEG COM に構造化された印を `RecordView.detail` で付与。`JPEG_COM_A1111_BOTH_MARKERS`（`a1111-both-markers`）と `JPEG_COM_A1111_ONE_MARKER`（`a1111-one-marker`）を公開定数として追加。`label` は表示専用のまま | `jpeg_com_with_one_marker_sets_the_partial_mark`、`jpeg_com_mark_does_not_depend_on_the_label`、sorter の T-06 |
| REQ-EXT-01 | Stealth Auto ゲートを「実質レコードが1件も無いときだけ検査」に変更。種別表（閉じる／閉じない）を `is_substantive_record` に実装。表に無い種別は内容側＝閉じるに倒す | `t40_substantive_records_close_the_stealth_gate`、`t40_generic_records_keep_the_stealth_gate_open` |
| REQ-EXT-06 | XMP パケットの委譲を iTXt だけでなく tEXt／zTXt でも行う（`text_records` に一本化） | `t16_reads_xmp_fields_from_text_and_ztxt_chunks`、sorter の `xmp_packet_in_a_text_chunk_is_sorted_as_webui` |
| REQ-EXT-07 | Draw Things の本文合成・`dc:description` 抑制・`Draw Things (XMP)` ラベルを廃止し、生 JSON をそのままレコード化 | `t17_draw_things_json_is_not_synthesized_nor_suppresses_description`、`draw_things_c_only_json_is_passed_through_as_is` |
| REQ-DSP-01 | TextSink の「XMP Description 単独 → Midjourney (XMP)」再ラベルを廃止 | `xmp_description_is_never_relabeled_as_midjourney`（G-01） |
| REQ-DSP-02 | 診断文（tail 案内文言）を stderr へ分離。見出し行とレコードは stdout のまま。`extract_to_writer_with_diagnostics` を追加（`extract_to_writer` の既存挙動は共有 writer で維持） | `golden_comfyui_png_workflow_after_idat_via_stderr_diagnostics`（G-02）、`golden_*` で stderr が空であること |
| §9.3 | 版を 2.2.0 へマイナー上げ（公開レコード内容が変わるため）。sorter 側の依存も更新 | `Cargo.toml`、sorter の `Cargo.toml` |

## 2. ゴールデン出力の差分（すべて要件 ID に追跡可能）

§9.2 の原則に従い、実際に生じた差分を列挙する。列挙に無い差分は無い。

| 差分 | 要件 ID | テスト |
|---|---|---|
| XMP Description のラベルが `Midjourney (XMP)` から `XMP Description` へ | REQ-DSP-01 | `golden_webp_xmp_description_element`、output.rs／jpeg.rs のテスト |
| tail 案内文言が stdout から stderr へ（stdout の見出し・レコードは不変） | REQ-DSP-02 | `golden_comfyui_png_workflow_after_idat_via_stderr_diagnostics` |
| Draw Things XMP の本文が合成テキストから生 JSON へ、description が抑制されずに出る | REQ-EXT-07 | `fixture_8_…`、`t17_…` |
| tEXt／zTXt 格納の XMP パケットからレコードが出る（従来は0件） | REQ-EXT-06 | `t16_…` |
| Stealth の出現範囲が変わる（汎用レコードのみの画像でも Stealth を調べる） | REQ-EXT-01 | `t40_…` |
| JPEG COM レコードの `detail` が付く（本文・ラベルは不変） | REQ-API-01 | jpeg.rs のテスト |

## 3. 未対応・申し送り

- **タグ付け**: 版は 2.2.0 に上げたが、`v2.2.0` タグの公開は本ブランチのマージ後に行う。
  sorter 側は現在 `fast_meta = { path = "../checkImageMetadata" }` を指しており、
  タグ公開後に `{ git = "https://github.com/wai55555/checkImageMetadata", tag = "v2.2.0" }`
  へ戻す（sorter の Cargo.toml にコメントを残した）。
- **P-16 / T-40 の実ファイル検証**: sorter 側に Stealth PNG（LSB）を生成する合成器が無いため、
  ゲートの検証は fast_meta の T-40（レコード種別表の固定）と sorter のレコード単位 P-16 で行う。
  実ファイルでの Stealth 経路は既存の phase4_stealth テストが担保する。
- **REQ-CLS-02 (f)**（Midjourney の XMP のみ）は U-7 の結論により実装しない（Phase 0 記録）。
- Phase 3 で扱う fail-soft 化（REQ-EXT-02〜05、08〜11、12〜15）は未着手。
