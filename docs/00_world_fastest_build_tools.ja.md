<!-- i18n: language-switcher -->
[English](00_world_fastest_build_tools.md) | [日本語](00_world_fastest_build_tools.ja.md)

# 現在の高速ビルドツールの見取り図

## 結論

「世界で一番速いビルドツール」は、ワークロードによって変わる。

```text
大規模多言語モノレポ:
  Buck2 / Bazel / Pants / Please

C/C++ローカルインクリメンタルビルド:
  Ninja

JavaScript/TypeScriptモノレポ:
  Turborepo / Nx

再現性のある環境/パッケージビルド:
  Nix
```

単独で万能の1位はない。理由は、ビルドパフォーマンスは次の要素で決まるから。

```text
1. クリーンビルドかインクリメンタルビルドか
2. ローカルのみかリモート実行か
3. 大きなリンカーアクション1つか、多数の小さな並列アクションか
4. 言語エコシステム
5. キャッシュヒット率
6. 依存グラフの正確さ
7. テスト選択の質
8. アーティファクトのダウンロード/マテリアライズコスト
```

## Buck2

Buck2は、大規模モノレポのインクリメンタルビルドにおいて最も有力な候補の一つ。

重要なアイデア:

```text
- Rust実装
- 単一のインクリメンタル依存グラフ
- より多くの並列性
- 動的依存関係
- リモート実行サポート
- 遅延マテリアライズ
```

Metaによると、Buck2は実践的にはBuck1より最大2倍高速だという。これは、Buck2が常にBazelより2倍速いという意味ではないが、ビルドエンジンのアーキテクチャだけでも大きな差を生むことを示している。

参考文献:

- https://github.com/facebook/buck2
- https://engineering.fb.com/2023/04/06/open-source/buck2-open-source-large-scale-build-system/
- https://buck2.build/docs/about/benefits/compared_to_buck1/
- https://buck2.build/docs/users/advanced/deferred_materialization/

## Bazel

Bazelは、大規模な多言語ビルドのための最も確立された基準点。

重要なアイデア:

```text
- 明示的な依存グラフ
- 並列実行
- ローカルキャッシュ
- リモートキャッシュ
- リモート実行
- アクションキャッシュ + 内容アドレス指定ストア
- 豊富なルールエコシステム
```

Bazelは正確さやエコシステム面で他を圧倒しやすい。速度面で勝つには、新しいツールはより鋭い剪定層、より良いスケジューラ、または一般的なインクリメンタルケースのオーバーヘッド低減が必要。

参考文献:

- https://bazel.build/
- https://bazel.build/remote/caching
- https://bazel.build/versions/8.2.0/remote/rbe
- https://github.com/bazelbuild/remote-apis

## Ninja

Ninjaは、意図的にシンプルなため非常に高速。CMakeやMesonのような上位ツールがビルドファイルを生成することを前提としている。

キーポイント:

```text
とにかくシンプルに
グラフの読み込みを高速に
生成されたファイルが指示する通りに実行
```

小規模なローカルインクリメンタルC/C++ビルドでは勝ちやすいが、リモート実行やパッケージ環境管理、クロス言語のテスト選択といったモノレポプラットフォームとしては不十分。

参考文献:

- https://ninja-build.org/
- https://ninja-build.org/manual.html

## Nx / Turborepo

NxとTurborepoは、JS/TSのモノレポにおいて強力。

共通のアイデア:

```text
- タスクグラフ
- 影響範囲の検出
- 並列タスク実行
- ローカル/リモートキャッシュ
```

実用的で導入も容易だが、一般的にはプロジェクトやタスクレベルで動作し、すべての言語の深いコンパイラレベルやマイクロパーティションレベルまで深く操作するわけではない。

参考文献:

- https://nx.dev/
- https://turborepo.dev/

## Nix

Nixは、主に高速ビルドオーケストレーターではない。その最大の価値は、ビルド環境の正確さと再現性。

キーポイント:

```text
パッケージを孤立してビルド
未宣言の依存関係を避ける
環境を宣言的に作る
```

FrostBuildにおいては、Nixはビルドスケジューラ全体ではなく、環境やツールチェーン層として最適。

参考文献:

- https://nixos.org/
- https://nix.dev/manual/nix/2.18/command-ref/new-cli/nix3-build

## 実用的なベースライン

私たちの目標にとって、Makeやシェルスクリプトは適切ではない。適切なベースラインは:

```text
リモートキャッシュ/リモート実行付きBazel
リモート実行付きBuck2
JS専用ワークロードにはNx/Turborepo
ローカルC/C++のインナーループにはNinja
```

これらより高速なツールは、次のいずれかの領域で勝つことで実現できる:

```text
1. グラフ評価のオーバーヘッドを減らす
2. 影響範囲検出の精度を高める
3. より積極的だが安全なテスト選択
4. 出力マテリアライズコストを低減
5. キャッシュの局所性スケジューリングを改善
6. 設定のオーバーヘッドを減らす
7. より良いコンパイラ永続ワーカー
8. より正確な環境ハッシュ
```