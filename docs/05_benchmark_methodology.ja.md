<!-- i18n: language-switcher -->
[English](05_benchmark_methodology.md) | [日本語](05_benchmark_methodology.ja.md)

# ベンチマーク手法

## ベンチマーク対象

クリーンビルドだけでなく、さまざまなケースを測定してください。

実際の開発者の生産性は以下に依存します：

```text
1. 無操作ビルド
2. 小さな段階的変更
3. 中規模のパッケージレベルの変更
4. 横断的な設定変更
5. クリーンビルド
6. テストのみの変更
7. CIのコールドキャッシュ
8. CIのウォームキャッシュ
9. リモートキャッシュヒット
10. アーティファクトダウンロードを伴うリモート実行
```

## 測定指標

以下を収集します：

```text
wall_time_ms
cpu_time_ms
planner_time_ms
graph_load_time_ms
action_count_total
action_count_executed
action_count_cached
action_count_pruned
cache_hit_rate
cas_upload_bytes
cas_download_bytes
materialized_output_bytes
critical_path_ms
worker_queue_ms
selected_test_count
skipped_test_count
missed_test_failures
```

## ベースライン

以下と比較します：

```text
ナイーブなフルリビルド
Bazelローカルキャッシュ
Bazelリモートキャッシュ
Bazelリモート実行
利用可能ならBuck2
JSワークスペース用Nx/Turborepo
C/C++ワークスペース用Ninja
```

## 公平性の確保

同じ条件を使用してください：

```text
ソースグラフ
コンパイラコマンド
マシン
並列度
キャッシュ状態
出力要件
```

比較しないでください：

```text
FrostBuildのウォームキャッシュ
vs
Bazelのコールドキャッシュ
```

## このPOCのベンチマーク方法

このPOCは合成グラフを使用します：

```text
20の独立したパッケージ
各パッケージに8つのモジュール
パッケージのヘッドに依存するアプリターゲット1つ
合計161のビルドターゲット
```

1つのリーフモジュールの変更は次に影響します：

```text
1つのパッケージ内の8モジュール
+ アプリ
= 9つのビルドターゲット
```

したがって、プランナーは次のように剪定します：

```text
161 - 9 = 152ターゲット
```

実行コマンド：

```bash
python3 frost.py bench --workspace sample --jobs 8
```

これにより比較されるのは：

```text
マイクロパーティションの段階的ビルド
vs
すべてのビルドターゲットのナイーブなフルリビルド
```

これはシミュレーションベンチマークです。剪定戦略を証明するものであり、コンパイラの速度を測るものではありません。

## Bazelとの比較方法

Bazelがインストールされている場合：

```bash
bash scripts/compare_bazel.sh
```

サンプルワークスペースには以下が含まれます：

```text
sample/MODULE.bazel
sample/BUILD.bazel
sample/tools/gen.py
```

Bazelとの比較は任意です。なぜなら、このzipはBazelがなくても動作する必要があるからです。

## 標準的なNinja / Makeのベースラインハーネス

比較対象が従来のタイムスタンプベースのビルダーの場合は`frost-bench`を使用します：

```bash
./frost-bench run --suite standard --tools ninja,make --sizes 1000,10000 --iterations 5 --jobs 8
```

標準スイートは各ツールとサイズに対して同一のチェーン状ワークスペースを生成します。以下のタイミングの中央値を記録します：

```text
クリーン
無操作
段階的リーフ
ホットヘッダー
キャッシュヒット再ビルド
```

`cache_hit_rebuild`は、外部のコンテンツアドレス化されたアクションキャッシュをこれらのツールに追加しないため、NinjaとMakeでは適用外です。この設定により、レポートの正確性を保ちつつ、FrostBuildやリモートキャッシュランナーのシナリオに対応します。

レポートにはホスト名、プラットフォーム、Pythonバージョン、CPUコア数、負荷平均、CPUガバナー、ターボ状態が含まれます。`--out bench/baselines/<日付>-<ホスト>.json`を使用して再現可能なベースラインアーティファクトを保存してください。

クリーンなクローンから、すべての現在のベンチマークレポートを次のコマンドで実行します：

```bash
scripts/reproduce.sh
```

このスクリプトは`bench/results/`にタイムスタンプ付きのレポートを書き込み、`frost.py bench`を実行する前にサンプルのFrost POCワークスペースを再生成します。これにより、古いローカル出力に依存しません。

## 現在のベースライン

コミット済みの`bench/baselines/2026-07-05-E14.json`は、2026年7月5日に8ジョブ、CPUガバナー`performance`、ターボ有効状態で取得されました。

ミリ秒単位の中央値タイミングは次の通りです：

```text
ツール   サイズ   クリーン   無操作   段階的リーフ   ホットヘッダー
ninja  1000   1065.252   5.867   7.519            1041.167
make   1000   1229.647   129.719 126.531          1266.797
ninja  10000  11655.407  49.755  57.099           11618.390
make   10000  30857.041  2104.566 2144.258        31991.726
```

この合成チェーンでは、Ninjaは無操作と1リーフ段階的チェックにおいてMakeよりもはるかに高速です。フルチェーンのクリーンやホットヘッダーの再ビルドは、アクションのファンアウトによる影響が大きく、これらはFrostBuildが剪定やキャッシュ、より粗いアクションバッチングによって避ける必要があるシナリオです。

生成された10kワークスペースにおけるNinjaの無操作分解は`ninja -d stats`で取得されました（このマシンでは`strace`が利用できなかったため）：

```text
.ninja parse      1       11.0 ms
.ninja_log load   1        5.0 ms
.ninja_deps load  1        0.0 ms
node stat         20003  176.8 ms
```

重要なポイントは、無操作コストは主に依存グラフの読み込みとファイルシステムのメタデータチェックに起因するということです。FrostBuildの無操作パスは、グラフのロード時間と`stat`やキャッシュルックアップの回数をアクション実行時間とは別に追跡すべきです。

## ベンチマークを実環境にする方法

シミュレートされた`.fb`ソースを実アダプタに置き換えます：

```text
TypeScript:
  tsserverやswcを使ったインポートのパース
  tsc/esbuild/bunでビルド

Rust:
  cargo metadataのパース
  crateごとまたはより細かいrustcユニットでcargo check/buildを使用

Go:
  go list -deps -jsonを使用
  パッケージレベルのアクションを使用

Python:
  インポートとpytestの収集をパース

Docker:
  レイヤーをアーティファクトのパーティションとして扱う
```

その後、実際のモノレポでベンチマークを行います。