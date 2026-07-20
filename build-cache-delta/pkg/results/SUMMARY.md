# 予備実験の結果サマリ

対象: lua/lua 連続40コミット、成果物 = 34個の .o + liblua.a + lua

- `lua-O2`: `make all`（-O2、リリース相当）
- `lua-g` : `-g -O0`（デバッグ相当、成果物が約2.7倍）

測定対象は「前バージョンが手元にあるcache miss」のみ。exact hitは全方式で0バイト。

## 転送バイト数（MB）

| corpus | FastCDC avg | misses | 1-chunk率 | raw | zstd | cdc+zstd | zdelta | bsdiff | best/cdc+zstd |
|---|---|---|---|---|---|---|---|---|---|
| lua-O2 | 512 KiB | 113 | 100.0% | 20.98 | 8.08 | 8.08 | 0.47 | 0.29 | **3.6%** |
| lua-O2 | 64 KiB | 113 | 59.3% | 20.98 | 8.08 | 5.29 | 0.47 | 0.29 | **5.5%** |
| lua-O2 | 16 KiB | 113 | 27.4% | 20.98 | 8.08 | 3.74 | 0.47 | 0.29 | **7.8%** |
| lua-g | 512 KiB | 192 | 67.2% | 78.74 | 26.34 | 21.47 | 4.79 | 0.33 | **1.5%** |
| lua-g | 64 KiB | 192 | 49.5% | 78.74 | 26.34 | 14.76 | 4.79 | 0.33 | **2.2%** |
| lua-g | 16 KiB | 192 | 7.3% | 78.74 | 26.34 | 10.75 | 4.79 | 0.33 | **3.0%** |

`512 KiB` がBazelのデフォルト（`ChunkingConfig.DEFAULT_AVG_CHUNK_SIZE`）。
`64` / `16` はCDCに有利な方向へ振った条件（REAPIの許容範囲内）。

## 読み取れること

1. **Bazelのデフォルトパラメータでは、FastCDCがリリースビルド成果物に対して完全に無効。**
   -O2 corpusでは全113件のmissが1チャンクに収まり、CDCの転送量はraw転送と同一。
   min chunk size 128 KiB に対して成果物が小さすぎる。

2. **CDCに有利なパラメータを与えても、neighbor deltaが1桁以上優位。**
   avg=16 KiBまで下げてCDCの1-chunk率を7〜27%まで改善しても、
   bsdiffはcdc+zstdの3.0〜7.8%に収まる。

3. **デバッグビルドで差が最大化する。**
   `lua-g` ではbsdiffがcdc+zstdの **1.5%**（21.5 MB → 0.33 MB）。
   デバッグ情報はoffsetと行番号が全域に散るため、
   chunk完全一致を前提とするCDCが最も苦手とする変化パターンになる。

4. **zdeltaとbsdiffの差が大きい。**
   `lua-g` の .a では zdelta 4.14 MB に対して bsdiff 0.051 MB（約80倍差）。
   zstdのdictionary matchは大きなシフトを追えないのに対し、
   bsdiffはsuffix sortでaddress shiftを吸収できる。
   **バイナリ特化のdelta形式であることが本質的に効いている。**

## 限界（論文に明記すべきこと）

- corpusが1プロジェクトのみ、artifactが最大3 MB。CDCが本来想定する
  数十MB級の成果物では結論が変わる可能性がある
- 近傍選択は「同一ファイル名の直前バージョン」のみ。oracle上限は未測定
- CPU時間を測っていない。bsdiffはsuffix arrayを作るため、
  転送量削減とCPUコストのトレードオフが未評価。**次の必須測定項目**
- チャンクのlocal CASは無制限に保持と仮定（CDCに有利な条件）

