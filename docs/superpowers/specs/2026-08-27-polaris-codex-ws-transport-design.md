# polaris Codex WebSocket トランスポート(フェーズ1)設計書

## 背景・目的

利用者が指摘した「polarisのraw total(698,358)がcodex純正(86,878)を大幅に上回る」問題を調査し、2件の対処(`read`ツールの説明文改善、commit `8666bdc`・`533b0a1`)で34%削減した。しかし利用者から「キャッシュ比率90%前後も維持しつつraw totalを減らす別の方針」を求められ、`store: false`のまま往復粒度を変える調整では両立が構造的に不可能と判明した(`docs/superpowers/CURRENT.md`「polaris vs codex の生トークン量の差」節参照)。

両立を実現しうる唯一の手段として`store: true` + `previous_response_id`継続を検討したが、実際に`crates/polaris-provider/src/codex.rs`が話している`https://chatgpt.com/backend-api/codex`エンドポイントへ直接`store: true`を送って検証したところ、サーバーから即座に拒否された。

```
{"detail":"Store must be set to false"}
```

upstream `codex-rs`のコード(`core/src/client.rs:899`の`store: provider.is_azure_responses_endpoint()`)と突き合わせると、このバックエンド(Azure以外)では`store`は常に`false`である。それでもupstreamが往復ごとの差分送信を実現しているのは、`store: true`によるサーバー側永続化ではなく、**WebSocket接続自体が持つセッション内の継続状態**(`x-codex-turn-state`によるsticky routing)による。plain HTTPの経路(`stream_responses_api`)には差分送信のロジックが無く、`ModelClientSession`のdocコメントも「同じ接続を再利用することで増分リクエストを送れる」と明記している。

したがって、raw totalをcodex並みに近づけるには、upstreamと同じくWebSocketトランスポートへの移行が要る。この規模はupstreamの圧縮関連実装(10ファイル4,251行)に匹敵しうると判断し、2フェーズに分割する。本設計書はフェーズ1(WebSocketトランスポート自体の導入。観測可能な挙動は変えない)のみを対象とする。フェーズ2(差分送信+`previous_response_id`継続によるraw total削減の実現)は別途設計する。

## スコープ

対象: `crates/polaris-provider/src/codex.rs`(新規WS接続管理コード)、`crates/polaris-core/src/agent.rs`・`spawn.rs`(下記「並行subagentとの整合」節の変更のみ)、`crates/polaris-provider/src/lib.rs`(`Provider`トレイトへの1メソッド追加)。

非対象:
- 差分送信・`previous_response_id`継続(フェーズ2)
- `openai.rs`(Chat Completions APIには対応する概念が無い。トレイト追加メソッドの自明な実装のみ持つ)
- `session.rs`・`compaction.rs`への変更(フェーズ1では不要)
- WSの正確なワイヤ形式(接続URL・アップグレードヘッダ・メッセージ封筒のJSON形状・ping/keepalive)を本設計書で確定させること——ただし、計画書作成の過程で実際にupstream(`codex-api/src/endpoint/responses_websocket.rs`・`codex-api/src/common.rs`・`codex-api/src/sse/responses.rs`)を読み解いてワイヤ形式を確定させた。その結果は実装計画書(`docs/superpowers/plans/2026-08-27-polaris-codex-ws-transport.md`)側に具体的なコードとして記載する。本設計書はアーキテクチャ判断(接続の生存期間・並行subagentとの整合・フォールバック方針)の記録に留める

## アーキテクチャ

### 1. 並行subagentとの整合(最初に解決する前提条件)

`crates/polaris-core/src/agent.rs`の`run`/`run_loop`は、`spawn`ツールの実行時に`provider_pool: Arc<dyn Provider>`をそのまま`spawn::run_wave`へ渡し、複数subagentの`run_loop`が同一のプロバイダインスタンスを共有したまま`futures_util::join_all`で真に並行実行される(`spawn.rs:402`、専用テスト`tasks_within_the_concurrency_limit_run_genuinely_concurrently`)。

upstreamのturn-stateは「同一ターン内の複数リクエストでは使い回すが、別ターンをまたいで使い回すとルーティング事故になる」という制約を持つ。1個の永続WS接続+1個のturn-stateを`CodexProvider`インスタンス単位で持たせると、親の`run_loop`と並行実行される各subagentの`run_loop`が同じ接続・turn-stateを奪い合い、この制約に抵触する。

**対処**: `Provider`トレイトへ、object-safeな新規メソッドを1本、**デフォルト実装付きで**追加する。

```rust
pub trait Provider: Send + Sync {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError>;

    /// Returns a fresh, independently-scoped provider handle for a new
    /// logical turn (a subagent's own `run_loop`), when this provider
    /// holds per-turn connection state that must not be shared across
    /// concurrently-running turns (see `CodexProvider`'s WS transport).
    /// Returns `None` when there's no such state to isolate — the
    /// default, safe for every provider without per-turn connection
    /// state, real or test double alike.
    fn for_new_turn(&self) -> Option<Arc<dyn Provider>> {
        None
    }
}
```

このトレイトの実装はワークスペース全体で14箇所ある(本物2つ`CodexProvider`・`OpenAiProvider`、テスト用モック12個)。デフォルト実装を`None`にすることで、実際に接続状態を分離する必要がある`CodexProvider`だけが上書きすればよく、残り13箇所(`OpenAiProvider`含む)は一切変更不要になる。

- `CodexProvider::for_new_turn`は、同じ`base`/`model`/`effort_override`(現在値のスナップショット)/`tokens`(`Arc<dyn TokenSource>`、共有して問題ない不変ハンドル)/`idle`(タイムアウト設定)を使って新しい`CodexProvider`インスタンスを構築し、`Some(Arc::new(...))`を返す。`client: reqwest::Client`は内部で新規に作る(接続プールを共有する積極的な理由が無く、WS接続自体が独立している以上、道連れで共有する理由も無い)。新インスタンスは自分専用の(まだ確立していない)WS接続状態を持つ。
- `spawn.rs`の`run_wave`は、各subagentタスクを起動する直前に`provider_pool.for_new_turn().unwrap_or_else(|| provider_pool.clone())`を呼び、そのタスク専用のプロバイダハンドルを渡す(`provider_pool`自体をそのまま共有しない)。親の`run_loop`自身は、起動時に受け取った`provider`をそのまま使い続ける(こちらは変更不要)。
- 既存のテスト用`Scripted`等のモックプロバイダにも、この新規メソッドの単純な実装(`Arc::new(self.clone())`相当)を追加する。挙動は変わらないため、既存テストへの影響は無い想定。

### 2. WS接続の生存期間

`CodexProvider`インスタンスの生存期間(1回の`polaris exec`実行、または1回のTUI連続セッション実行——`for_new_turn`で分離されたsubagent用インスタンスの場合はそのsubagent1体の`run_loop`実行)にわたって、WS接続を遅延確立・再利用する。

- 初回の`complete()`呼び出し時に接続を確立する。以降の`complete()`呼び出しは同じ接続を再利用する。
- `x-codex-turn-state`は、接続確立後の最初のレスポンス(正確な取得元はワイヤ形式調査タスクで確定)から取得し、以降の同一インスタンスでのリクエストへ再送する。
- インスタンスがdropされる(プロセス終了、またはsubagentの`run_loop`終了)と接続も閉じる。明示的なcleanupロジックは持たず、`reqwest`のWebSocketクライアント(または採用するcrate)のdrop時の挙動に委ねる。

### 3. HTTPフォールバック

WSアップグレードに失敗した場合(接続拒否、プロキシ等によるブロック、upstreamの`UPGRADE_REQUIRED`相当)は、既存の`reqwest`ベースSSE/HTTP経路(現行の`attempt`関数)へ自動的にフォールバックする。

- フォールバックの判定は接続確立時の1回のみ行い、以降そのインスタンスの生存期間中はフォールバック済みの状態を保持する(毎ターンWSへの再挑戦はしない——失敗が明らかな環境で往復のたびに接続試行のオーバーヘッドを払うのを避けるため)。
- フォールバック中はフェーズ1以前と完全に同じ挙動(現行の`attempt`関数)になる。

### 4. 観測可能な挙動

フェーズ1完了時点で、`store: false`のまま毎ターン全履歴を送る現行の意味論は変えない。トランスポートがHTTP/SSEからWSに変わるだけで、`input_items`が生成する内容(全履歴)は変わらない。raw totalの削減自体はフェーズ2の対象であり、フェーズ1単体では測定可能な削減は無い想定(WSの接続再利用によるレイテンシ改善はありうるが、未測定)。

## エラー処理

- WS接続確立失敗: 上記の通りHTTP/SSEへフォールバック。
- 接続確立後の切断(タイムアウト、ネットワーク断): 次の`complete()`呼び出し時に再接続を試みる。再接続後はturn-stateが失われている可能性があるため、turn-stateを持たない状態からの初回リクエストとして送る(upstreamの挙動に倣う——具体の再接続プロトコルはワイヤ形式調査タスクの一部として確定)。
- `for_new_turn`が呼ばれた新規インスタンスの接続確立に失敗した場合も、同様にHTTP/SSEへフォールバックする(subagent側にも同じフォールバック挙動が及ぶ)。

## テスト方針

1. `for_new_turn`: `CodexProvider`は`Some`を返し、返されたインスタンスが独立した状態(接続・turn-stateを共有しない)を持つこと。デフォルト実装(`OpenAiProvider`・テスト用モック)は`None`を返すこと。
2. `spawn.rs`の`run_wave`: 各subagentタスクが`for_new_turn().unwrap_or_else(|| provider_pool.clone())`経由で得たプロバイダハンドルを使うこと(モックプロバイダで呼び出し回数・独立性を検証)。
3. WS接続確立・再利用・フォールバックのテストは、正確なワイヤ形式が判明してから設計する(実装計画側のタスクとして先送り)。ローカルWSテストサーバ(crateの選定含む)を使うか、upstream同様に接続層を抽象化してモック可能にするかは実装時に判断する。
4. 既存の`attempt`(HTTP/SSE)経路の全テストは無変更のまま通ること(フォールバック経路として存置されるため)。

## 受け入れ基準

1. `Provider`トレイトに、`None`を返すデフォルト実装付きの`for_new_turn`が追加され、`CodexProvider`がこれを`Some`を返す形で上書きしている。
2. `spawn::run_wave`が各subagentタスクへ`for_new_turn()`由来の独立したプロバイダハンドルを渡し、親の`run_loop`とWS接続・turn-stateを共有しない。
3. `CodexProvider`がWS接続を確立できる環境では、複数ターンにわたって同一接続を再利用する。
4. WS接続を確立できない環境では、現行のHTTP/SSE経路(`attempt`)へ自動的にフォールバックし、フェーズ1以前と同じ挙動になる。
5. フェーズ1完了時点で送信される`input`の内容(全履歴)はフェーズ1以前と変わらない。
6. `cargo test --workspace`・`cargo clippy --workspace --all-targets -- -D warnings`・`cargo fmt --all -- --check`が通る。

## 見送った代替案

- **WS接続を`CodexProvider`インスタンス単位ではなく`complete()`呼び出し単位(リクエストごとに新規接続)にする案**: トレイト変更を避けられるが、upstreamのturn-state sticky routingは「同じ接続を保ったまま複数リクエストを送ることで同じバックエンドレプリカへルーティングされ続ける」ことが前提と見られ、毎回新規接続ではこの前提が崩れ、フェーズ2の差分送信が機能しない可能性が高い。見送った。
- **`provider_pool`の共有をそのまま維持し、`CodexProvider`内部でミューテックスによりWS接続へのアクセスを直列化する案**: トレイト変更は避けられるが、`spawn_concurrency`の「本当に並行実行する」という既存の設計意図(専用テストで担保済み)を、codexプロバイダ利用時にだけ暗黙に破ることになる。意図的な設計判断をコードの見えない場所で覆すのは望ましくないため見送った。
- **WSの正確なワイヤ形式をこの設計書で確定させる案**: upstreamの該当コード(`ApiWebSocketConnection`・`stream_request`・関連の型定義)を読み解く作業自体が相応の調査量になり、ブレインストーミングの場では完了させられなかった。実装計画の最初のタスクとして持ち越す。
