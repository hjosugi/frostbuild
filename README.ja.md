<!-- i18n: language-switcher -->
[English](README.md) | [日本語](README.ja.md)

# FrostBuild POC

FrostBuildはこのアイデアのための小さな概念実証です：

```text
Nixのような正確性
+ Bazel/Buck2のような依存関係グラフとアクションキャッシュ
+ Snowflakeのようなマイクロパーティションメタデータの剪定
= 大規模な多言語モノレポの高速なインクリメンタルビルド
```

このzipには以下が含まれています：

```text
frost.py                     Pythonのプロトタイプビルドエンジン
sample/                      合成ワークスペース（ビルドターゲット161個 + Bazelファイル）
scripts/run_poc_benchmark.sh  ローカルPOCベンチマークを実行
scripts/compare_bazel.sh      Bazelとの比較（Bazelがインストールされている場合）
frost-bench                  生成されたNinja/Makeワークスペースの中央値ベンチマークハーネス
docs/                        ビルドツールの知識、論文、戦略資料
zig_skeleton/               Zig実装のスケルトンと設計ノート
```

## クイックスタート

```bash
cd frostbuild_poc
python3 frost.py bench --workspace sample --jobs 8
```

期待される出力例：

```json
{
  "micro_partition_incremental_s": 0.34,
  "naive_full_rebuild_s": 0.90,
  "speedup_naive_over_micro": 2.6,
  "micro_selected_count": 9,
  "micro_pruned_count": 152,
  "naive_target_count": 161
}
```

このベンチマークは決定論的なシミュレーションです。 このプロトタイプが実際の言語でBazelより優れていると主張するものではありません。 これはコア戦略を示しています：ローカルの変更に対して、完全なプロジェクトクロージャの代わりに影響を受けた小さなマイクロパーティションのクロージャを再ビルドします。

## コマンド一覧

新しいサンプルワークスペースを生成：

```bash
python3 frost.py init-sample --out sample --groups 20 --modules-per-group 8 --cost-ms 30 --force
```

冷静な計画を表示：

```bash
python3 frost.py plan --workspace sample --dry-run
```

ビルド：

```bash
python3 frost.py build --workspace sample --jobs 8
```

ソースファイルを1つ変更：

```bash
printf '\n# local change\n' >> sample/src/pkg05_mod07.fb
```

インクリメンタルマイクロパーティション計画を表示：

```bash
python3 frost.py plan --workspace sample --dry-run
```

インクリメンタルビルドを実行：

```bash
python3 frost.py build --workspace sample --jobs 8
```

影響を受けるテストを実行：

```bash
python3 frost.py test --workspace sample --jobs 8
```

ローカル出力をクリーン（ローカルアクションキャッシュ/CASは保持）：

```bash
python3 frost.py clean --workspace sample
```

出力とキャッシュ状態を削除：

```bash
python3 frost.py clean --workspace sample --cache
```

Bazel比較のオプション：

```bash
bash scripts/compare_bazel.sh
```

これには`bazel`のインストールが必要です。インストールされていない場合、スクリプトはスキップメッセージを出力します。

標準のビルドツールベースラインハーネスを実行：

```bash
./frost-bench run --suite standard --tools ninja,make --sizes 1000,10000 --iterations 5 --jobs 8
```

このハーネスは一時的なNinjaとMakeのワークスペースを生成し、クリーン、何もしない、葉のインクリメンタル、ホットヘッダーの再ビルドシナリオを計測し、CPU governor/turbo/loadのメタデータを記録し、JSONを出力します。ベースラインのレポートは`bench/baselines/`にあります。

公開されたベースライン：

```text
ビルドツールJSON: bench/baselines/2026-07-05-E14.json
Frost POC JSON:  bench/baselines/2026-07-05-E14-frost-poc.json
ホスト: E14、Linux 7.1.2、x86_64、8ジョブ、CPUガバナー性能、ターボ有効
```

リンクされたJSONからのFrost POCシミュレーション：

| 選択 | 剪定 | マイクロインクリメンタル | ナイーブ再ビルド | スピードアップ |
| ---: | ---: | ---: | ---: | ---: |
| 9 | 152 | 0.2877秒 | 0.6703秒 | 2.33倍 |

リンクされたJSONからのビルドツールの中央値タイミング：

| ツール | ターゲット数 | クリーン | 無操作 | 葉のインクリメンタル | ホットヘッダー |
| --- | ---: | ---: | ---: | ---: | ---: |
| Ninja | 1,000 | 1065.252 ms | 5.867 ms | 7.519 ms | 1041.167 ms |
| Make | 1,000 | 1229.647 ms | 129.719 ms | 126.531 ms | 1266.797 ms |
| Ninja | 10,000 | 11655.407 ms | 49.755 ms | 57.099 ms | 11618.390 ms |
| Make | 10,000 | 30857.041 ms | 2104.566 ms | 2144.258 ms | 31991.726 ms |

すべての現在のベンチマークレポートをクリーンなクローンから再現：

```bash
scripts/reproduce.sh
```

クイックスモーク実行例：

```bash
FROST_BENCH_SIZES=10 FROST_BENCH_ITERATIONS=1 scripts/reproduce.sh
```

## このPOCが実装している内容

```text
1. ターゲットグラフ
2. 逆依存関係グラフ
3. ソース内容のハッシュ
4. ローカルアクションキャッシュ
5. 出力のコンテンツアドレスストア
6. 変更されたソースファイルからの影響ターゲットの剪定
7. 選択されたDAG上の並列実行
8. 比較用のBazelワークスペース（オプション）
```

## まだ実装されていない内容

```text
1. 実際のコンパイラ統合
2. 実際のNixサンドボックス化
3. リモート実行
4. リモートキャッシュプロトコル
5. システムコールトレース
6. 細粒度のシンボルレベル依存推論
7. 安全な本番レベルのリグレッションテスト選択
8. Zigの本番エンジン
```

## この仕組みが適切なワークロードで2倍高速になる理由

2倍は常に保証されるわけではありません。ビルドに避けられない巨大なアクションが1つある場合、そのコンパイラやリンカを改善しない限り、ビルドプランナーは2倍高速にできません。何もしないビルドがすでにほぼゼロに近い場合、2倍の余地はありません。

しかし、多くのコミットが小さな範囲に触れる大規模なモノレポでは、マイクロパーティションプランナーはほとんどの作業を剪定して、プロジェクトレベルの再ビルドを上回ることができます。

このサンプル例：

```text
合計ビルドターゲット：161
変更されたソース：src/pkg05_mod07.fb
影響ターゲット：9
剪定ターゲット：152
```

これが主なパフォーマンスの鍵です。

## 次の最良のステップ

これを本格的なツールにするために：

```text
フェーズ1：Pythonのプランナーを維持し、TS/Rust/Go用の実アダプターを追加
フェーズ2：エンジンコアをZigまたはRustで実装
フェーズ3：リモートCAS/アクションキャッシュを追加
フェーズ4：Nixバックのツールチェーン環境を追加
フェーズ5：システムコールトレースと静的解析に基づくテスト選択を追加
```

## ライセンス

0BSD。ほぼあらゆる目的でこのプロジェクトを使用、コピー、修正、配布できます。