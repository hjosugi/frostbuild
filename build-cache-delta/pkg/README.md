# build-cache-delta — 実証パッケージ

remote build cacheのartifact転送量を、近傍からのbinary deltaで削減できるかを検証する。

比較対象は **Bazelが実際に持っている転送方式**:

| scheme | Bazelでの対応 |
|---|---|
| raw | 無圧縮のByteStream転送 |
| zstd | `--remote_cache_compression` |
| cdc / cdc+zstd | `--experimental_remote_cache_chunking`（FastCDC 2020 + SplitBlob/SpliceBlob） |
| zdelta | 提案。zstdのraw dictionary delta（`zstd --patch-from` 相当） |
| bsdiff | 提案。バイナリ特化delta |

すべての方式は再構成後にexact digest検証で終端するため、健全性は同一。
比較軸は転送バイト数だけ。

## クイックスタート

```bash
pip install zstandard bsdiff4

# 1. corpusを作る（連続40コミットをビルドして成果物を収集）
harness/collect_corpus.sh https://github.com/lua/lua 40 corpus/lua-O2 \
  "make -j1 all" '*.o' 'lua' 'liblua.a'

# 2. 測定
python3 harness/analyze.py corpus/lua-O2 --avg-kib 512

# 3. FastCDCパラメータの掃引
for k in 512 128 64 16; do
  python3 harness/analyze.py corpus/lua-O2 --avg-kib $k
done
```

## 構成

```
CLAUDE.md                       ローカルClaude向け作業指示（まずこれを読む）
docs/
  build-cache-research-memo.md  研究メモ本体。論文アウトライン・対照表・検証ログ
harness/
  fastcdc.py                    BazelのFastCdcChunker.javaのbit-compatible移植
  fastcdc_params.json           Bazelから抽出したGEAR/MASKS定数
  collect_corpus.sh             連続コミットでビルドして成果物を収集
  analyze.py                    転送方式の比較測定
results/
  *.txt                         測定結果
```

## 測定モデル

コミット i で artifact A が必要になったとき、クライアントは
コミット 1..i-1 の成果物をlocal CASに持っている。
remote action cacheはhitしている（= 出力digestは既知）。
このとき何バイトをネットワーク越しに運ぶ必要があるか、を測る。

exact CAS hit（前と完全に同一）は全方式で0バイトなので、
比較対象は **「変化したが、前バージョンが手元にある」artifact** に絞る。

## 注意

- bsdiffは suffix array を作るため大きいファイルで遅い。
  数十MB級のcorpusでは `--no-bsdiff` で先にzdeltaだけ回すとよい
- FastCDCのデフォルトはBazel準拠（avg 512KiB / min 128KiB / max 2MiB、normalization 2）
- corpusのビルドは1コミットあたり数秒〜数分。`collect_corpus.sh` は
  既存ディレクトリをスキップするので中断・再開できる
