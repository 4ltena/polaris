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
| `POLARIS_PROVIDER` | `openai` | `openai` か `codex`。`codex` は `polaris login` で得た ChatGPT のサブスクリプション認証を使い、API キーを要しない。 |
| `POLARIS_API_KEY` | なし | `POLARIS_PROVIDER=openai` のとき必須。OpenAI 互換エンドポイントの API キー。 |
| `POLARIS_BASE_URL` | `https://api.openai.com/v1` | `POLARIS_PROVIDER=openai` のときのベース URL。 |
| `POLARIS_MODEL` | `gpt-5.4` / `gpt-5.3-codex` | 使用するモデル名。既定はプロバイダごとに異なる。 |

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

## ChatGPT のサブスクリプションで使う

API キーを持たない場合は、ChatGPT の認証で繋げる。

```
polaris login
POLARIS_PROVIDER=codex polaris -p "Cargo.toml は何行か"
```

`polaris login` はブラウザを開き、`http://localhost:1455/auth/callback` で
認可を受け取る。このポートは登録済みの redirect_uri のものなので選び直せず、
`codex login` とは同時に走らない。資格情報は `~/.polaris/auth.json` に 0600 で
保管する。`~/.codex/` には読み書きとも触れない。

`polaris logout` で保管した資格情報を消す。

### 保管先とパーミッションを手で確かめる

実際の資格情報を要するため、ここから先は自動テストで固定できない。手で追う。

`polaris login` を実行する前に、codex 側の資格情報の状態を控える。

```
shasum -a 256 ~/.codex/auth.json 2>/dev/null || echo "codex 側の資格情報は無い"
```

Linux では `sha256sum` を使う。`polaris login` のあとに同じコマンドを実行し、
出力が前と一致することを見る。`codex login` を一度も実行していなければ、
前後とも「無い」と表示される。それも正しい状態である。

polaris 側の保管先は次で見る。

```
ls -l ~/.polaris/auth.json
```

`-rw-------` であること。group と other に権限が残っていれば、同じホストの
別のユーザーがアクセストークンを読める。

### 期限切れからの更新を手で確かめる

`~/.polaris/auth.json` の `expires_at` を、過去の Unix 秒か `null` へ書き換える。
期限が不明な資格情報は毎回更新する扱いなので、どちらでも更新経路へ入る。行ごと
消してはいけない。`expires_at` は最後の項目であり、直前の行に余分なカンマが
残って JSON が壊れる。

その状態でもう一度実行する。

```
POLARIS_PROVIDER=codex polaris -p "Cargo.toml は何行か"
```

答えが返れば、`refresh_token` での更新を経てバックエンドへ到達している。
`polaris login` を促すエラーが出た場合は、更新そのものが失敗している。

更新のあとの `~/.polaris/auth.json` も見る。`expires_at` が未来の値へ進んで
いれば、更新結果が書き戻っている。`account_id` が空文字へ変わっていないこと、
パーミッションが `-rw-------` のままであることも併せて確かめる。
