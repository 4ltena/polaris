# polaris Codex プロバイダ 設計

2026-08-18

## 目的

polaris が ChatGPT のサブスクリプション認証でモデルへ繋がるようにする。現行の
`OpenAiProvider` は OpenAI 互換の `/chat/completions` に API キーで投げるため、
キーを持たない利用者は polaris を一度も実モデルに繋げられない。M1 の受け入れ
基準のうち唯一未確認のまま残っている「実キーでの一発実行」は、これで満たす。

このマイルストーンを **M2.5** と呼び、v1.0.0 (`hamal`) のタグ付けはこの完了を
待つ。M1 と M2 が揃った時点で 1.0.0 という当初の方針を1段ずらすのは、一度も
実モデルに繋いだことのないハーネスに 1.0.0 を打つと、看板と中身がずれるため
である。

## 前提と根拠

OpenAI は Codex for Open Source の案内で、第三者クライアントを名指ししている。

> "Developers should code in the tools they prefer, whether that's Codex,
> OpenCode, Cline, pi, OpenClaw, or something else, and this program supports
> that work."

したがって Codex 以外のクライアントがサブスクリプションの sign-in を用いる
こと自体は、回避でも逸脱でもない。実装に要する値は、逆解析ではなく OpenAI が
配布している codex CLI（本設計時点で 0.147.0）と公開実装から得た。

| 項目 | 値 |
| --- | --- |
| 認証発行者 | `https://auth.openai.com` |
| 認可 / トークン / 失効 | `/oauth/authorize`、`/oauth/token`、`/oauth/revoke` |
| client_id | `app_EMoamEEZ73f0CkXaXp7hrann` |
| PKCE | S256 |
| scope | `openid profile email offline_access` |
| コールバック | `http://localhost:1455/auth/callback` |
| API | `https://chatgpt.com/backend-api/codex/responses` |
| ヘッダ | `Authorization: Bearer <access_token>`、`chatgpt-account-id` |
| モデル | 設計時にバイナリの文字列から拾った `gpt-5.1-codex-max`・`gpt-5.2-codex`・`gpt-5.3-codex` は、後の実機検証で実カタログに1つも存在しないと判明した（400、ChatGPT アカウントでの Codex 利用は未サポートと明確に拒否。認証自体は通っていた）。実際に有効な名前は `codex debug models` の実カタログでのみ確認できる。実装は `gpt-5.6-sol` を既定とする |

コールバックのポート 1455 は client_id に登録済みの redirect_uri であり、変更
できない。したがって polaris の login はこのポートを掴む必要があり、
`codex login` とは同時に走らない。これは回避策のある制約ではなく、設計が受け
入れる制約である。

## 非目標

- 端末へのストリーミング表示。SSE は内部で畳み、`Provider` は非ストリーミングの
  ままとする。表示は M5 の TUI が扱う
- モデルカタログの取得。モデル名は環境変数で受ける
- 複数アカウントの切り替え
- `~/.codex/` への読み書き。**一切触れない**
- `previous_response_id` による会話の再利用。毎ターン全文を送る

## 構成

新しいクレート `polaris-auth` を追加し、既存の `polaris-provider` に
`codex.rs` を追加する。認証の寿命管理とワイヤ形式の変換は別の責務であり、
壊れ方も、テストの当て方も違う。認証は時計とファイルシステムとネットワークの
競合で壊れ、変換はワイヤ形式の解釈で壊れる。同じクレートに置くと、どちらの
欠陥かが見えにくくなる。M2 でサンドボックスを独立させたのと同じ判断である。

### `polaris-auth`

OAuth の寿命と資格情報の保管だけを持つ。Responses API も polaris の
`Provider` トレイトも知らない。

- `Credentials { access_token, refresh_token, account_id, expires_at }`
- `login() -> Result<Credentials>` — PKCE の verifier と challenge を生成し、
  `127.0.0.1:1455` を bind してからブラウザで認可 URL を開く。`/auth/callback`
  で `code` と `state` を受け、`state` が一致しなければ拒否する。`/oauth/token`
  で交換し、保存して返す
- `load() -> Result<Option<Credentials>>`
- `ensure_fresh(&Credentials) -> Result<Credentials>` — 期限まで余裕が無ければ
  `refresh_token` で更新し、保存して返す
- `logout()` — 保管したファイルを削除する

保管先は `~/.polaris/auth.json`、パーミッションは 0600。書き込みは一時ファイル
へ書いてから rename する。途中で落ちても、旧ファイルが半端な内容に置き換わる
ことがない。

`~/.codex/auth.json` は読まない。コピーもしない。サーバが refresh token を
ローテーションさせる場合、polaris が更新した瞬間に codex 側の控えが失効しうる。
独立した store を持てば、この事故は原理的に起きない。

### `polaris-provider/src/codex.rs`

`CodexProvider` が `Provider` を実装する。トークンは `TokenSource` トレイト
越しに受け取る。

```rust
#[async_trait::async_trait]
pub trait TokenSource: Send + Sync {
    async fn token(&self) -> Result<Token, ProviderError>;
    async fn refreshed(&self) -> Result<Token, ProviderError>;
}

pub struct Token {
    pub access_token: String,
    pub account_id: String,
}
```

`TokenSource` は `polaris-provider` に置き、`polaris-auth` を用いる実装は
`polaris-cli` に置く。こうすると `polaris-auth` が `polaris-provider` へ依存
せず、依存の向きが一方向に保たれる。同時に、プロバイダのテストが OAuth も
ファイルもブラウザも要らなくなる。

`polaris-cli` の実装は `token()` で `load()` の結果に `ensure_fresh()` を適用し、
`refreshed()` では期限に関わらず更新する。期限の判定は `polaris-auth` の内側に
閉じ、プロバイダは「いま使えるトークン」だけを見る。トークン応答が `expires_in`
を持たない場合は、期限が不明であるとして毎回更新する。楽観的に使い回すと、
失効したトークンでの 401 が通常経路になる。

## ワイヤ形式

要求本文は Responses API の形をとる。

```json
{
  "model": "<POLARIS_MODEL>",
  "instructions": "<CompletionRequest.system>",
  "input": [ ... ],
  "tools": [ ... ],
  "store": false,
  "stream": true
}
```

`store` を偽にして毎ターン全文を送る。接頭辞を動かさないことでプロンプト
キャッシュを保つという polaris の方針と一致し、サーバ側に会話状態を持たせない
ぶん、送るものと測るものが一致する。

### メッセージの変換

| polaris の `Message` | Responses の `input` 要素 |
| --- | --- |
| `Role::User` | `{"type":"message","role":"user","content":[{"type":"input_text","text":…}]}` |
| `Role::Assistant`（本文） | `{"type":"message","role":"assistant","content":[{"type":"output_text","text":…}]}` |
| `Role::Assistant`（`tool_calls`） | 各呼び出しにつき `{"type":"function_call","call_id":…,"name":…,"arguments":"<JSON 文字列>"}` |
| `Role::Tool` | `{"type":"function_call_output","call_id":…,"output":…}` |

`arguments` は JSON そのものではなく JSON を収めた文字列である。polaris の
`ToolCall.arguments` は `serde_json::Value` なので、送出時に文字列化し、受信時に
解釈する。本文と `tool_calls` の両方を持つアシスタントのターンは、`message`
要素と `function_call` 要素の両方を、この順で並べる。

### ツール定義の変換

`/chat/completions` は関数を入れ子にする（`{"type":"function","function":{…}}`）
が、Responses は平坦である。

```json
{"type": "function", "name": …, "description": …, "parameters": …}
```

`polaris_tools::ToolSpec` から直接組み立てる。既存の
`openai::tool_wire_shape` とは別の関数にする。片方を直すともう片方が黙って
壊れる形にしない。

### 応答の畳み込み

`Accept: text/event-stream` で受け、既存の `sse::SseDecoder` でフレームに分ける。
デコーダは push 境界での分割と CRLF を既に扱っており、そのまま使う。

意味論は次のとおり。

- `response.output_item.done` — 確定したアイテムを1件受け取る。`message` なら
  本文へ連結し、`function_call` なら `ToolCall` へ積む
- `response.completed` — ここで終わる
- `response.failed` — 本文を運んでエラーにする
- `response.cancelled` — エラーにする
- それ以外（`delta` を含む）— 読み飛ばす

差分（delta）を再結合しないのは、確定したアイテムだけを見れば同じ結果が得られ、
再結合の失敗という壊れ方を丸ごと持ち込まずに済むためである。端末へ逐次表示
する必要が生じたときに、はじめて delta を見る。

## エラー処理

認証の失敗がモデルの失敗に見えてはならない。M2 で「サンドボックスの適用失敗」と
「サンドボックスの拒否」を別事象にしたのと同じ理由である。守っていない状態が
守っている状態と同じ見た目になると、原因に辿り着けない。

`ProviderError` に `Auth(String)` を追加する。

| 事象 | 扱い |
| --- | --- |
| 未ログイン | `Auth`。`polaris login` を名指しする。HTTP エラーに混ぜない |
| 401（1回目） | `TokenSource::refreshed()` で強制更新し、**1回だけ**再試行する |
| 401（2回目） | `Auth`。再ログインを促す |
| 更新自体の失敗 | `Auth`。再ログインを促す |
| 429 | `Http`。バックエンドが返すリセット情報を文面へ含める |
| `response.completed` を見ずに終了 | `Decode`。硬い失敗にする |
| `response.failed` | `Http`。運ばれた本文を含める |

`response.completed` を見ないまま `text` が空で `tool_calls` も空、という応答を
正常終了として返してはならない。エージェントループはツール呼び出しが無いことを
「完了」と読むため、黙って空の最終回答を返す。M1 が `openai.rs` で塞いだのと
同じ穴である。

タイムアウトは応答全体ではなく**無通信**で測る。SSE では応答全体の長さが正常に
伸びるため、全体で測ると正常な長考を打ち切る。既存の `OpenAiProvider` が持つ
読み取りタイムアウトと同じ形をとる。

## CLI

`POLARIS_PROVIDER` が `openai` と `codex` をとる。既定は `openai` であり、
現行の挙動は変わらない。`POLARIS_MODEL` は共用し、`codex` のときの既定を
`gpt-5.6-sol` とする（実機検証で判明した実カタログの値。上の「前提と根拠」
節を参照）。

`reasoning.effort` は `chatgpt_plan_type`（`access_token` の claim）から
決める。`plus` は `low`、`pro` で始まる値（`pro`・`prolite` 等、Pro の
利用量ティアは `plan_type` だけでは区別できないため一律）は `xhigh`。
どちらにも当たらない値や取得できない場合は `reasoning` キー自体を送らず、
サーバの既定に委ねる。

サブコマンド `login` と `logout` を追加する。ここには既知の罠がある。現行の
`--prompt` は `required_unless_present = "confined_apply"` であり、M2 の Task 8
では `--confined-apply` が clap の必須検証に阻まれて到達不能になっていた。同じ
形であるから、**サブコマンドが実際に到達することを固定するテストを置く**。
引数の組み立てを目で読んで正しく見えることは、到達することの証拠にならない。

ポート 1455 が塞がっている場合は、汎用の bind エラーではなく `codex login` との
衝突を名指しする。原因を掴めない拒否メッセージは同じ失敗の反復を招く。

常時コンテキストへの影響は無い。プロバイダはシステムプロンプトにもツール定義
にも現れない。予算のテストは値を変えずに通るはずであり、通らなければ何かを
取り違えている。

## テスト

### `polaris-auth`

- PKCE の verifier から challenge を導く計算を、既知のベクタで固定する
- `state` が一致しないコールバックを拒否する
- 保管したファイルのパーミッションが 0600 である
- rename の前に落ちても旧ファイルが無傷である
- 期限の余裕の境界。余裕の内側では更新し、外側では更新しない
- 更新が失敗したとき、再ログインを促すエラーになる
- ポート 1455 が既に使われているとき、衝突を名指しするエラーになる

### `polaris-provider/src/codex.rs`

`wiremock` に定型の SSE を置いて確かめる。

- 本文だけの応答
- ツール呼び出しを含む応答
- **イベントの途中で分割されたフレーム**。分割に耐えることは `sse.rs` の
  責任だが、この経路が実際にそれを通っていることは別に確かめる
- `response.completed` を見ないまま終わる応答 → エラー
- `response.failed` → 本文を運ぶエラー
- 401 のあと更新して再試行し、成功する
- 401 が 2 回続く → `Auth`。再試行は 1 回で止まる
- 送出した要求本文が Responses の形をしていること。`tools` が平坦であること、
  `arguments` が文字列であること、`store` が偽であること

否定を確かめるテストには、同じテストの中に肯定の対照を置く。M2 では、すべてを
拒否する実装が拒否のテストを通り、起動すらできないヘルパが「範囲外へ書かなかった」
テストを通った。片側だけのテストは、対象が何もしなくても通る。

**実 API 呼び出しを行うテストと、実資格情報を読むテストは 1 本も作らない。**
トークンは `TokenSource` のスタブが供給する。

## 受け入れ基準

1. `polaris login` がブラウザでの認可を経て完了し、`~/.polaris/auth.json` が
   0600 で作られる。`~/.codex/auth.json` は変更されない
2. `POLARIS_PROVIDER=codex polaris -p "Cargo.toml は何行か"` が、実際の
   バックエンドから行数を含む答えを返す
3. `access_token` の期限が切れた状態から始めても、更新を経て 2 が成立する
4. ログアウト状態で 2 を実行すると、`polaris login` を名指しするエラーが出る。
   HTTP エラーにはならない
5. `POLARIS_PROVIDER` を設定しない既定の実行が、これまでと同じく
   `OpenAiProvider` を使う

1 と 2 と 3 は実際の資格情報を要するため、無人では確認できない。確認する手順は
README に記す。4 と 5 はテストで固定する。

## 保証しない範囲

- サブスクリプションの利用条件そのもの。Codex for Open Source の便益は譲渡
  できないと明記されており、その適格性は利用者が負う
- バックエンドのプロトコルが変わらないこと。`/backend-api/codex/responses` は
  OpenAI が第三者クライアント向けに安定を約束した公開 API ではない。壊れたら
  直す。壊れていることが分かる形にしておくことが、ここでの設計上の義務である
- 1455 が使えない環境での login。ポートは client_id に紐づいており、選び直せない
