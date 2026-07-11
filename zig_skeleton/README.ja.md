<!-- i18n: language-switcher -->
[English](README.md) | [日本語](README.ja.md)

# Zig スケルトン

これは将来の Zig 実装のための非実行設計スケルトンです。

実行可能な POC は `../frost.py` の Python プロトタイプです。

推奨される今後のコマンド：

```bash
zig build run -- plan --workspace ../sample
```

Zig を選ぶ理由：

```text
- 起動オーバーヘッドが低い
- ガベージコレクターの一時停止がない
- シンプルな静的バイナリ配布
- 明示的なメモリと入出力の制御
```

推奨アプローチ：

```text
1. アルゴリズムの反復には Python プロトタイプを維持
2. 安定した frost.json/frost.toml モデルを定義
3. プランナー + キャッシュ + スケジューラーを Zig に移植
4. 最初は言語解析ツールをプラグインとして維持
```