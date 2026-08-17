# polaris

polaris はコーディングエージェントの harness である。最大の制約は、毎ターン
モデルへ送る常時コンテキストを 990 トークン以下に保つことにあり、この数値は
見積ではなく `tiktoken_rs::o200k_base()` による実測で担保する。

M1（このマイルストーン）はヘッドレスのワンショットエージェントである。
パスポリシー付きの `read` ツール、OpenAI 互換プロバイダ、伏字化付きの
追記専用監査ログ、量的な停止条件、エージェントループ、CLI から成る。指示を
1つ渡すと、必要なら `read` でファイルを読みながら答えを返し、終了する。
対話や複数ターンにまたがるセッションの再開は持たない。

## ビルド

Rust 1.96.0（`rust-toolchain.toml` で固定）、edition 2024 を使う。

```
cargo build --release
```

バイナリは `target/release/polaris` に生成される。

## 環境変数

| 変数 | 既定値 | 意味 |
| --- | --- | --- |
| `POLARIS_API_KEY` | なし（必須） | OpenAI 互換エンドポイントの API キー。未設定だと起動直後に失敗する。 |
| `POLARIS_BASE_URL` | `https://api.openai.com/v1` | チャット補完エンドポイントのベース URL。OpenAI 互換の別エンドポイントに差し替えられる。 |
| `POLARIS_MODEL` | `gpt-5.4` | 使用するモデル名。 |

`polaris --help` にも同じ内容を載せてある。

## 監査ログ

`--audit <path>` で出力先を指定できる。省略すると
`~/.polaris/state/<project-id>/audit.jsonl` に書く。`<project-id>` は作業
ディレクトリの canonicalize 済みパスから求める識別子で、作業ディレクトリの
外（ホーム配下の状態ディレクトリ）に書くのは、リポジトリを `git add -A`
した瞬間に伏字化の取りこぼしがコミットへ混ざるのを避けるためである。1 回の
ツール呼び出しにつき 1 行の JSON を追記し、書き込む前に必ず秘密情報の伏字化
を通す。

## 実行

```
POLARIS_API_KEY=sk-... polaris -p "Cargo.toml は何行か"
```

`cargo build --release` の前であれば `cargo run -p polaris-cli --` の後に
同じ引数を続けてもよい。これが M1 の受け入れ基準そのものであり、動けば
`Cargo.toml` の行数を含む答えを標準出力へ返す。
