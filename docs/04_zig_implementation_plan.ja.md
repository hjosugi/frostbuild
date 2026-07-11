<!-- i18n: language-switcher -->
[English](04_zig_implementation_plan.md) | [日本語](04_zig_implementation_plan.ja.md)

# Zig実装計画

## Zigは良い選択か？

はい、Zigは小さく高速で予測可能なバイナリを目指す場合、エンジンのコアとして妥当な選択です。

良い点：

```text
- GCなし
- 高速な起動
- 明示的なメモリ管理
- シングルバイナリ配布が容易
- Cとの連携が良好
- IOやハッシュの制御が良好
- スケジューラ／キャッシュ／インデクサエンジンに適している
```

リスク：

```text
- RustやGoよりエコシステムが小さい
- リモート実行プロトコル用の成熟したライブラリが少ない
- チームの採用が難しい可能性
- async／runtimeエコシステムはGoやRustほど標準化されていない
```

## 推奨される分割

最初からすべてをZigで実装しない。

```text
Zig:
  コアエンジン
  グラフプランナー
  ハッシュ／CASコード
  ローカルスケジューラ
  ファイルスキャナ
  CLIバイナリ

Python／TypeScript／Rustプラグイン:
  言語解析
  インポートグラフ抽出
  テスト検出
  フレームワーク固有の統合

Nix:
  ツールチェーン環境層

REAPI／gRPCサイドカー:
  Zig gRPCが面倒になった場合のリモート実行プロトコルサポート
```

## なぜすぐに純粋なZigにしないのか？

難しいのは速度ではなく正確さです。

```text
- 依存関係推論
- 動的依存関係
- テスト選択の安全性
- ルールエコシステム
- リモートキャッシュの互換性
- サンドボックスの挙動
```

まずPythonでアルゴリズムを証明し、その後ホットパスを移植します。

## データ構造

コア構造体：

```zig
const Partition = struct {
    id: []const u8,
    kind: Kind,
    src: []const u8,
    deps: []const []const u8,
    reverse_deps: []const []const u8,
    output: []const u8,
    source_hash: [32]u8,
    toolchain_hash: [32]u8,
    last_duration_ms: u64,
};

const ActionKey = struct {
    digest: [32]u8,
};

const ActionResult = struct {
    action_key: ActionKey,
    output_digest: [32]u8,
    status: Status,
};
```

## Zig MVPのマイルストーン

```text
M1:
  simple frost.jsonのパース
  グラフ構築
  変更されたソースハッシュの検出
  影響範囲の計画生成

M2:
  ローカルCAS
  アクションキャッシュ
  並列スケジューラ

M3:
  実行サンドボックスラッパーの処理
  depfileリーダー
  JSONイベントログ

M4:
  Nix環境ハッシュの統合
  リモートキャッシュクライアント

M5:
  REAPIリモート実行クライアント
```

## CLIの形状

```bash
frost init
frost plan //app
frost build //app --jobs 16
frost test //app --affected
frost bench --baseline bazel
frost explain //app --why-built
frost query 'changed(src/pkg05_mod07) -> affected_tests'
```

## スケルトン

`zig_skeleton/src/main.zig`を参照してください。

これは意図的にスケルトンのみです。なぜなら、この環境にはZigコンパイラが含まれていないからです。実行可能なPOCは`frost.py`です。