# CLAUDE.md

ローカルで作業を引き継ぐClaude向けの指示。

## このプロジェクトは何か

ビルドキャッシュの転送量削減に関する実証研究。論文化を目指している。

**中心仮説:** remote build cacheのartifact転送において、
近傍artifactからのbinary delta（bsdiff / zstd patch-from）は、
Bazelが2026年時点で実験実装しているFastCDC chunk dedupより大幅に転送量を削減できる。

**なぜ健全性が壊れないか:** 再構成後にexact digest検証を行うため。
これはREAPIのSpliceBlobが既に採用しているパターン（`SpliceBlobRequest.blob_digest` 必須）。
誤った予測のコストは時間だけで、正しさには一切触れない。

## 現在の状態

| 項目 | 状態 |
|---|---|
| V1（REAPIにdelta encodingがないこと） | **確認済**。`docs/build-cache-research-memo.md` 第10節の検証ログ参照 |
| FastCDC実装 | **Bazelのソースからbit-compatibleに移植済**（`harness/fastcdc.py`） |
| 予備実験（Lua） | **実施済**。`results/` 参照 |
| V4（先行研究の網羅調査） | **未実施。最優先** |
| 大規模corpus（LLVM / Chromium規模） | 未実施 |
| 論文執筆 | 未着手 |

## 最優先タスク（v3で更新）

### 0. メモはv3を読む

`docs/build-cache-research-memo.md` はv3（deep research v2 + 予備実験 + URL実地検証を統合）。
中心提案は **DeltaCDC**: CDCでmissになったchunkだけを類似baseからchunk-level binary delta
で再構成し、chunkとblobのdigestで二重検証する。whole-blob deltaではなくchunk-levelに
した理由はbsdiffのCPUコスト有界化（§4.1）。

### 1. V4: 先行研究の網羅調査

これが埋まらないと論文が書けない。「近傍選択delta encodingをbuild cacheに適用」の
先行研究が本当に存在しないかを確認する。

検索先: DBLP, Google Scholar, arXiv, ACM DL, BazelCon資料, buildbarn/justbuild周辺

キーワード候補:
```
build cache delta encoding
remote execution artifact reuse
content-defined chunking build artifacts
incremental artifact transfer
binary diff software distribution
```

特に **SplitBlob/SpliceBlob の提案元**（REAPI PR #282、author: Sascha Roloff、
justbuild周辺と思われる）が、設計時にdelta encodingを検討して却下した記録がないかを確認する。
**却下されていた場合はその理由が本論の最大の障害になる。**

結果は `docs/build-cache-research-memo.md` の第4節の表に追記すること。

### 2. corpusの拡大（Phase A残り）

Luaの結果（小artifact領域）はGO判定済。残るのは**マルチchunk blob（>2MiB）での
chunk-level residual delta測定**。

- **LLVM（hermetic-llvm）を最優先** — EngFlowが同構成（Bazel 9.1.1 + FastCDC2020）で
  CDCの効果を公表済（version bump後CAS upload 50%削減）。同条件で測れば直接比較になる
- **Go製の大きめの静的リンクバイナリ** — address shiftが全域に波及、CDC最悪ケース
- **Rust** — rlib / 実行ファイル

追加の必須測定: **CPU時間**（bsdiff encode/decode）。analyze.pyに計測を足す。
chunk-level化でコストは有界だが実測が要る。

`harness/collect_corpus.sh` はビルドコマンドとglobを引数に取るので、
プロジェクトを差し替えるだけで動く。

### 3. oracle近傍選択の実装

現在の近傍選択は「同一ファイル名の直前バージョン」だけ（trivial selector）。
論文には上限が必要:

- (i) trivial: 直前バージョン ← 実装済
- (ii) sketch ANN: SimHash/MinHashで近傍探索 ← 未実装
- (iii) oracle: local CAS全体から最良 ← 未実装

**(i) が (iii) に十分近いなら、sketchは不要という結論になる。**
それも正当な結果なので、無理にsketchを推さないこと。

## ハーネスの使い方

```bash
# corpus収集（連続コミットでビルドして成果物を集める）
harness/collect_corpus.sh <repo-url> <n-commits> <out-dir> "<build-cmd>" '<glob>'...

# 測定
python3 harness/analyze.py <corpus-dir> --avg-kib 512 --out results/raw.json

# FastCDCのパラメータ掃引（レビュアが必ず聞くので必須）
for k in 512 64 16; do python3 harness/analyze.py <corpus-dir> --avg-kib $k; done
```

依存: `pip install zstandard bsdiff4`

## 作業上の約束

1. **引用を捏造しない。** 一次文献を確認していない主張は
   `docs/build-cache-research-memo.md` 第4節に `[要検証]` で登録する。
   LLM出力（自分のものを含む）をそのまま論文に書かない。

2. **数字を丸めない・盛らない。** 測定結果はraw JSONを残し、
   有利な条件だけを報告しない。FastCDCには最も有利なパラメータを与えて比較する。

3. **negative resultも書く。** neighbor deltaがCDCに勝てなければ、
   その事実を記録して方向転換する。判断ポイントはメモ第9節。

4. **メモが唯一の真実源。** 新しい調査結果は
   `docs/build-cache-research-memo.md` の対照表に流し込む。
   別ファイルに知見を散らさない。

## 文脈

- 著者はIIJのリードエンジニア。Javaバックエンドとデータ基盤が主戦場
- この研究はHive→Snowflake移行で得た「pruningが並列度に勝つ」という
  観察をビルドシステムに転用したもの
- 出力は日本語。ただし論文本文・コード内コメントは英語
- 短い文、平易な語彙を好む
