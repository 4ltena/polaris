# File map

This is a generated file. Do not edit it by hand. `crates/polaris-core/tests/filemap.rs`
reads the actual state of the repository from the result of
`git ls-files --cached --others --exclude-standard`, rebuilds the body, and checks it
against `docs/filemap.md`. The test fails on any drift. To update it, run:

```
UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap
```

## `.`

- `CHANGELOG.md` — 変更履歴
- `Cargo.toml` — workspace definition and shared dependencies
- `README.en.md` — polaris
- `README.md` — polaris
- `rust-toolchain.toml` — pinned toolchain

## `agents/file-inspector`

- `SKILL.md` — SKILL.md

## `agents/files-md-writer`

- `SKILL.md` — files.md

## `apps/macos`

- `README.md` — macOSデスクトップ

## `crates/polaris-auth`

- `Cargo.toml` — manifest for the polaris-auth crate

## `crates/polaris-auth/src`

- `api_key.rs` — Storage for a plain OpenAI API key at `~/.polaris/api_key.json`.
- `lib.rs` — Lifecycle of ChatGPT subscription auth (OAuth) and storage of credentials.
- `login.rs` — Building the authorization URL, and receiving the callback exactly once.
- `pkce.rs` — PKCE (RFC 7636) verifier and challenge.
- `protection.rs` — Process-wide metadata-only protection for authentication stores.
- `store.rs` — Storage for credentials. `~/.polaris/auth.json`, 0600, atomic writes.
- `token.rs` — Exchange and refresh against `/oauth/token`.

## `crates/polaris-auth/tests`

- `trusted_home.rs` — Trusted home protection without environment variables, including credential rotation.

## `crates/polaris-cli`

- `Cargo.toml` — manifest for the polaris-cli crate

## `crates/polaris-cli/examples`

- `cache_quality.rs` — Offline-testable driver for the cache-affinity quality matrix.
- `preimplementation_eval.rs` — One approved two-turn pre-implementation evaluation trial.
- `sadalmelik_pilot.rs` — Two-send live canary using production Session, workflow and Codex transport.
- `sadalmelik_quality.rs` — Offline run-loop fixture driver.  Live Codex is deliberately blocked.

## `crates/polaris-cli/src`

- `main.rs` — Entry point for the `polaris` binary. Decides the endpoint from environment
- `memory.rs` — Local project memory commands; embedding requests require explicit configuration.

## `crates/polaris-cli/tests`

- `cli.rs` — Integration tests for the CLI binary. Launches the real process to verify behavior.
- `confined_helper.rs` — Integration test that launches the real `polaris` binary as a helper
- `memory.rs` — Exercises project memory through the real CLI without model calls.
- `subcommands.rs` — Pins down that subcommands are actually reachable.

## `crates/polaris-core`

- `Cargo.toml` — manifest for the polaris-core crate

## `crates/polaris-core/src`

- `agent.rs` — The agent loop. Returns the body text at the point tool calls stop.
- `approval.rs` — Approval boundary. `sandbox_mode` sets the technical boundary;
- `audit.rs` — Append-only audit log. Not signed: in-process, the entity signing and the
- `budget.rs` — Measurement of the always-on context. Numbers are backed by measurement,
- `compaction.rs` — Automatic history summarization. Fires when the conversation's measured
- `config.rs` — Loads config files. Not existing is normal; being malformed is not.
- `constitution.rs` — The part of the always-on context that the harness does not own. The
- `conversation_memory.rs` — Publication boundary between the raw session marker and conversation indexes.
- `conversation_state.rs` — Versioned session storage, separate from the legacy Message-only JSONL reader.
- `desktop_approval.rs` — Bounded approval mailbox. The controller owns the pending request and
- `desktop_events.rs` — Desktopキューだけを有界にする。provider/tool内のbufferは対象外。
- `desktop_execution.rs` — Desktopの待機窓口。worker・結果・回収handleの所有はserviceに置く。
- `desktop_memory.rs` — v3記憶処理のworker/owner境界。Writerはserviceの所有者だけが操作する。
- `desktop_response.rs` — providerの借用deltaをdesktopの有界queueへ渡す表示adapter。
- `desktop_run.rs` — Owned real-core worker for the trusted desktop engine (macOS only).
- `dir_watch.rs` — `bash`/`write`/`edit` 呼び出しの前後でファイルシステムを比較し、新規
- `events.rs` — ターン実行中にツール呼び出し・subagent活動をTUIへリアルタイム通知する
- `execution_owner.rs` — 共通のrun専用有界実行所有者。保存スレッドではcommandを実行しない。
- `files_md.rs` — `dir_watch` が検出した変更を、実際に `files.md` を書く subagent の
- `gitignore.rs` — `files.md`(ハーネスが自動生成する、コミット対象外のファイル)を
- `isolated_run.rs` — macOS prepare-only composition. Paths and executable identity are supplied by
- `isolated_workspace.rs` — Bounded, sanitized source snapshots. No apply-back or execution policy lives here.
- `lib.rs` — Entry point for polaris-core. Ties together the budget, constitution, and prompt modules.
- `local_execution.rs` — Fixed local selections with endpoint-wide physical inference admission.
- `project.rs` — Resolves the project root.
- `prompt.rs` — The single place that assembles the set of things loaded every turn.
- `session.rs` — Request history with optional durable v2 storage or a v3 desktop owner port.
- `session_store.rs` — Durable session attachment, generation checks, and tombstone-serialized writes.
- `spawn.rs` — The `spawn` tool's implementation. Type discovery reuses
- `stop.rs` — Stop conditions. No automatic recovery is attempted. Continuing to spin
- `strict_provider.rs` — Dedicated summary-provider adapter and installation of the fixed local helper.
- `tool_memory.rs` — Opt-in recoverable tool-result retention; original output lives outside history.
- `workflow.rs` — Explicit workflow focus, independent of tool permissions and completion evidence.
- `workspace_apply.rs` — macOS file-only apply-back with retained originals and a durable intent journal.

## `crates/polaris-core/src/constitution`

- `refresh.rs` — Request-boundary reads of two owner-selected files. No polling or model paths.

## `crates/polaris-core/src/constitution/refresh`

- `tests.rs` — Request-boundary instruction reloads and protected source refusal tests.

## `crates/polaris-core/src/conversation_memory`

- `strict_history.rs` — Shared strict ten-turn preparation with v2 publication and v3 owner integration.

## `crates/polaris-core/src/desktop_run`

- `tests.rs` — Owned desktop worker completion, cancellation, and trusted input boundary tests.

## `crates/polaris-core/src/desktop_store`

- `bootstrap.rs` — Bounded native-owner bootstrap reader. Validated metadata is not a runtime
- `children.rs` — Bounded child ownership ledger. Child completion never accepts a task.
- `disk.rs` — 公開prefixだけの検証と、両log同期後の単一marker置換。失敗注入は試験内に閉じる。
- `memory.rs` — strict10のv3公開境界。モデルを呼ばず、v2 writerを開かない。
- `mod.rs` — v3保存。単一writer、世代公開、要求台帳と復旧を提供する。
- `model.rs` — P1値を共用する保存marker、付随状態、要求結果と明示的な保存エラー。
- `roles.rs` — Durable role choices and past metadata observations, never runtime authorization.
- `source_apply.rs` — Controller-only evidence ledger. No filesystem authorization or dispatch.
- `tests.rs` — S01〜S08の公開境界、要求台帳、復旧と実OS lockの回帰試験。
- `workflow.rs` — Durable workflow evidence; never an execution grant or a disk skill loader.
- `writer.rs` — TempDirの所有、writerのOS lock、対象別CASと実行意図・終端の公開をまとめる。

## `crates/polaris-core/src/desktop_store/bootstrap`

- `tests.rs` — Owner bootstrap metadata, identity, configuration and provider validation tests.

## `crates/polaris-core/src/desktop_store/memory`

- `tests.rs` — v3 strict10 publication, scope, recovery and bounded original-source tests.

## `crates/polaris-core/src/desktop_store/roles`

- `tests.rs` — Durable role metadata validation without provider resolution.

## `crates/polaris-core/src/execution_owner`

- `prepared_tests.rs` — Prepared workspace policy and lifetime checks through deferred process cleanup.

## `crates/polaris-core/src/isolated_run`

- `tests.rs` — Prepared workspace, staged helper identity, and runtime allowlist tests.
- `toolchains.rs` — Explicit, bounded copies of already relocatable packages. No discovery,

## `crates/polaris-core/src/isolated_run/toolchains`

- `tests.rs` — Copied runtime isolation, limits, and fixed executable search paths.

## `crates/polaris-core/src/isolated_workspace`

- `tests.rs` — Sanitized snapshots, excluded secrets, source conflicts, and copy limits.

## `crates/polaris-core/src/local_execution`

- `tests.rs` — Local endpoint capacity, bounded waiters, cancellation, and quarantine tests.

## `crates/polaris-core/src/secret_screen`

- `mod.rs` — Remna's privacy filter. Pure logic that runs **before** a captured event

## `crates/polaris-core/src/workspace_apply`

- `tests.rs` — Conflict-safe apply-back, durable recovery artifacts, and failure injection tests.

## `crates/polaris-core/tests`

- `filemap.rs` — A snapshot test that confirms `docs/filemap.md` matches the actual state of the repository.
- `isolated_execution.rs` — Synthetic end-to-end snapshot, confined execution, and source conflict tests.

## `crates/polaris-desktop-protocol`

- `Cargo.toml` — manifest for the polaris-desktop-protocol crate

## `crates/polaris-desktop-protocol/src`

- `codec.rs` — 1MiB以下の長さ付きJSONを有限byte列だけで処理し、全階層の重複キーとnullを拒否する。
- `event.rs` — 配信位置付きeventと本文byte offset・durability、承認更新を型付きpayloadで保持する。
- `ids.rs` — 役割ごとに異なる不透明IDと、精度を失わない正規十進u64文字列を定義する。
- `lib.rs` — Desktop IPCの値、有限byte列のcodec、純粋な受付・run遷移を定義する。I/Oや認可は行わない。
- `local_models.rs` — Bounded inventory observations, never model availability or execution authority.
- `request.rs` — M1の要求を専用paramsで表し、methodとsession対象の組合せをwire境界で検査する。
- `response.rs` — 成功resultと9種のエラーを排他的に表し、要求IDとmethodへの対応を検査する。
- `role_bindings.rs` — Durable local role choices. Values are metadata, never endpoint authority.
- `run_state.rs` — 単一attemptの制御入力と観測終端を分離し、取消後の成功保持と再開禁止を純粋に判定する。
- `snapshot.rs` — 保存世代、下書き、設定、作業・親子run、回答可能な未解決承認のsnapshot値を保持する。
- `source_apply.rs` — Saved source-apply facts only. Neither a live stage nor an execution capability.
- `source_recovery.rs` — Bounded result-save recovery wire values. No I/O, authority or apply replay.
- `validate.rs` — hello前後の受付を純粋に判定する。構造の受理は認可・保存・実行開始を意味しない。
- `workspace_view.rs` — Bounded read-only workspace and explicit attachment projections.

## `crates/polaris-desktop-protocol/tests`

- `boundaries.rs` — ID・u64の値境界、有限frameの全分割とEOF、不正UTF-8・JSON・深さ上限を検査する。
- `run_transitions.rs` — 制御×状態の全表と取消・成功の両順序を検査し、同attemptの終端を保持する。
- `source_apply.rs` — Source application wire contracts, correlation and bounded pagination tests.
- `wire.rs` — 手書きfixtureと対比し、全method、厳格な構文、snapshotの復元情報と相関を検査する。

## `crates/polaris-desktop-service`

- `Cargo.toml` — manifest for the polaris-desktop-service crate

## `crates/polaris-desktop-service/src`

- `auth_tokens.rs` — Trusted desktop owner adapter. Paths never come from model requests.
- `configured_factory.rs` — Explicit configured runtime factory without discovery or credential reads.
- `core_integration_tests.rs` — Real core + CLI helper + durable approval, with a deterministic local provider.
- `engine.rs` — 保存・実行の単一所有者。実行入力は信頼済みownerから明示注入する。
- `execution.rs` — 共通core ownerへのservice内部入口。実行・回収registryはcoreの一箇所に保持する。
- `launch_arguments.rs` — Strict native launch metadata. Parsing performs no I/O or authorization.
- `lib.rs` — Durable storage IPC and explicitly configured native-owner execution.
- `memory_resources.rs` — Trusted-owner strict10 preflight. No project configuration or provider requests.
- `os_home.rs` — Trusted OS account home lookup. Never consults or changes the environment.
- `owner_launch.rs` — Trusted existing-store composition. No discovery, provisioning or replay.
- `package_manifest.rs` — Fixed packaged execution-helper metadata, read only by the trusted launcher.
- `production_source_factory.rs` — Trusted source recovery preparation; never source dispatch authority.
- `recovery_base.rs` — Trusted startup-only recovery placement; never a source-write grant.
- `recovery_socket.rs` — Fixed inherited endpoint setup, before runtime initialization or diagnostics.
- `recovery_transport.rs` — Bounded result-only framing. Closing this transport does not stop its owner.
- `source_apply.rs` — Trusted, single-operation source-apply owner. No RPC or model authorization.
- `source_apply_io.rs` — One owned source-I/O thread, without Writer access or dispatch authorization.
- `startup.rs` — Production composition, owned until both IPC channels and engine have drained.
- `tests.rs` — 実OS匿名pipeで制御・本文・停止を往復し、論理engine再起動を別途検証する。
- `transport.rs` — P1 codecの有界非同期I/O。単一writerのフレーム全体に期限を適用する。
- `workspace_view.rs` — Read-only workspace projections under the authentication protection lock.

## `crates/polaris-desktop-service/src/bin`

- `polaris-desktop-service.rs` — Native-owner launch. Isolate the inherited recovery socket before diagnostics.
- `polaris-fake-service.rs` — 試験用の匿名stdin/stdout入口。常に自身の新規TempDirを所有し、path引数を持たない。

## `crates/polaris-desktop-service/src/configured_factory`

- `recipe.rs` — Reusable preparation recipe. Runs only on the owned preparation thread.
- `tests.rs` — Configured factory resource, source grant and runtime binding tests.

## `crates/polaris-desktop-service/src/engine`

- `local_models.rs` — One owned metadata query. No Writer or provider generation crosses this boundary.
- `real.rs` — Explicit trusted production integration; no provider discovery or source apply.
- `role_bindings.rs` — Trusted next-run role metadata configuration. Runtime binding happens only
- `source_owner.rs` — Engine-owned source lifecycle. Only the I/O Job leaves the blocking owner;
- `source_recovery.rs` — Trusted result-only ingress. The reactor holds a sender, never the Writer.
- `source_view.rs` — Bounded saved source-apply pages; never a live job state or dispatch authority.

## `crates/polaris-desktop-service/src/engine/real`

- `tests.rs` — Offline real-engine ownership, durable transcript, and cancellation fixtures.

## `crates/polaris-desktop-service/src/launch_arguments`

- `tests.rs` — Strict native launch argument grammar and legacy compatibility tests.

## `crates/polaris-desktop-service/src/memory_resources`

- `tests.rs` — Trusted strict10 resource validation and offline preflight failure tests.

## `crates/polaris-desktop-service/src/owner_launch`

- `tests.rs` — Existing-store owner composition and authority mismatch tests.

## `crates/polaris-desktop-service/src/package_manifest`

- `tests.rs` — Packaged helper manifest identity, secret exclusion and race tests.

## `crates/polaris-desktop-service/src/production_source_factory`

- `tests.rs` — Private source application placement, policy and retained recovery evidence tests.

## `crates/polaris-desktop-service/src/recovery_base`

- `tests.rs` — Recovery directory placement, exclusive creation and pinned failure evidence tests.

## `crates/polaris-desktop-service/src/source_apply`

- `tests.rs` — Source-apply controller authorization, single dispatch, and recovery tests.

## `crates/polaris-desktop-service/src/startup`

- `tests.rs` — Production startup resources, package aliases and real capability composition tests.

## `crates/polaris-desktop-service/tests`

- `native_launch.rs` — Real binary boundary: inherited control fd must never receive diagnostics.
- `native_owner.rs` — Actual packaged OwnerV1 startup with synthetic local metadata and both IPC channels.
- `persistence.rs` — Production storage IPC across OS children; all roots/content are disposable dummy fixtures.
- `subprocess.rs` — helper自身の新規TempDirで実OS子プロセスのstdin/stdoutと終了を検証する。

## `crates/polaris-http`

- `Cargo.toml` — manifest for the polaris-http crate

## `crates/polaris-http/src`

- `lib.rs` — Shared HTTP client setup.

## `crates/polaris-http/src/fixtures`

- `README.md` — TLSテスト用の証明書と鍵

## `crates/polaris-memory`

- `Cargo.toml` — manifest for the polaris-memory crate

## `crates/polaris-memory/src`

- `conversation.rs` — Pending, scope-bound summaries for strict conversation history.
- `lib.rs` — Local, explicitly populated memory. Retrieved text is quoted historical evidence,

## `crates/polaris-memory/tests`

- `conversation.rs` — Conversation index scope, publication, provenance, and forgetting contracts.
- `memory.rs` — Persistence, isolation, retrieval budgets and explicit embedding behavior.

## `crates/polaris-provider`

- `Cargo.toml` — manifest for the polaris-provider crate

## `crates/polaris-provider/src`

- `attempts.rs` — Physical provider attempts, durable reservations and conservative settlement.
- `cache_pacing.rs` — Optional per-provider spacing of model request dispatches.
- `cache_prefix.rs` — Fixed, opt-in instructions for the cache-prefix cost experiment.
- `codex.rs` — A provider that speaks the Responses API using ChatGPT subscription
- `codex_metrics.rs` — Opt-in, content-free diagnostics for each actual Codex HTTP attempt.
- `lib.rs` — Provider abstraction. Transport-dependent parts live in each
- `local.rs` — Bounded, metadata-only discovery of explicitly configured local runtimes.
- `local_inference.rs` — Inference through an explicitly selected loopback runtime.
- `openai.rs` — OpenAI-compatible chat completions. Swap out `base_url` and you can hit
- `role.rs` — Immutable routing for catalog-validated child roles. No implicit fallback.
- `sse.rs` — Incrementally decodes SSE (text/event-stream). Pushing a byte chunk
- `turn_affinity.rs` — Opaque transport continuity scoped to one logical user turn.
- `web_search.rs` — Hosted web-search request policy and Responses-style output parsing.

## `crates/polaris-provider/src/local`

- `tests.rs` — Verify bounded local-runtime discovery and independent metadata observations.

## `crates/polaris-provider/src/local_inference`

- `tests.rs` — Wire-only inference contracts; no real runtime or credentials.

## `crates/polaris-provider/src/role`

- `tests.rs` — Immutable role routing, missing-binding refusal, and usage propagation.

## `crates/polaris-provider/tests`

- `web_search.rs` — Hosted Web request boundaries and anonymous response fixtures.

## `crates/polaris-sandbox`

- `Cargo.toml` — manifest for the polaris-sandbox crate

## `crates/polaris-sandbox/src`

- `broker.rs` — Client for the outer native-sandbox broker.
- `child_environment.rs` — Native child environment only; broker requests do not carry this contract.
- `child_fds.rs` — Native exec must not inherit the harness's nonstandard descriptors.
- `confine.rs` — Launching a process under confinement. Dispatches to a per-platform
- `controlled.rs` — Bounded native process ownership, not a secret-file or inherited-FD sandbox.
- `helper.rs` — The mutation operation executed inside the confined child.
- `lib.rs` — Sandbox policy definitions, and delegation to OS mechanisms.
- `linux.rs` — Linux enforcement. Applies a landlock ruleset to the process itself,
- `macos.rs` — macOS enforcement. Builds a Seatbelt profile at runtime and launches the
- `policy.rs` — Policy and writable roots. Roots are normalized at construction time.
- `stage.rs` — Stage the helper binary outside the writable roots.

## `crates/polaris-sandbox/tests`

- `isolated_macos.rs` — macOS isolated filesystem, environment, scratch, and ordinary tool compatibility tests.
- `isolated_parent_access.rs` — Explicit macOS runtime test. No arbitrary PID or real parent environment is

## `crates/polaris-skills`

- `Cargo.toml` — manifest for the polaris-skills crate

## `crates/polaris-skills/resources/workflow/brainstorm`

- `SKILL.md` — SKILL.md

## `crates/polaris-skills/resources/workflow/deliver`

- `SKILL.md` — SKILL.md

## `crates/polaris-skills/resources/workflow/implement`

- `SKILL.md` — SKILL.md

## `crates/polaris-skills/resources/workflow/review`

- `SKILL.md` — SKILL.md

## `crates/polaris-skills/resources/workflow/specify`

- `SKILL.md` — SKILL.md

## `crates/polaris-skills/resources/workflow/verify`

- `SKILL.md` — SKILL.md

## `crates/polaris-skills/resources/workflow/workflow-core`

- `SKILL.md` — SKILL.md

## `crates/polaris-skills/src`

- `agent_type.rs` — subagent 型の定義（`agents/<type>/SKILL.md`）の解析と discovery。
- `discovery.rs` — Skill discovery. A single corrupt skill must not take down the whole
- `frontmatter.rs` — SKILL.md frontmatter parsing. Validates only the constraints the specification lays down; adds no constraints of its own.
- `lib.rs` — Loading of skills that conform to the Agent Skills specification. Adds no frontmatter fields of its own.
- `workflow_profile.rs` — Deterministic resolution of the optional shipped workflow skill profile.

## `crates/polaris-skills/tests`

- `workflow_profile.rs` — Mandatory workflow skill resolution, source identity, and token budgets.

## `crates/polaris-tools`

- `Cargo.toml` — manifest for the polaris-tools crate

## `crates/polaris-tools/src`

- `bash.rs` — `bash` tool. Runs a command inside a confined `/bin/sh`.
- `edit.rs` — `edit` tool. Goes through the same confined path as `write`.
- `isolated_read.rs` — OS隔離されたhelper内でのみ本文を読む。親側の直読fallbackは用意しない。
- `lib.rs` — polaris's built-in tools. The always-on tool set never exceeds 6 tools.
- `path_policy.rs` — Decides which paths reading is denied on. Leans toward avoiding missed
- `predicate.rs` — Predicts ahead of time whether a write will be denied.
- `read.rs` — read tool. Line numbers are attached to the output so the model can
- `schema_validate.rs` — subagent の結果を、型が宣言した JSON Schema に照合するだけの薄い
- `skill.rs` — skill tool. Returns the body on an exact name match, otherwise returns a
- `write.rs` — `write` tool. The actual write happens inside a confined child process.

## `crates/polaris-tools/src/skill`

- `bm25.rs` — BM25 ランキング。トークナイズ・語幹化・同義語展開・スコアリングだけを
- `near_universal.rs` — 「ほぼ常に関連する」skill の選定。BM25 のスコアリングを一切知らず、

## `crates/polaris-tui`

- `Cargo.toml` — manifest for the polaris-tui crate

## `crates/polaris-tui/src`

- `approver.rs` — The `Approver` that runs inside the TUI: draws a modal over the current
- `clipboard.rs` — Copies text to the system clipboard via the OSC 52 terminal escape
- `input.rs` — Pure keystroke-to-action mapping for the input box. Kept separate from
- `lib.rs` — The polaris interactive TUI. Entered by `polaris-cli` when `--prompt`
- `memory.rs` — Explicitly enabled project memory and pre-compaction archival.
- `onboarding.rs` — The onboarding screen: shown by `polaris-cli` when the interactive TUI
- `persist.rs` — Session persistence: one JSON `Message` per line.
- `render.rs` — Pure rendering: turns a `Session` + input state into terminal cells.
- `selection.rs` — Mouse drag-to-select over the conversation history. Operates entirely
- `sessions.rs` — Enumerates saved conversations under `~/.polaris/sessions/` for the
- `slash.rs` — Slash commands: local, client-side commands recognized when the input
- `time.rs` — A tiny, dependency-free UTC timestamp formatter — just enough to
- `tool_memory.rs` — Immutable, scoped tool output storage behind the reserved memory read path.

## `docs`

- `codex-update-efficiency.md` — Codex更新と使用量削減策の適用
- `context-efficiency.md` — コンテキストの効率化とローカル記憶
- `filemap.md` — File map
- `gpt6-cache-affinity-results.md` — 本文量を維持したキャッシュ再利用と品質検証
- `gpt6-cache-cost-results.md` — GPT-6のキャッシュ率と費用の比較
- `gpt6-cache-pacing-results.md` — 要求間隔とキャッシュ再利用の検証
- `gpt6-efficiency-results.md` — GPT-6 medium 効率化の初回実測
- `preimplementation-evaluation.md` — 実装前段階の評価計画
- `sadalmelik-validation.md` — v0.11.0の検証状況
- `skill-scaling-benchmark.md` — skill/pluginを大量に含めた比較
- `subagents.md` — subagent
- `testing.md` — テスト
- `usage.md` — 利用ガイド

## `docs/superpowers`

- `CURRENT.md` — Polaris 現在の状態

## `docs/superpowers/plans`

- `2026-08-16-polaris-m1-headless-loop.md` — polaris M1 ヘッドレス最小ループ Implementation Plan
- `2026-08-17-polaris-m2-write-and-sandbox.md` — polaris M2 実装計画 — write / edit / bash とサンドボックス
- `2026-08-17-polaris-m3a-skills.md` — polaris M3a Skills ローダと skill ツール Implementation Plan
- `2026-08-18-polaris-m25-codex-provider.md` — polaris M2.5 Codex プロバイダ 実装計画
- `2026-08-20-polaris-m3b-bm25-skill-router.md` — polaris M3b BM25 skill ルータ 実装計画
- `2026-08-20-polaris-m4-core.md` — polaris M4 コア（`spawn` と単一波オーケストレーション）実装計画
- `2026-08-21-polaris-tui-onboarding.md` — polaris TUI Onboarding Screen Implementation Plan
- `2026-08-21-polaris-tui-v2.md` — polaris TUI v2 Implementation Plan
- `2026-08-21-polaris-tui.md` — polaris TUI Implementation Plan
- `2026-08-24-polaris-files-md-autogen.md` — polaris ディレクトリ別 files.md 自動生成 Implementation Plan
- `2026-08-24-polaris-tui-live-tool-progress.md` — polaris TUI ライブツール進捗・diff表示 Implementation Plan
- `2026-08-25-polaris-tui-fullscreen-scroll.md` — polaris TUI 自前スクロール管理・フッター固定化 Implementation Plan
- `2026-08-26-polaris-codex-reasoning-continuity.md` — polaris Codexプロバイダ reasoning item保持 Implementation Plan
- `2026-08-26-polaris-history-compaction.md` — polaris 会話履歴の自動圧縮(compaction) Implementation Plan
- `2026-08-26-polaris-tui-mouse-drag-selection.md` — polaris TUI マウスドラッグ選択・自前クリップボードコピー Implementation Plan
- `2026-08-27-polaris-codex-ws-transport.md` — polaris Codex WebSocket トランスポート(フェーズ1) Implementation Plan

## `docs/superpowers/specs`

- `2026-08-16-polaris-harness-design.md` — polaris 設計仕様
- `2026-08-18-polaris-codex-provider-design.md` — polaris Codex プロバイダ 設計
- `2026-08-20-polaris-skill-bm25-router-design.md` — polaris skill ルータ BM25 化 設計
- `2026-08-21-polaris-tui-design.md` — polaris TUI 設計
- `2026-08-21-polaris-tui-onboarding-design.md` — polaris TUI 初回起動オンボーディング画面 設計
- `2026-08-21-polaris-tui-v2-design.md` — polaris TUI v2 設計
- `2026-08-24-polaris-files-md-autogen-design.md` — polaris ディレクトリ別 `files.md` 自動生成 設計書
- `2026-08-24-polaris-tui-live-tool-progress-design.md` — polaris TUI ライブツール進捗・diff表示 設計書
- `2026-08-25-polaris-tui-fullscreen-scroll-design.md` — polaris TUI 自前スクロール管理・フッター固定化 設計書
- `2026-08-26-polaris-codex-reasoning-continuity-design.md` — polaris Codexプロバイダ reasoning item保持 設計書
- `2026-08-26-polaris-history-compaction-design.md` — polaris 会話履歴の自動圧縮(compaction) 設計書
- `2026-08-26-polaris-tui-mouse-drag-selection-design.md` — polaris TUI マウスドラッグ選択・自前クリップボードコピー 設計書
- `2026-08-27-polaris-codex-ws-transport-design.md` — polaris Codex WebSocket トランスポート(フェーズ1)設計書

## `skills/mutation-check`

- `SKILL.md` — 変異で確かめる

## `skills/verify-a-change`

- `SKILL.md` — 変更を検証する
