<!-- i18n: language-switcher -->
[English](03_papers_and_references.md) | [日本語](03_papers_and_references.ja.md)

# 論文と参考文献

## ビルドシステムの多彩なアプローチ

著者：

```text
アンドレイ・モホフ、ニール・ミッチェル、サイモン・ペイトン・ジョーンズ
```

重要性：

```text
- ビルドシステムを比較するためのフレームワークを提供
- スケジューラ、リビルダー、依存関係モデル、ストアモデルを分離
- ビルドシステムを再結合可能なコンポーネントとして示す
- 動的依存関係やクラウドビルドについて議論
```

FrostBuildへの応用：

```text
Nixの正確性、Bazelのクラウドビルド、動的依存関係サポートを組み合わせる理論的基盤として利用。
```

参考文献：

- https://www.microsoft.com/en-us/research/wp-content/uploads/2018/03/build-systems.pdf
- https://dl.acm.org/doi/10.1145/3236774
- https://simon.peytonjones.org/assets/pdfs/build-systems-jfp.pdf

## Pluto：動的依存関係を持つ健全かつ最適な増分ビルドシステム

重要性：

```text
- 健全かつ最適な増分再構築に焦点を当てる
- 動的依存関係をサポート
- ビルドの要約と必要条件・成果物を記録
```

FrostBuildへの応用：

```text
動的依存関係の記録と検証を用いて、マイクロパーティションの剪定を安全に保つ。
```

参考文献：

- https://www.pl.informatik.uni-mainz.de/files/2019/04/pluto-incremental-build.pdf

## 回帰テスト選択の研究

キーワード：

```text
コード変更による影響を受けるテストだけを実行し、失敗を見逃さないようにする。
```

重要なアプローチ：

```text
静的RTS：
  静的プログラム解析による依存関係の推定

動的RTS：
  以前のテスト実行から依存関係を収集

ハイブリッドRTS：
  両者を組み合わせる
```

FrostBuildへの応用：

```text
ビルドシステムに対応した多言語RTSは、FrostBuildがポリグロットのモノレポを対象とするため直接関係する。
```

参考文献：

- CIにおけるビルドシステム対応の多言語回帰テスト選択： https://mediatum.ub.tum.de/doc/1656311/1656311.pdf
- 静的RTSの研究： https://www.cs.cornell.edu/~legunsen/pubs/LegunsenETAL16StaticRTSStudy.pdf
- CIにおけるRTS： https://mir.cs.illinois.edu/marinov/publications/ShiETAL19RTSinCI.pdf
- STARTS静的RTSデモ： https://mir.cs.illinois.edu/awshi2/publications/ASEDEMO2017.pdf
- ハイブリッドRTS 2024： https://zbchen.github.io/files/ase2024.pdf

## 予測的テスト選択

Metaが予測的テスト選択に関する研究を公開。

FrostBuildへの応用：

```text
確率的テスト選択はあくまで高速化のためのオプションモードとして利用。
安全モードは保守的に保つべき。
```

参考文献：

- https://research.facebook.com/publications/predictive-test-selection/

## Bazelリモート実行とキャッシュ

重要性：

```text
- アクションキャッシュ
- 内容アドレス指定ストレージ
- リモート実行API
- 分散テスト・ビルドアクション
```

FrostBuildへの応用：

```text
まずリモート実行プロトコルを新たに考案せず、可能であればBazelリモート実行APIとの互換性から始める。
```

参考文献：

- https://bazel.build/remote/caching
- https://bazel.build/versions/8.2.0/remote/rbe
- https://github.com/bazelbuild/remote-apis
- https://buf.build/bazel/remote-apis/docs/main:build.bazel.remote.execution.v2

## Buck2

重要性：

```text
- 強力な現代的ベースライン
- Rustエンジン
- 単一の増分依存グラフ
- 動的依存関係
- 実際にはBuck1より2倍高速
- 遅延マテリアライゼーション
```

FrostBuildへの応用：

```text
Buck2は最も重要な設計競合相手。
```

参考文献：

- https://github.com/facebook/buck2
- https://engineering.fb.com/2023/04/06/open-source/buck2-open-source-large-scale-build-system/
- https://buck2.build/docs/about/why/
- https://buck2.build/docs/users/advanced/deferred_materialization/
- https://github.com/facebookincubator/buck2-change-detector

## Nixと再現性のあるビルド

重要性：

```text
- 正確な環境が正しいキャッシュキーに影響
- ビルドの隔離により未宣言の依存関係を防止
- 再現性は単なるキャッシュ以上の強い性質
```

参考文献：

- https://nixos.org/
- https://nix.dev/manual/nix/2.25/advanced-topics/diff-hook
- https://reproducible-builds.org/docs/definition/

## Snowflakeマイクロパーティション

重要性：

```text
- メタデータ駆動の剪定
- 高価な処理前に不要なデータをスキップ
```

FrostBuildへの応用：

```text
ビルド・テスト・アーティファクト単位をメタデータ付きのパーティションとして扱い、メタデータカタログを用いてスケジューリング前に作業を剪定。
```

参考文献：

- https://docs.snowflake.com/en/user-guide/tables-clustering-micropartitions

## ビルドパフォーマンスの測定

重要性：

```text
測定しなければ改善できない。
```

追跡項目：

```text
- グラフの読み込み時間
- 計画時間
- アクション数
- 実行されたアクション数
- キャッシュヒット数
- キャッシュ探索遅延
- アーティファクトダウンロードサイズ
- 出力のマテリアライゼーション時間
- クリティカルパス長
- ワーカーキュー待ち時間
- テスト選択/スキップ比率
- テスト選択の偽陰性率
```

参考文献：

- https://bazel.build/advanced/performance/build-performance-breakdown
- https://bazel.build/advanced/performance/build-performance-metrics