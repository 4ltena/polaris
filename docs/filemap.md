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

## `agents/doc-writer`

- `SKILL.md` — 文書を書く

## `agents/file-inspector`

- `SKILL.md` — SKILL.md

## `agents/file-reader`

- `SKILL.md` — SKILL.md

## `agents/files-md-writer`

- `SKILL.md` — files.md

## `agents/implementer`

- `SKILL.md` — 実装する

## `agents/reviewer`

- `SKILL.md` — レビューする

## `apps/macos`

- `README.md` — macOSデスクトップ

## `crates/polaris-auth`

- `Cargo.toml` — manifest for the polaris-auth crate

## `crates/polaris-auth/examples`

- `git-credential-owned-dp-probe.rs` — Owned macOS DP Keychain validation. Never queries existing user items.

## `crates/polaris-auth/src`

- `api_key.rs` — Storage for a plain OpenAI API key at `~/.polaris/api_key.json`.
- `git_credentials.rs` — C08/C10 shared, local Keychain credential binding.
- `lib.rs` — Lifecycle of ChatGPT subscription auth (OAuth) and storage of credentials.
- `login.rs` — Building the authorization URL, and receiving the callback exactly once.
- `pkce.rs` — PKCE (RFC 7636) verifier and challenge.
- `protection.rs` — Process-wide metadata-only protection for authentication stores.
- `quota.rs` — Pure quota validation and account selection.
- `store.rs` — Storage for credentials. `~/.polaris/auth.json`, 0600, atomic writes.
- `token.rs` — Exchange and refresh against `/oauth/token`.

## `crates/polaris-auth/src/accounts`

- `directory.rs` — Fixed account-directory ownership and its non-secret protection journal.
- `mod.rs` — Fixed, descriptor-relative infrastructure for the account registry.
- `observation.rs` — M5-3b: the usage fetch job, its memory-only cache and the selection input
- `owner.rs` — M5-2c: tokens bound to one registration, a single refresh owner per slot
- `store.rs` — Durable account registry: schema, deterministic transitions and exact

## `crates/polaris-auth/src/accounts/observation`

- `tests.rs` — A loopback issuer, a loopback usage host and temporary homes only.

## `crates/polaris-auth/src/accounts/owner`

- `tests.rs` — Synthetic tokens, a loopback issuer and temporary homes only.

## `crates/polaris-auth/src/accounts/store`

- `tests.rs` — Synthetic credentials and temporary homes only.

## `crates/polaris-auth/src/git_credentials`

- `runner_tests.rs` — Git credential runner authorization, execution and replay regression tests.

## `crates/polaris-auth/src/quota`

- `usage.rs` — M5-3a: the bound usage request and its normalization into M5-1 input.

## `crates/polaris-auth/src/quota/usage`

- `tests.rs` — Synthetic responses from a loopback server; no real account is contacted.

## `crates/polaris-auth/tests`

- `trusted_home.rs` — Trusted home protection without environment variables, including credential rotation.

## `crates/polaris-cli`

- `Cargo.toml` — manifest for the polaris-cli crate

## `crates/polaris-cli/examples`

- `cache_quality.rs` — Offline-testable driver for the cache-affinity quality matrix.
- `m12_input_eval.rs` — Offline, content-free measurements of one M12 input case and preparation mode per process.
- `m12_model_eval.rs` — Opt-in M12 input comparison through durable sessions and the production agent.
- `m23_read_eval.rs` — Opt-in M23 comparison through the production agent/read/spawn path.
- `preimplementation_eval.rs` — One approved two-turn pre-implementation evaluation trial.
- `preselection_compare.rs` — 段階1: the arms that can be measured without a new adapter.
- `preselection_cost.rs` — What each preselection route costs, and who pays it.
- `preselection_e5_large.rs` — Does a better candidate list make the light model choose better?
- `sadalmelik_pilot.rs` — Two-send live canary using production Session, workflow and Codex transport.
- `sadalmelik_quality.rs` — Offline run-loop fixture driver.  Live Codex is deliberately blocked.

## `crates/polaris-cli/examples/eval_support`

- `accounting.rs` — Reconcile physical evidence and price known consumption without double counting.
- `auth.rs` — Explicit, read-only, single-load credentials for opt-in evaluation executables.

## `crates/polaris-cli/examples/m12_model_eval`

- `guard.rs` — Admit one root request, verify its actual input, and reject every tool response.
- `input.rs` — Bounded M12 fixtures, value-free response instructions and deterministic scoring.
- `tests.rs` — Offline positive and failure controls at the actual agent/provider boundary.

## `crates/polaris-cli/examples/m23_read_eval`

- `accounting.rs` — M23 model/effort binding for the shared physical-accounting implementation.
- `guard.rs` — Shared send, fee and tool admission guards for the parent and its one child.
- `input.rs` — Freeze bounded fixtures, public prompts, pair order and deterministic scoring.
- `tests.rs` — Offline scripted regressions through the real agent, read and spawn paths.

## `crates/polaris-cli/src`

- `account.rs` — 複数アカウントの登録操作と、1つのrunを1つの登録へ固定する束縛。
- `admission.rs` — One-shot CLI admission owner. The manager commit precedes MCP prompt
- `benchmark_report.rs` — Opt-in, machine-readable observations for the offline comparison harness.
- `compare.rs` — フォルダ同士がどれだけ似ているかを示す。名前とサイズだけを読み、
- `decided.rs` — `polaris decided` — the ledger of questions this project has settled.
- `decision.rs` — Explicit local decision management, separate from agent/provider startup.
- `env.rs` — Direct environment setup: parsing and presentation only. No model is used.
- `handoff.rs` — 実装が終わったことを、実機で試せる人へ渡す文面にする。
- `main.rs` — Entry point for the `polaris` binary. Decides the endpoint from environment
- `mcp.rs` — MCP registrations, secrets, catalog checks and setup from the terminal.
- `memory.rs` — Local project memory commands; embedding requests require explicit configuration.
- `provider_startup.rs` — Provider construction for the native CLI run after continuation preflight.
- `setting.rs` — 同梱の定型コマンド。引数の受け取りと表示だけを行い、検出・計画・記録は
- `spare.rs` — 頼まれていない整理へ余剰枠を使ってよいかを判定し、その理由を示す。
- `strict_history_tests.rs` — Exercises the CLI history constructor through persisted retrieval and a counting judge.
- `sync.rs` — M7b-5: the `polaris sync` entry points. Every command works on this

## `crates/polaris-cli/src/bin`

- `polaris-git-askpass.rs` — The only credential handoff executable used by C08/C10 Git children.

## `crates/polaris-cli/tests`

- `c08.rs` — Actual CLI rejection boundaries with disposable HOME, data and project roots.
- `cli.rs` — Integration tests for the CLI binary. Launches the real process to verify behavior.
- `confined_helper.rs` — Integration test that launches the real `polaris` binary as a helper
- `entry_points.rs` — The three commands that had no caller until now: comparing folders,
- `env.rs` — Direct management stays independent of model credentials and preserves the
- `input_preparation.rs` — Real CLI input preparation with an isolated home and a synthetic loopback model.
- `linux_signal_cleanup.rs` — Real Linux PTY evidence for interactive shutdown. Every child is the
- `mcp.rs` — `polaris mcp` against a synthetic home; no server or model is contacted.
- `memory.rs` — Exercises project memory through the real CLI without model calls.
- `setting.rs` — The bundled setting commands from the terminal. Real directories and a
- `subcommands.rs` — Pins down that subcommands are actually reachable.
- `sync.rs` — Two local devices exchanging through a bare repository; no network.
- `windows_execution.rs` — Windows refusal is tested at the real product entry before input or state I/O.
- `windows_sync_apply.rs` — Real product entry; all input and state live under disposable fixtures.

## `crates/polaris-cli/tests/support`

- `product_binary.rs` — Holds a private executable package for real CLI integration-test children.
- `windows_fixture.rs` — Capture recursive fixture contents for Windows CLI preservation assertions.

## `crates/polaris-core`

- `Cargo.toml` — manifest for the polaris-core crate

## `crates/polaris-core/examples`

- `clef_probe.rs` — Offline integration smoke for an explicitly pinned Clef installation.

## `crates/polaris-core/src`

- `activity.rs` — M6-2: what the host knows about a run's liveness, decided from events and
- `agent.rs` — The agent loop. Returns the body text at the point tool calls stop.
- `apply_intent.rs` — Canonical evidence for an owned, recoverable workspace apply.
- `apply_owner.rs` — The C12/D27 owner retains the project lease across durable lifecycle steps.
- `approval.rs` — Approval boundary. `sandbox_mode` sets the technical boundary;
- `audit.rs` — Append-only audit log. Not signed: in-process, the entity signing and the
- `budget.rs` — Measurement of the always-on context. Numbers are backed by measurement,
- `build_cache.rs` — What a project's build output may grow to, and what goes when it does.
- `clef.rs` — A lazy, offline Clef process owner. Package activation supplies the pinned
- `common_tests.rs` — A small, bounded black-box test schema and its confined runner.
- `compaction.rs` — Automatic history summarization. Fires when the conversation's measured
- `computer.rs` — Driving an application by naming the goal, not the pixels.
- `config.rs` — Loads config files. Not existing is normal; being malformed is not.
- `constitution.rs` — The part of the always-on context that the harness does not own. The
- `conversation_memory.rs` — Publication boundary between the raw session marker and conversation indexes.
- `conversation_state.rs` — Versioned session storage, separate from the legacy Message-only JSONL reader.
- `decision_bands.rs` — The band rule, in one place.
- `decision_capacity.rs` — Memory recommendations and conservative admission for a trusted model profile.
- `decision_diagnostics.rs` — Read-only typed-stage diagnostics shared by CLI and TUI; no connection probes.
- `decision_policy.rs` — Local-only admission for judgments and auxiliary generative arbitration.
- `decisions.rs` — What this project has already settled, so it is not settled twice.
- `desktop_approval.rs` — Bounded approval mailbox. The controller owns the pending request and
- `desktop_events.rs` — Desktopキューだけを有界にする。provider/tool内のbufferは対象外。
- `desktop_execution.rs` — Desktopの待機窓口。worker・結果・回収handleの所有はserviceに置く。
- `desktop_memory.rs` — v3記憶処理のworker/owner境界。Writerはserviceの所有者だけが操作する。
- `desktop_response.rs` — providerの借用deltaをdesktopの有界queueへ渡す表示adapter。
- `desktop_run.rs` — Owned real-core worker for the trusted desktop engine (macOS only).
- `desktop_workflow.rs` — Durable workflow checkpoint port. The single desktop owner writes state;
- `dir_watch.rs` — `bash`/`write`/`edit` 呼び出しの前後でファイルシステムを比較し、新規
- `environment.rs` — Host environment setup. Source admission uses the same protected,
- `error_region.rs` — Which part of a long tool result is worth quoting back.
- `events.rs` — ターン実行中にツール呼び出し・subagent活動をTUIへリアルタイム通知する
- `execution_owner.rs` — 共通のrun専用有界実行所有者。保存スレッドではcommandを実行しない。
- `filemap.rs` — Routing a request to a handful of files, instead of sending the whole map.
- `files_md.rs` — `dir_watch` が検出した変更を、実際に `files.md` を書く subagent の
- `gitignore.rs` — `files.md`(ハーネスが自動生成する、コミット対象外のファイル)を
- `handoff.rs` — M7a-1: the prompt that hands a finished implementation to someone who can
- `input_preparation.rs` — Reversible, host-only projection of repeated paragraphs in a new root input.
- `isolated_run.rs` — Prepare-only composition on Linux and macOS. Paths and executable identity are supplied by
- `isolated_workspace.rs` — Bounded, sanitized source snapshots. No apply-back or execution policy lives here.
- `lib.rs` — Entry point for polaris-core. Ties together the budget, constitution, and prompt modules.
- `linux_l04_parent.rs` — Read-only validation of an empty disposable restart fixture parent.
- `llama_server.rs` — Supervising the bundled `llama-server`.
- `local_execution.rs` — Fixed local selections with endpoint-wide physical inference admission.
- `managed_decision.rs` — Package-approved local decision generations. Project configuration selects
- `managed_runtime.rs` — Package-trusted, read-only Python runtime image.
- `management_effect.rs` — Durable, typed lifecycle records for management side effects.
- `mcp.rs` — Run-owned MCP connections, request context and local result retention.
- `measurement.rs` — M8: what one comparison measurement leaves behind. Nothing here sends
- `os_home.rs` — Trusted OS account lookup shared with the MCP migration owner.
- `preselect.rs` — Ranking candidates by embedding similarity — M10's second route.
- `preselection_corpus.rs` — The fixed corpus every preselection comparison is measured against, and
- `project.rs` — Resolves the project root.
- `project_analysis.rs` — Minimal, state-driven bootstrap for a first-turn project analysis.
- `project_git.rs` — A read-only git call against the user's own repository.
- `project_mutation.rs` — One project-wide lock for turns, owned children and workspace mutations.
- `prompt.rs` — The single place that assembles the set of things loaded every turn.
- `recipe.rs` — M7c: the build recipes Polaris ships. A recipe is a fixed definition, not
- `request_input.rs` — Ephemeral raw-event identity and request slots for lossless input preparation.
- `resume.rs` — M6-2b: whether a finished run should be continued without being asked.
- `rewind_admission.rs` — Execution admission for saved conversations after a workspace rewind.
- `rewind_candidate_capacity.rs` — Fail-closed capacity accounting for durable rewind candidates.
- `rewind_workspace.rs` — Durable, bounded workspace checkpoints used by file-and-context rewind.
- `sandbox_command.rs` — One explicit command under a narrowed startup ceiling and project lease.
- `scratch_registry.rs` — M6-5: the private copies Polaris created, recorded so that a later start
- `session.rs` — Request history with optional durable v2 storage or a v3 desktop owner port.
- `session_manager.rs` — Durable, side-effect-free admission and multi-session scheduling state.
- `session_settings.rs` — Conversation-owned settings, independent draft and revision-bound goals.
- `session_store.rs` — Durable session attachment, generation checks, and tombstone-serialized writes.
- `session_switch.rs` — Project-owned local session switches. A started switch is never replayed
- `similarity.rs` — M7a-2: which registered project folders look like one another, and what
- `spare.rs` — M7a-3: whether spare quota may be spent on refactoring nobody asked for.
- `spawn.rs` — The `spawn` tool's implementation. Type discovery reuses
- `stop.rs` — Stop conditions. No automatic recovery is attempted. Continuing to spin
- `strict_provider.rs` — Dedicated summary-provider adapter and installation of the fixed local helper.
- `subagent_control.rs` — Durable, side-effect-free control state for delegated agents.
- `sync.rs` — M7b: sharing one project across devices. The project's own repository is
- `tier.rs` — Deciding how much a delegated task is worth spending on.
- `tool_memory.rs` — Opt-in recoverable tool-result retention; original output lives outside history.
- `translation.rs` — Offline-only adapter for the pinned CAT-Translate 3.3B helper.
- `turn_intent.rs` — What one user turn is, along several axes at once.
- `windows_fs.rs` — Windows filesystem primitives remain private until owner integration.
- `workflow.rs` — Explicit workflow focus, independent of tool permissions and completion evidence.
- `workflow_artifact_store.rs` — Private immutable bodies for the durable workflow artifact graph.
- `workspace_apply.rs` — Unix file-only apply-back with retained originals and a durable intent journal.

## `crates/polaris-core/src/activity`

- `tests.rs` — Fixed durations only; no clock is read.

## `crates/polaris-core/src/agent`

- `delegated_read.rs` — Shared execution boundary for one delegated file-reader task.
- `input_preparation_tests.rs` — Request-boundary regressions for source-bound, host-only input preparation.
- `mcp_dispatch.rs` — Borrow the run owner; model operations cannot construct settings or grants.
- `request_context.rs` — Shared assembly for the sections appended to a request system prompt.
- `skill_catalog.rs` — Request-local metadata-only optional skill catalog.
- `verification.rs` — Parser boundary for the root-only verification tool.
- `workflow_artifact.rs` — Parsing boundary for root workflow-artifact calls.
- `workflow_context.rs` — Request-only projection of exact active workflow artifacts.
- `workflow_control.rs` — Bounded model proposals; authority and publication stay with the host.

## `crates/polaris-core/src/answer_finalizer`

- `mod.rs` — Pure contracts for final-answer observation and, once separately activated,
- `protected.rs` — Deterministic protected-span extraction and restoration.
- `shadow.rs` — Host-owned shadow finalization journal and bounded worker.
- `tests.rs` — Regression tests for answer finalization identities, evidence, and delivery policy.

## `crates/polaris-core/src/audit`

- `arbitration.rs` — Metadata-only observation of existing generative selection calls.
- `decision.rs` — Metadata-only typed-stage audit. An unmatched start has unknown completion/usage.

## `crates/polaris-core/src/audit/arbitration`

- `tests.rs` — Generative receipts preserve calls, missing usage and unknown cancellation.

## `crates/polaris-core/src/audit/decision`

- `tests.rs` — Written start precedes dispatch; raw prompts never enter typed audit records.

## `crates/polaris-core/src/build_cache`

- `tests.rs` — Each test names one thing that must hold before this is allowed to delete

## `crates/polaris-core/src/clef`

- `tests.rs` — Protocol and lifecycle fixtures, with no real weights or remote calls.
- `wire.rs` — Strict, bounded native worker protocol. No upstream diagnostics escape it.

## `crates/polaris-core/src/computer`

- `macos.rs` — The accessibility driver for macOS, through System Events.

## `crates/polaris-core/src/config`

- `decision_fallback.rs` — User-owned typed-routing failover configuration; project files cannot add destinations.
- `decision_system_one.rs` — User-owned System One endpoint/model/environment reference; no secret value.

## `crates/polaris-core/src/config/decision_fallback`

- `tests.rs` — User-only routing policy, destination validation and secret-free preflight display.

## `crates/polaris-core/src/config/decision_system_one`

- `tests.rs` — Configuration provenance and strict compatible target boundaries.

## `crates/polaris-core/src/constitution`

- `refresh.rs` — Request-boundary reads of two owner-selected files. No polling or model paths.

## `crates/polaris-core/src/constitution/refresh`

- `tests.rs` — Request-boundary instruction reloads and protected source refusal tests.

## `crates/polaris-core/src/conversation_memory`

- `observation.rs` — One accepted history observation shared by desktop, terminal and audit output.
- `related.rs` — Explicit historical search. It never supplies a current-state decision.
- `strict_history.rs` — Shared strict ten-turn preparation with v2 publication and v3 owner integration.
- `structured.rs` — Host validation for the structured10 outer summary envelope.
- `structured_owner.rs` — Structured publication through the CLI/TUI's existing v2 raw owner.

## `crates/polaris-core/src/conversation_memory/strict_history`

- `related_history.rs` — Related evidence through the same frozen raw owner and receipt transaction.
- `structured_history.rs` — Structured extraction and recovery through the existing raw owner.
- `structured_tests.rs` — Synthetic provider calls through the real shared history and v2 owner.

## `crates/polaris-core/src/conversation_memory/strict_history/structured_tests`

- `related.rs` — Common saved-owner retrieval through synthetic backends and actual dispatch.

## `crates/polaris-core/src/conversation_memory/structured`

- `authority.rs` — Decision authority comes from verified raw user-input boundaries, not role alone.
- `authority_tests.rs` — Supplemental user-role content never supplies decision authority.
- `facts.rs` — Source-ordered fact transitions bound to published entity and fact identities.
- `identity.rs` — Deterministic name and explicit-relation disambiguation for structured memory.
- `packing.rs` — Complete, bounded evidence bundles for one frozen structured-memory request.
- `retrieval.rs` — Relevant state evidence, ordered before coverage/raw checks and context packing.
- `sources.rs` — Raw evidence resolution after exact structured-publication authorization.
- `transitions_tests.rs` — Regression checks for source authority, identity transitions, and bounded summary hints.

## `crates/polaris-core/src/conversation_memory/structured_owner`

- `tests.rs` — Real v2 raw/receipt/SQLite boundaries; no live provider or embedding service.

## `crates/polaris-core/src/conversation_state`

- `structured_attempt.rs` — Durable structured-summary receipts owned by the existing conversation lock.
- `windows_store_tests.rs` — Windows conversation storage, publication, locking and refusal regression tests.

## `crates/polaris-core/src/decision_bands`

- `fallback.rs` — Build only an explicitly configured routing chain; finalizers never use it.
- `tests.rs` — A local stub stands in for the service. No real endpoint is contacted.

## `crates/polaris-core/src/decision_bands/fallback`

- `tests.rs` — Factory failover uses synthetic owners and loopback HTTP, never real models or credentials.

## `crates/polaris-core/src/decision_capacity`

- `colocation.rs` — Host-owned provider plans captured per typed request, not a shared last-budget slot.
- `observation.rs` — Read-only host memory hints. Missing GPU/availability readings stay unknown.
- `tests.rs` — Synthetic capacity edges and domain separation; no model or GPU is started.

## `crates/polaris-core/src/decision_capacity/colocation`

- `tests.rs` — Host plans are request-local snapshots; unknown children and overflow never mean zero.

## `crates/polaris-core/src/decision_policy`

- `tests.rs` — Synthetic dispatch probes for opt-in local-only admission and wrappers.

## `crates/polaris-core/src/decisions`

- `tests.rs` — What the ledger must not lose, and what the route must not claim.

## `crates/polaris-core/src/desktop_run`

- `context.rs` — Trusted initial request inputs; no mutable MCP runtime is stored in Session.
- `tests.rs` — Owned desktop worker completion, cancellation, and trusted input boundary tests.

## `crates/polaris-core/src/desktop_run/tests`

- `history.rs` — Desktop inputs are created by a real writer, including retained tool turns.
- `input_preparation.rs` — Desktop request preparation preserves accepted raw input across worker and reopen boundaries.
- `mcp_entry.rs` — Real worker/owner boundary with synthetic providers and a loopback MCP server.

## `crates/polaris-core/src/desktop_run/tests/mcp_entry`

- `agent_dispatch.rs` — Scripted model responses exercise the real worker, approval owner and raw store.

## `crates/polaris-core/src/desktop_run/tests/mcp_entry/agent_dispatch`

- `outcomes.rs` — Failure outcomes must retain canonical evidence and control continuation.

## `crates/polaris-core/src/desktop_store`

- `account.rs` — The registration a run was bound to when it was accepted. Evidence only:
- `artifacts.rs` — Root-scoped immutable bodies for workflow artifacts.
- `bootstrap.rs` — Bounded native-owner bootstrap reader. Validated metadata is not a runtime
- `children.rs` — Bounded child ownership ledger. Child completion never accepts a task.
- `disk.rs` — 公開prefixだけの検証と、両log同期後の単一marker置換。失敗注入は試験内に閉じる。
- `goal.rs` — M6-3a: the goal a session is working toward, and the instructions that
- `hub.rs` — M6-1: the one state window an agent reads, and the difference since what
- `memory.rs` — strict10のv3公開境界。モデルを呼ばず、v2 writerを開かない。
- `migration.rs` — Simultaneously retained observations of the supplied transport roots.
- `mod.rs` — v3保存。単一writer、世代公開、要求台帳と復旧を提供する。
- `model.rs` — P1値を共用する保存marker、付随状態、要求結果と明示的な保存エラー。
- `roles.rs` — Durable role choices and past metadata observations, never runtime authorization.
- `session_manager.rs` — Root-owned durable state for the two-session admission manager.
- `session_manager_family.rs` — Bounded family index reads and durable session-family membership.
- `session_manager_family_migration.rs` — Read-only whole-family inventory; no index repair or manager recovery.
- `session_manager_generation.rs` — Session-manager generation publication, sealing and archive recovery.
- `session_manager_migration.rs` — Private no-create/no-repair observations for the MCP migration owner.
- `source_apply.rs` — Controller-only evidence ledger. No filesystem authorization or dispatch.
- `star_names.rs` — Stable display names for child runs.
- `subagent_control.rs` — Durable, private owner storage for the typed subagent control plane.
- `tests.rs` — S01〜S08の公開境界、要求台帳、復旧と実OS lockの回帰試験。
- `workflow.rs` — Durable workflow evidence; never an execution grant or a disk skill loader.
- `writer.rs` — TempDirの所有、writerのOS lock、対象別CASと実行意図・終端の公開をまとめる。

## `crates/polaris-core/src/desktop_store/bootstrap`

- `tests.rs` — Owner bootstrap metadata, identity, configuration and provider validation tests.

## `crates/polaris-core/src/desktop_store/disk`

- `windows_tests.rs` — Windows desktop store directory, locking and generation persistence tests.

## `crates/polaris-core/src/desktop_store/goal`

- `tests.rs` — Bounds and identity only; no store and no I/O.

## `crates/polaris-core/src/desktop_store/hub`

- `tests.rs` — Pure projection and difference; no store and no I/O.

## `crates/polaris-core/src/desktop_store/memory`

- `fork.rs` — Host-owned fork seed and immutable, exact ancestor publication grants.
- `frozen.rs` — Frozen memory evidence only; this never recovers or executes a parent ledger.
- `initialize.rs` — Owner-aware additive index initialization before resource binding.
- `related.rs` — 保存済み応答と原文へ照合した関連要約候補。投入・分類・認可は変更しない。
- `tests.rs` — v3 strict10 publication, scope, recovery and bounded original-source tests.

## `crates/polaris-core/src/desktop_store/memory/frozen`

- `bounded.rs` — Limits parser input before decoding, and encoded proof size before retention.
- `history.rs` — Prove inherited copies against fixed parent publications, without a database.
- `initialize.rs` — Keep validated frozen owners locked through the additive SQLite commit.
- `prompt.rs` — Validate host prompt provenance without recreating a parent execution owner.
- `publication.rs` — Validate frozen owners while publishing only the current session's marker.
- `raw.rs` — Canonical raw JSONL is framed by newline; retain only a requested event/turn.
- `sidecar.rs` — Typed streaming projection, with no dependence on JSON field order.

## `crates/polaris-core/src/desktop_store/memory/tests`

- `fork_tests.rs` — Fork publication, run ordinal ownership and bounded frozen grant regressions.
- `frozen_tests.rs` — Fixed ancestor reader: locks, receipts, raw bindings and read-only failures.
- `oracle_tests.rs` — Authored source annotations exercise durable memory; they do not measure model extraction.
- `publication_process_tests.rs` — A failed publication marker must recover across an actual process boundary.
- `wire_tests.rs` — Actual owner context and agent requests, serialized by both provider builders.

## `crates/polaris-core/src/desktop_store/memory/tests/frozen_tests`

- `initialization_tests.rs` — Initializer ownership, additive migration and unchanged failure state.
- `prompt_tests.rs` — Real writer publications, then independently re-hashed invalid frozen files.

## `crates/polaris-core/src/desktop_store/memory/tests/frozen_tests/prompt_tests`

- `history_tests.rs` — Inherited request provenance uses real forks and exact fixed raw copies.
- `ready_tests.rs` — Ready must return the writer's projection, never the worker snapshot's raw.

## `crates/polaris-core/src/desktop_store/memory/tests/oracle_tests`

- `action_tests.rs` — Authored action decisions exercise agent/tool wiring, not semantic classification.
- `authority_tests.rs` — Fixed annotations test host authority, not natural-language classification accuracy.
- `budget_tests.rs` — An authored long-plan variant tests the real preparation budget boundary.
- `failure_tests.rs` — Fixed-oracle absence, source integrity, and interrupted withdrawal publication.

## `crates/polaris-core/src/desktop_store/migration`

- `discovery.rs` — Existing-only transport topology in every supplied data/settings location.
- `legacy.rs` — Existing-only legacy JSONL and pre-compaction archive observation.
- `management.rs` — Retained project mutation exclusion and historical management observation.
- `tests.rs` — Simultaneous storage-root ownership and migration closure regression tests.

## `crates/polaris-core/src/desktop_store/migration/discovery`

- `tests.rs` — Transport storage-root discovery and retained-inventory regression tests.

## `crates/polaris-core/src/desktop_store/migration/legacy`

- `tests.rs` — Read-only legacy transcript and archive migration inspection tests.

## `crates/polaris-core/src/desktop_store/migration/management`

- `tests.rs` — Management-store migration ownership and terminal-state inspection tests.

## `crates/polaris-core/src/desktop_store/roles`

- `tests.rs` — Durable role metadata validation without provider resolution.

## `crates/polaris-core/src/desktop_store/tests`

- `admission_tests.rs` — Admission append is one durable, replay-safe publication and isolates a
- `details_tests.rs` — Run details show the saved turn of exactly the accepted run.
- `mcp_tests.rs` — Durable MCP evidence, raw-call binding and uncertain-ACK recovery.
- `prompt_tests.rs` — Host prompt acceptance, raw provenance and confirm-only recovery.

## `crates/polaris-core/src/desktop_store/tests/prompt_tests`

- `history_tests.rs` — Owner request projection never rewrites raw or substitutes the search query.

## `crates/polaris-core/src/desktop_store/writer`

- `details.rs` — Display projection of one saved run: its accepted input and raw turn. The
- `history.rs` — Owner-proven request projection; durable raw and actual user query stay separate.
- `mcp.rs` — Atomic MCP intent/raw publication using the existing desktop writer.
- `migration.rs` — Read-only classification of decoded history before MCP store migration.
- `migration_local.rs` — Read-only local conversation migration inspection through pinned directories.
- `migration_root.rs` — Complete existing desktop-root observation; no create, repair, or activation.
- `prompt.rs` — One host-selected prompt per accepted input; saved proof never grants a retry.
- `recipe.rs` — M7c-3a: what a bundled setting command has already done. One saved step

## `crates/polaris-core/src/desktop_store/writer/recipe`

- `tests.rs` — Records only; nothing is executed and no file is read.

## `crates/polaris-core/src/environment`

- `capture.rs` — Admit project sources and dependency inventories under protected-path checks.
- `native.rs` — Read-only discovery of the existing Apple toolchain. No SDK or compiler is
- `owner.rs` — Shared project-bound host owner for CLI, TUI and the typed environment tool.
- `recovery.rs` — Reconcile retained evidence without transferring, spawning or publishing.
- `tool.rs` — Root-only typed port. The live turn host lends its existing activity lease;

## `crates/polaris-core/src/error_region`

- `tests.rs` — Each test names one property of the selection and fails if that property

## `crates/polaris-core/src/execution_owner`

- `mcp.rs` — The desktop execution owner's bounded MCP approval and publication relay.
- `prepared_tests.rs` — Prepared workspace policy and lifetime checks through deferred process cleanup.
- `registration.rs` — Host-local MCP registration: one durable approval and intent per proposal.
- `step.rs` — Ordered polling phases for the single `RunExecution` owner.

## `crates/polaris-core/src/execution_owner/mcp`

- `tests.rs` — Real owner/Writer and loopback transport; no model or production server.

## `crates/polaris-core/src/execution_owner/mcp/tests`

- `fixture.rs` — Loopback-only, scripted MCP server. No credentials or production endpoint.
- `history.rs` — History uses the existing lane without an operation, grant or publication.
- `prompt.rs` — Explicit prompt selection uses the same owner without model-call authority.

## `crates/polaris-core/src/execution_owner/registration`

- `tests.rs` — Real owner/Writer; the grant is the only authority to save.

## `crates/polaris-core/src/filemap`

- `tests.rs` — The rules that decide which files the model is pointed at, and what the

## `crates/polaris-core/src/handoff`

- `tests.rs` — Fixed inputs only; nothing is sent anywhere.

## `crates/polaris-core/src/input_preparation`

- `tests.rs` — Lossless encoding, bounded decoding and adoption-threshold regressions.

## `crates/polaris-core/src/isolated_run`

- `helper_staging.rs` — FD-fixed copy kernel with separate ephemeral and persistent lifetimes.
- `helper_staging_tests.rs` — Persistent helper staging, publication faults and retained-runtime regression tests.
- `tests.rs` — Prepared workspace, staged helper identity, and runtime allowlist tests.
- `toolchains.rs` — Explicit, bounded copies of already relocatable packages. No discovery,

## `crates/polaris-core/src/isolated_run/toolchains`

- `tests.rs` — Copied runtime isolation, limits, and fixed executable search paths.

## `crates/polaris-core/src/isolated_workspace`

- `tests.rs` — Sanitized snapshots, excluded secrets, source conflicts, and copy limits.

## `crates/polaris-core/src/llama_server`

- `tests.rs` — The supervisor, against a stand-in server.

## `crates/polaris-core/src/local_execution`

- `tests.rs` — Local endpoint capacity, bounded waiters, cancellation, and quarantine tests.

## `crates/polaris-core/src/managed_decision`

- `native.rs` — Bind a package-approved subject to fixed private generation paths, lazily.

## `crates/polaris-core/src/managed_decision/native`

- `download.rs` — Confirmed package downloads feed verified private files into atomic import.
- `install.rs` — Explicit offline import and lossless repair of fixed decision generations.
- `plan.rs` — Fresh consent plans bind the package, sources, destination and inventory.
- `tests.rs` — Package-lock and private-generation fixtures; no real model is executed.
- `verification.rs` — Explicit full-digest check of an existing pinned generation; no mutation or launch.

## `crates/polaris-core/src/managed_decision/native/download`

- `staging.rs` — Descriptor-pinned download staging; cleanup touches only this transaction.
- `tests.rs` — Dummy transfer and private filesystem fixtures; no external model downloads.

## `crates/polaris-core/src/managed_decision/native/install`

- `tests.rs` — Offline import transactions use synthetic bytes and never execute a worker.

## `crates/polaris-core/src/managed_decision/native/plan`

- `tests.rs` — Consent and inventory fixtures; no downloads, credentials or real weights.

## `crates/polaris-core/src/managed_decision/native/verification`

- `tests.rs` — Read-only reuse verification with tiny synthetic files; no process or weights.

## `crates/polaris-core/src/management_effect`

- `account_change.rs` — Account management uses the existing auth owner and project effect journal.
- `generation.rs` — Durable management-effect generations, checkpoints and sealed archives.
- `mcp_change.rs` — MCP project admission changes retain the registry CAS owner and effect journal.
- `memory_change.rs` — One project owner for explicit import, embedding, indexing and session forgetting.
- `store.rs` — Lease-bound management-effect journal storage and validation.
- `tests.rs` — Management-effect ownership, journal and recovery regression tests.
- `v2.rs` — Version-two management contracts. Durable publication is owned by the
- `workflow_change.rs` — Durable local-session workflow changes.  A started change is reconciled
- `workspace_change.rs` — Confirmed Git recipe changes share the existing recipe ledger and effect journal.

## `crates/polaris-core/src/management_effect/generation`

- `migration_v2.rs` — Immutable, whole-generation v1 -> v2 transformation.

## `crates/polaris-core/src/management_effect/generation/migration_v2`

- `tests.rs` — Management generation staging, payload validation and migration fault tests.

## `crates/polaris-core/src/management_effect/mcp_change`

- `local_source.rs` — C28 local-source publication followed by C27 registry replacement.

## `crates/polaris-core/src/management_effect/memory_change`

- `tests.rs` — Memory-change effect tests for durable mutation and recovery.

## `crates/polaris-core/src/management_effect/store`

- `replay.rs` — Pure V1 journal envelopes and replay reducer.
- `windows_backend.rs` — Qualified Windows I/O for the existing management journal algorithms.
- `windows_tests.rs` — Windows management-effect journal lifecycle, replay and storage refusal tests.

## `crates/polaris-core/src/management_effect/v2`

- `reducer.rs` — V2 lifecycle replay. The store supplies durable ledger proofs; this module
- `tests.rs` — Management effect v2 request, snapshot and successor validation tests.

## `crates/polaris-core/src/mcp`

- `apps.rs` — Built-in creative-app definitions (M2b). Every entry is unverified until a
- `configure.rs` — Host-owned preparation and application of the one supported local Unity MCP runtime.
- `credentials.rs` — Typed ownership of MCP credential mutations.
- `lease.rs` — Fail-closed leases for parent-captured MCP read evidence.
- `local.rs` — CLI and TUI hosts: the same store, startup admission and setup tool as the
- `migration.rs` — Core-private MCP migration authority. Read-only MCP observations never grant
- `operation.rs` — Host-bound MCP operation evidence.
- `preselection.rs` — Host-owned tool preselection for one root run. BM25 narrows indexed
- `results.rs` — Bounded, nonpersistent result bytes. Handles are host-generated and usable
- `runtime.rs` — Retained run ownership for lazy MCP connections, tools and resource reads.
- `selection.rs` — Frozen MCP candidate ranking and bounded tool selection for one request.
- `setup.rs` — Host-only detection of MCP servers already configured for Codex and Claude.

## `crates/polaris-core/src/mcp/configure`

- `install.rs` — Descriptor-relative independent Unity runtime copies and verified manifest publication.
- `source.rs` — Descriptor-bound Unity source manifests, executable pins and the exact esbuild pair exception.
- `tests.rs` — Isolated fixture owners and filesystem evidence for Unity configuration tests.

## `crates/polaris-core/src/mcp/configure/tests`

- `apply.rs` — Approved runtime publication, registry CAS, and management-effect fault checks.
- `binding.rs` — Read-only preview and descriptor-bound source/registration consistency checks.
- `scanner.rs` — Unity-only hardlink exception and ordinary tree-policy rejection controls.

## `crates/polaris-core/src/mcp/local`

- `tests.rs` — Shared CLI/TUI preparation with synthetic stores and providers, no network.

## `crates/polaris-core/src/mcp/migration`

- `account_home.rs` — Retained OS account-home identity and descriptor binding for migration.
- `coordinator.rs` — Private retained admission for physical MCP migration.
- `entry_gate.rs` — Exclusive migration entry-gate ownership and retained home binding.
- `intent_file.rs` — Descriptor-owned external migration intent, outside the activation payload.
- `preflight.rs` — Side-effect-free source inspection before acquiring the migrating entry gate.
- `read_admission.rs` — Live private guard binding around MCP's non-authoritative read observations.
- `test_fixture.rs` — Shared private migration guard fixtures and canonical intent records.

## `crates/polaris-core/src/mcp/migration/account_home`

- `tests.rs` — Account-home identity, alias and namespace-drift migration tests.

## `crates/polaris-core/src/mcp/migration/coordinator`

- `retirement.rs` — Physical source retirement. Failure preserves all on-disk evidence and the
- `tests.rs` — Migration coordinator storage ownership and fresh-boundary regression tests.

## `crates/polaris-core/src/mcp/migration/coordinator/retirement`

- `source_payload.rs` — Read-only source preparation borrowed from the retained retirement owner.
- `staging.rs` — Global pre-epoch staging under the retained retirement owner. Failed writes
- `tests.rs` — Management retirement, global staging and partial-evidence recovery tests.

## `crates/polaris-core/src/mcp/migration/coordinator/retirement/source_payload`

- `tests.rs` — Retired source payload, registry and package-pin inspection tests.

## `crates/polaris-core/src/mcp/migration/coordinator/retirement/staging`

- `tests.rs` — Physical global staging and completed-project payload binding tests.

## `crates/polaris-core/src/mcp/migration/entry_gate`

- `process_inventory.rs` — Native inventory only. Unknown live identities are blockers, never zero.
- `tests.rs` — Migration entry-gate identity, binding and fail-closed regression tests.

## `crates/polaris-core/src/mcp/migration/intent_file`

- `tests.rs` — Durable external migration intent history, binding and replacement tests.

## `crates/polaris-core/src/mcp/migration/preflight`

- `tests.rs` — Migration source preflight inventory and production manifest validation tests.

## `crates/polaris-core/src/mcp/migration/read_admission`

- `tests.rs` — Historical migration read admission and live entry-authority closure tests.

## `crates/polaris-core/src/mcp/operation`

- `read.rs` — Exact resource meta-call binding; it cannot select prompts or grant tools.
- `saved.rs` — Immutable evidence for a saved MCP operation, never a reconstructed grant.

## `crates/polaris-core/src/mcp/preselection`

- `decided.rs` — J-3: the same preselection, decided by typed questions instead of by
- `tests.rs` — Lexical narrowing, bounded light-model output and closed failure reasons.

## `crates/polaris-core/src/mcp/preselection/decided`

- `tests.rs` — A fake decider stands in for the real one; nothing is sent.

## `crates/polaris-core/src/mcp/results`

- `tests.rs` — Regression checks for bounded MCP result projection, handles, and expiry.

## `crates/polaris-core/src/mcp/runtime`

- `local.rs` — Local hosts: the Gate approves the exact prepared operation, then one send.
- `metadata.rs` — Serialized metadata acquisition with bounded retries and retained cleanup.
- `resources.rs` — Trusted startup resources, shared with the factory until native cleanup ends.
- `tests.rs` — Loopback runtime ownership, bounded catalog selection and durable dispatch.

## `crates/polaris-core/src/mcp/runtime/resources`

- `tests.rs` — Registered cwd admission, absent secrets, native confinement and scratch lifetime.
- `windows_tests.rs` — Windows MCP resource and metadata refusal tests that preserve registered state.

## `crates/polaris-core/src/mcp/runtime/tests`

- `budget.rs` — Catalog acquisition limits survive eviction, refresh, failure and cancellation.
- `prompt.rs` — Host prompt preparation blocks request snapshots until durable confirmation.
- `retry.rs` — Retry only pre-operation metadata failures, preserving cancellation and bounds.

## `crates/polaris-core/src/mcp/selection`

- `tests.rs` — Regression checks for MCP selection budgets, schemas, and stale catalogs.

## `crates/polaris-core/src/mcp/setup`

- `data_tests.rs` — MCP setup proposal hashing, authentication and credential data validation tests.
- `launcher.rs` — What a launcher command is actually asking for.
- `materialise_tests.rs` — Prepared package downloads bind the approval card before any fetch begins.
- `register.rs` — Approved proposals: the approval text, its payload binding and the save order.
- `tests.rs` — Host detection from Codex and Claude configs, with secrets bound by HMAC.
- `tool.rs` — Root-run `mcp_setup`. Arguments carry an operation and a candidate ID only;
- `windows_tests.rs` — Windows MCP setup refusal tests that preserve storage and avoid approval requests.

## `crates/polaris-core/src/mcp/setup/launcher`

- `tests.rs` — Each test names one thing the parse must get right for a package to be

## `crates/polaris-core/src/measurement`

- `output.rs` — M8b-3: what a turn's output was made of. The split is mechanical — the
- `price.rs` — M8b-1/2: the one price list and the cost it gives. The table is a single
- `tests.rs` — Pure records; nothing is sent and no file is read.

## `crates/polaris-core/src/measurement/output`

- `tests.rs` — Counting only; nothing is sent and no transcript is read from disk.

## `crates/polaris-core/src/measurement/price`

- `tests.rs` — The embedded table and exact money arithmetic; nothing is sent.

## `crates/polaris-core/src/preselect`

- `tests.rs` — The ranking stage, against a deterministic embedder.

## `crates/polaris-core/src/preselection_corpus`

- `tests.rs` — The corpus is data the comparison depends on, so its shape is checked

## `crates/polaris-core/src/project_mutation`

- `windows_tests.rs` — Windows project mutation lease identity, exclusion and storage refusal tests.

## `crates/polaris-core/src/recipe`

- `confirm.rs` — M7c-3b-2: whether a step actually did what it was asked to do.
- `git.rs` — M7c-3c: what git looks like at a project, and what `git_setting` may do
- `ignore_rules.rs` — M7c-3c: which `.gitignore` rules a setting command may add.
- `session.rs` — M7c-3b-1: the durable session a bundled setting command writes into.
- `template.rs` — M7c-3e: the files the configure step writes.
- `tests.rs` — Fixed observations only; nothing is installed.
- `tool.rs` — M7c-3b-3: the bundled setting commands as an internal tool.

## `crates/polaris-core/src/recipe/confirm`

- `tests.rs` — Reads a fixture project. No command is run and nothing is installed.

## `crates/polaris-core/src/recipe/git`

- `tests.rs` — Real repositories in a temporary directory; nothing is initialized in a

## `crates/polaris-core/src/recipe/ignore_rules`

- `tests.rs` — Decisions only; no file is read or written.

## `crates/polaris-core/src/recipe/session`

- `tests.rs` — Opens a real store under a temporary `POLARIS_DATA_DIR`. Nothing is

## `crates/polaris-core/src/recipe/template`

- `tests.rs` — Writes into a temporary project. Nothing is installed and no command is

## `crates/polaris-core/src/recipe/tool`

- `tests.rs` — Real directories and a real ledger. Nothing is installed and no command
- `windows_tests.rs` — Windows recipe setting refusal tests that preserve project state and avoid approval requests.

## `crates/polaris-core/src/request_input`

- `tests.rs` — Raw identity, repeated text, request rebuilds and byte-range regression tests.

## `crates/polaris-core/src/resume`

- `tests.rs` — Fixed observations only.

## `crates/polaris-core/src/rewind_workspace`

- `manifest_compare.rs` — Pure manifest value, observation and change comparisons.

## `crates/polaris-core/src/scratch_registry`

- `tests.rs` — Temporary directories only; the registry path is process-wide, so these
- `windows_tests.rs` — Windows scratch registry refusal tests that preserve installed records and copies.

## `crates/polaris-core/src/secret_screen`

- `mod.rs` — Remna's privacy filter. Pure logic that runs **before** a captured event

## `crates/polaris-core/src/session`

- `delegation.rs` — Root delegation control, durable publication, runtime aborts and safe projections.

## `crates/polaris-core/src/session_settings`

- `owner.rs` — Preview and commit session settings through the management-effect owner.
- `tests.rs` — Session-settings persistence, denial and recovery regression tests.

## `crates/polaris-core/src/session_store`

- `windows_tests.rs` — Windows session persistence, lifecycle and unsupported storage refusal tests.

## `crates/polaris-core/src/similarity`

- `sample.rs` — Reading folders into the shape the comparison takes.
- `tests.rs` — Names and sizes only; no folder is read here.

## `crates/polaris-core/src/similarity/sample`

- `tests.rs` — Samples real temporary directories. No file is opened.

## `crates/polaris-core/src/spare`

- `from_quota.rs` — Turning real quota observations into what the decision needs.
- `tests.rs` — Fixed observations only.

## `crates/polaris-core/src/spare/from_quota`

- `tests.rs` — Readings only; no account is contacted and nothing is spent.

## `crates/polaris-core/src/sync`

- `apply.rs` — M7b-4: putting an agreed shared tree into the project. Nothing here writes
- `composite.rs` — Read-only project admission across retained C08/C10 and management evidence.
- `credential_rebind.rs` — C08 owns credential-binding replacement under one project EX lease.
- `exchange.rs` — M7b-3: exchanging with the other devices and deciding what each change
- `owner.rs` — Durable owner for sync operations that may mutate a remote ref.
- `participation.rs` — M7b-5: what this device remembers about a shared project. One small file
- `push.rs` — C10's shared push owner. Frontends consume its prepared token, never Git.
- `remote_intent.rs` — Durable request and receipt types for one state-changing sync remote call.
- `repository.rs` — M7b-2: the synchronization repository. Git only ever sees the staging
- `share.rs` — M7b-1: who takes part in a shared project, what is shared, and where the

## `crates/polaris-core/src/sync/apply`

- `tests.rs` — Temporary projects only; the apply owner is exercised through the copy.

## `crates/polaris-core/src/sync/composite`

- `tests.rs` — Composite sync read ownership, intent history and recovery regression tests.
- `windows_tests.rs` — Windows composite sync admission and namespace refusal tests that preserve state.

## `crates/polaris-core/src/sync/credential_rebind`

- `data_tests.rs` — Pure C08 DTO fixtures: no credential owner, live executable or project lease.
- `store_v1.rs` — Strict descriptor-relative storage for C08 history and no-intent evidence.
- `tests.rs` — Disposable C08 owner tests. No HOME override, Keychain query or Git process.
- `windows_tests.rs` — Windows credential rebind owner acquisition refusal and project preservation tests.

## `crates/polaris-core/src/sync/credential_rebind/store_v1`

- `data_tests.rs` — Pure filename, capacity and inventory data checks; no store or lease is opened.
- `tests.rs` — Credential rebind store ownership, archive and persistence regression tests.
- `windows_tests.rs` — Windows credential rebind intent inspection refusal and storage preservation tests.

## `crates/polaris-core/src/sync/exchange`

- `tests.rs` — Two local devices and a bare repository standing in for the remote.

## `crates/polaris-core/src/sync/owner`

- `store_v2.rs` — C10 bounded descriptor-relative store. Inspection and reservation write nothing.
- `windows_tests.rs` — Windows sync remote intent owner refusal and storage preservation tests.

## `crates/polaris-core/src/sync/owner/store_v2`

- `data_tests.rs` — Pure C10 store data checks; no owner, reservation or project lease is acquired.
- `tests.rs` — C10 owner store v2 identity, persistence and recovery regression tests.

## `crates/polaris-core/src/sync/participation`

- `tests.rs` — Temporary state directories only.

## `crates/polaris-core/src/sync/push`

- `owner_v1.rs` — Production C10 orchestration. Persisted DTOs never grant execution.
- `tests.rs` — Push payload decisions, exact confirmation and permission regression tests.
- `transport.rs` — One explicitly bound C10 network attempt. The owner supplies admission,

## `crates/polaris-core/src/sync/push/owner_v1`

- `data_tests.rs` — Sync push request, history, identity binding and spawn error data tests.
- `recovery.rs` — Explicit recovery never resumes a send or reconstructs a confirm token.
- `tests.rs` — Owner-only regression fixtures.  Every subprocess is an owned wrapper or
- `windows_tests.rs` — Windows sync push factory refusal tests that preserve state and avoid host callbacks.

## `crates/polaris-core/src/sync/push/transport`

- `tests.rs` — Controlled Git transport, ancestry and remote-change regression tests.

## `crates/polaris-core/src/sync/remote_intent`

- `v2.rs` — C10's closed, immutable identity and complete durable history. No transport.

## `crates/polaris-core/src/sync/remote_intent/v2`

- `tests.rs` — Remote intent v2 records, completed receipts and history validation tests.

## `crates/polaris-core/src/sync/repository`

- `c10.rs` — C10's private, descriptor-relative local Git repository owner.
- `tests.rs` — Temporary projects only; the synchronization repository is local.

## `crates/polaris-core/src/sync/repository/c10`

- `tests.rs` — Owned filesystem/Git fixtures only. Never contact Keychain or a remote.
- `windows_tests.rs` — Windows C10 repository refusal tests that preserve project and state contents.

## `crates/polaris-core/src/sync/share`

- `tests.rs` — Paths and rules only; no Git and no network.

## `crates/polaris-core/src/tier`

- `chat.rs` — Pricing a wave with an ordinary model, for runs with no typed decider.
- `tests.rs` — The ceiling, and what happens when a decider misbehaves.
- `typed.rs` — Pricing a wave of delegations with one typed decision.

## `crates/polaris-core/src/tier/chat`

- `tests.rs` — A scripted provider stands in for the model. Nothing is sent.

## `crates/polaris-core/src/tier/typed`

- `tests.rs` — A local stub stands in for the service. No real endpoint is contacted.

## `crates/polaris-core/src/verification`

- `adapters.rs` — Fixed-shape adapters for registered static checks and Rust test listing.
- `checkpoint.rs` — Durable, host-owned verification checkpoint metadata.
- `mod.rs` — Pure, durable verification inputs and decisions.
- `owner.rs` — Host-owned production port for the verification pipeline.
- `pipeline.rs` — Bounded production bridge from immutable workflow artifacts to verification
- `store.rs` — Private immutable verification records.
- `tests.rs` — Regression tests for verification selection, execution, evidence, and convergence.

## `crates/polaris-core/src/verification/pipeline/tests`

- `provenance.rs` — Persist the effective typed source after failover, not the configured primary.

## `crates/polaris-core/src/verification/store`

- `windows_tests.rs` — Windows verification store refusal tests that preserve roots and existing records.

## `crates/polaris-core/src/workflow`

- `artifacts.rs` — Versioned workflow artifacts. Bodies are immutable; stores publish only
- `context.rs` — Pure, bounded projection of the currently active workflow task.
- `evidence.rs` — Deterministic evidence and convergence records for durable workflow gates.

## `crates/polaris-core/src/workflow_artifact_store`

- `windows_tests.rs` — Windows workflow artifact store refusal tests that preserve roots and stored bodies.

## `crates/polaris-core/src/workspace_apply`

- `report.rs` — Shared apply-report format; filesystem ownership stays in the native owner.
- `tests.rs` — Conflict-safe apply-back, durable recovery artifacts, and failure injection tests.

## `crates/polaris-core/tests`

- `architecture.rs` — A snapshot test that confirms `docs/architecture.md` matches the
- `filemap.rs` — A snapshot test that confirms `docs/filemap.md` matches the actual state of the repository.
- `isolated_execution.rs` — Synthetic end-to-end snapshot, confined execution, and source conflict tests.
- `linux_apply_intent_restart.rs` — Local XFS restart acceptance. Phase fixtures use the public canonical wire
- `linux_filesystem_acceptance.rs` — Explicit, disposable-root acceptance probe for Linux filesystem primitives.

## `crates/polaris-core/tests/support`

- `repository_files.rs` — Reads the repository file ledger from Git or an explicitly prefetched listing.

## `crates/polaris-desktop-protocol`

- `Cargo.toml` — manifest for the polaris-desktop-protocol crate

## `crates/polaris-desktop-protocol/src`

- `browse.rs` — Home browsing for the read-only helper. Paths are home-relative; the root
- `codec.rs` — 1MiB以下の長さ付きJSONを有限byte列だけで処理し、全階層の重複キーとnullを拒否する。
- `event.rs` — 配信位置付きeventと本文byte offset・durability、承認更新を型付きpayloadで保持する。
- `ids.rs` — 役割ごとに異なる不透明IDと、精度を失わない正規十進u64文字列を定義する。
- `lib.rs` — Desktop IPCの値、有限byte列のcodec、純粋な受付・run遷移を定義する。I/Oや認可は行わない。
- `local_models.rs` — Bounded inventory observations, never model availability or execution authority.
- `mcp_settings.rs` — Host MCP registry and write-only secrets. Registry objects are validated by
- `orchestration.rs` — Bounded, secret-free V2 desktop projection of orchestration state.
- `request.rs` — M1の要求を専用paramsで表し、methodとsession対象の組合せをwire境界で検査する。
- `response.rs` — 成功resultと9種のエラーを排他的に表し、要求IDとmethodへの対応を検査する。
- `role_bindings.rs` — Durable local role choices. Values are metadata, never endpoint authority.
- `run_details.rs` — Saved run content, paged by the owner. Text is display data only; clients
- `run_state.rs` — 単一attemptの制御入力と観測終端を分離し、取消後の成功保持と再開禁止を純粋に判定する。
- `snapshot.rs` — 保存世代、下書き、設定、作業・親子run、回答可能な未解決承認のsnapshot値を保持する。
- `source_apply.rs` — Saved source-apply facts only. Neither a live stage nor an execution capability.
- `source_recovery.rs` — Bounded result-save recovery wire values. No I/O, authority or apply replay.
- `validate.rs` — hello前後の受付を純粋に判定する。構造の受理は認可・保存・実行開始を意味しない。
- `workspace_view.rs` — Bounded read-only workspace and explicit attachment projections.

## `crates/polaris-desktop-protocol/tests`

- `boundaries.rs` — ID・u64の値境界、有限frameの全分割とEOF、不正UTF-8・JSON・深さ上限を検査する。
- `browse.rs` — Home browsing wire: home-relative paths only and responses bound to the request.
- `mcp_settings.rs` — Host MCP settings: registry objects pass through, secret values stay write-only.
- `orchestration_v2.rs` — Wire compatibility tests for desktop orchestration snapshots and requests.
- `run_details.rs` — run.details pages: bounded fragments and run correlation.
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
- `home_browse.rs` — `--browse-home` helper: one-level listings and selected text files under
- `launch_arguments.rs` — Strict native launch metadata. Parsing performs no I/O or authorization.
- `lib.rs` — Durable storage IPC and explicitly configured native-owner execution.
- `memory_resources.rs` — Trusted-owner strict10 preflight. No project configuration or provider requests.
- `owner_launch.rs` — Trusted existing-store composition. No discovery, provisioning or replay.
- `package_manifest.rs` — Fixed packaged execution-helper metadata, read only by the trusted launcher.
- `production_source_factory.rs` — Trusted source recovery preparation; never source dispatch authority.
- `recovery_base.rs` — Trusted startup-only recovery placement; never a source-write grant.
- `recovery_socket.rs` — Fixed inherited endpoint setup, before runtime initialization or diagnostics.
- `recovery_transport.rs` — Bounded result-only framing. Closing this transport does not stop its owner.
- `shadow_finalizer.rs` — Compatibility surface for the shared answer-finalization owner.
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
- `mcp_catalog.rs` — One-server catalog check with no model and no store run record. The job
- `mcp_settings.rs` — Owner-served MCP registry and write-only secrets. Saved documents apply from
- `mcp_setup.rs` — Settings-screen detection and registration. The user's action there is the
- `projection.rs` — 子・workflow・確認状態を、上限付きの表示用summaryへ投影する。
- `real.rs` — Explicit trusted production integration; no provider discovery or source apply.
- `role_bindings.rs` — Trusted next-run role metadata configuration. Runtime binding happens only
- `run_details.rs` — `run.details`: one immutable details snapshot, paged by UTF-8-safe fragments.
- `session_admission.rs` — Durable admission adapter for the existing desktop `run.start` surface.
- `source_owner.rs` — Engine-owned source lifecycle. Only the I/O Job leaves the blocking owner;
- `source_recovery.rs` — Trusted result-only ingress. The reactor holds a sender, never the Writer.
- `source_view.rs` — Bounded saved source-apply pages; never a live job state or dispatch authority.

## `crates/polaris-desktop-service/src/engine/mcp_catalog`

- `cleanup_tests.rs` — Real stdio cancellation with a synthetic unconfirmed cleanup registry entry.

## `crates/polaris-desktop-service/src/engine/real`

- `tests.rs` — Offline real-engine ownership, durable transcript, and cancellation fixtures.

## `crates/polaris-desktop-service/src/engine/real/tests`

- `mcp_tests.rs` — Trusted factory injection through the real reactor, with no live provider.
- `project_routes.rs` — The owner's configured judge reaches a root run even without MCP.
- `structured_tests.rs` — Internal engine admission; the public resource gate remains closed.

## `crates/polaris-desktop-service/src/engine/real/tests/structured_tests`

- `recovery_tests.rs` — Separate-process continuation of real engine auxiliary failure states.

## `crates/polaris-desktop-service/src/home_browse`

- `tests.rs` — Synthetic home only; never the real account home.

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

## `crates/polaris-envkit`

- `Cargo.toml` — manifest for the polaris-envkit crate

## `crates/polaris-envkit/src`

- `acquisition.rs` — Shared schema-2 transfer capability. The management owner persists Started
- `adapter.rs` — Pure adapter resolution. Every operation references approved resources or
- `archive.rs` — Bounded extraction into a fresh private staging directory. Never use tar's
- `bundled.rs` — Fixed resource registry. No project-supplied file or plugin can replace a
- `catalog.rs` — Cheap catalog metadata. A name in this table is not an installation or
- `definition.rs` — Version-2 profile data. These definitions are loaded from bundled resources,
- `invocation.rs` — Render only revalidated compiled recipes. This module performs no I/O and
- `journal.rs` — Small append-only receipts inside one retained private operation directory.
- `lib.rs` — Fixed environment profiles shared by the CLI, TUI and typed host tool.
- `plan.rs` — Physical directory identity used by plan-bound host owners.
- `process.rs` — Offline compiled recipe execution on an existing controlled-process owner. Call on the
- `profile.rs` — Bundled reviewed bytes. Callers cannot substitute project/user profiles.
- `publication.rs` — Frozen postimages and create-only publication. The host retains the project
- `recipes.rs` — Compiled recipes; profiles cannot supply raw commands. Expanded program,
- `request.rs` — Typed host input, deliberately without executable, command or URL fields.
- `runner.rs` — The common bounded runner. Hosts own consent, the project lease and C43;
- `transfer.rs` — Plan-scoped, credential-free host transfers. The effect owner must persist
- `version.rs` — Profile-specific, bounded release selectors. Parsing has no I/O: channel

## `crates/polaris-envkit/src/adapter`

- `tests.rs` — Verify fixed plans, profile bindings and compatibility-only fallback.

## `crates/polaris-envkit/src/publication`

- `walk.rs` — Descriptor-anchored inventory. Child paths never follow an intermediate

## `crates/polaris-http`

- `Cargo.toml` — manifest for the polaris-http crate

## `crates/polaris-http/src`

- `artifact.rs` — Explicit, credential-free artifact transfers with pinned redirect origins.
- `distrust.rs` — Mandatory CA-key rejection for credential-free environment artifacts.
- `lib.rs` — Shared HTTP client setup.

## `crates/polaris-http/src/artifact`

- `tests.rs` — Local TLS fixtures only; no models, user credentials or external services.

## `crates/polaris-http/src/distrust`

- `keys.rs` — Pinned compromised certificate and public-key fingerprints.
- `tests.rs` — Verify actual-path certificate distrust and rejection before HTTP bytes.

## `crates/polaris-http/src/fixtures`

- `README.md` — TLSテスト用の証明書と鍵

## `crates/polaris-mcp`

- `Cargo.toml` — manifest for the polaris-mcp crate

## `crates/polaris-mcp/examples`

- `entry-manifest-owned-probe.rs` — Read-only inspection of a disposable, unpublished macOS package.

## `crates/polaris-mcp/src`

- `account_home.rs` — Compatibility name for OS account-home lookup.
- `admission.rs` — Shared read-only stdio resource admission for startup and store migration.
- `catalog.rs` — Bounded, SDK-independent catalog data. Descriptions and annotations are
- `content.rs` — SDK-independent result variants. Binary content is never serialized into a
- `entry_gate.rs` — Per-user production entry admission for the MCP protocol cutover.
- `lib.rs` — Host-owned MCP registration and bounded client data.
- `migration_observation.rs` — Read-only migration observations and strict, non-authoritative wire codecs.
- `native_mount.rs` — Compatibility adapter for the shared native descriptor mount identity.
- `registration.rs` — Validated host registrations, without SDK types or secret values.
- `secret_input.rs` — Masked, short-lived credential input for trusted interactive hosts.
- `store.rs` — Descriptor-relative private storage shared by trusted host surfaces.
- `template.rs` — Bounded RFC 6570 expansion for catalog resource templates. Expansion never
- `test_support.rs` — Loopback-only, scripted MCP server. No credentials or production endpoint.
- `transport.rs` — Private SDK boundary. Neither upstream errors nor credential-bearing clients

## `crates/polaris-mcp/src/catalog`

- `budget.rs` — Run-owned catalog accounting, independent of connection and future lifetimes.
- `index.rs` — Host-owned tool summaries saved by a dedicated catalog check. Text is

## `crates/polaris-mcp/src/store`

- `documents_v2.rs` — Closed v2 document envelopes for the store migration coordinator.
- `ledger_v1.rs` — Non-secret, whole-chain evidence for the per-user MCP credential ledger.
- `migration_v2.rs` — Private preparation for the approved MCP protocol migration.

## `crates/polaris-mcp/src/store/documents_v2`

- `tests.rs` — V2 credential and registry baseline envelope validation tests.

## `crates/polaris-mcp/src/store/ledger_v1`

- `file_store.rs` — Descriptor-bound durable ledger payload. This layer checks a caller-supplied
- `payload_head.rs` — Pure canonical head bytes shared by migration observations and ledger tests.
- `tests.rs` — Ledger epoch, baseline and operation-record validation tests.

## `crates/polaris-mcp/src/store/ledger_v1/file_store`

- `tests.rs` — Ledger file-store identity, persistence and namespace regression tests.

## `crates/polaris-mcp/src/store/migration_v2`

- `documents.rs` — Read-only v1 document pins and directly compared v2 baseline preparation.
- `inventory.rs` — Complete descriptor-pinned inventory for the migration's non-secret projection.
- `mapped.rs` — Fixed logical-final to inventoried-staging descriptor mapping. This is a
- `namespace.rs` — Retained descriptor and namespace observations, never an entry/stop grant.
- `paths.rs` — Pure typed-path migration for registered and materialized servers.
- `projection.rs` — Streaming non-secret migration projection from already enumerated children.
- `records.rs` — Closed, non-secret records for the approved store migration.

## `crates/polaris-mcp/src/store/migration_v2/documents`

- `tests.rs` — Migration document reads, staged payloads and source-content preservation tests.

## `crates/polaris-mcp/src/store/migration_v2/inventory`

- `tests.rs` — Migration tree inventory, inode replacement and directory-boundary tests.

## `crates/polaris-mcp/src/store/migration_v2/mapped`

- `tests.rs` — Mapped staging views and inventoried payload identity regression tests.

## `crates/polaris-mcp/src/store/migration_v2/paths`

- `tests.rs` — Deterministic registry destination and pinned-path migration mapping tests.

## `crates/polaris-mcp/src/store/migration_v2/projection`

- `tests.rs` — Canonical migration tree projection and namespace-change regression tests.

## `crates/polaris-mcp/src/store/migration_v2/records`

- `tests.rs` — External migration intent phases, anchors and canonical record validation tests.

## `crates/polaris-mcp/src/transport`

- `catalog.rs` — Bounded MCP catalog discovery and generation tracking.
- `failure.rs` — Closed metadata error classes survive SDK channel closure without raw errors.
- `handler.rs` — MCP client notification handling and unsupported server request rejection.
- `http.rs` — Restricted Streamable HTTP transport with explicit destination checks.
- `line_reader.rs` — Bounded line framing for managed MCP stdio.
- `operation.rs` — One-shot MCP operations with catalog validation and outcome classification.
- `sensitive.rs` — Sensitive MCP value handling at the transport boundary.
- `tests.rs` — Synthetic lifecycle and isolation checks for MCP transports.

## `crates/polaris-mcp/src/transport/operation`

- `tests.rs` — Synthetic MCP operation and resource-template regression checks.

## `crates/polaris-mcp/src/transport/tests`

- `apps.rs` — M2b synthetic app servers: research-shaped tool names, results and

## `crates/polaris-mcp/tests`

- `dependency_boundary.rs` — A01: the SDK stays a pinned client with no server, child-process or macro
- `migration_observation.rs` — Public migration observation identity and named-home replacement tests.

## `crates/polaris-memory`

- `Cargo.toml` — manifest for the polaris-memory crate

## `crates/polaris-memory/src`

- `conversation.rs` — Pending, scope-bound summaries for strict conversation history.
- `lib.rs` — Local, explicitly populated memory. Retrieved text is quoted historical evidence,
- `management.rs` — Bounded atomic management batches, durable project revisions and receipts.
- `search.rs` — Ranks legacy memory records into bounded, provenance-preserving excerpts.
- `structured.rs` — Version 3 structured-memory schema and host-owned domain values.
- `windows_storage.rs` — Dedicated SQLite VFS. Filesystem callbacks never forward to a pathname VFS.

## `crates/polaris-memory/src/conversation`

- `related.rs` — Bounded candidate generation, separate from structured-memory classification.

## `crates/polaris-memory/src/management`

- `tests.rs` — Atomic memory import, revision and forget regression tests.

## `crates/polaris-memory/src/structured`

- `receipt_stream_tests.rs` — Receipt completeness and snapshot boundaries for an owner-driven scan.
- `retrieval.rs` — Bounded, source-backed rows for one frozen structured-memory retrieval.
- `sources.rs` — Frozen source authority for the core owner's raw snapshot reader.

## `crates/polaris-memory/src/windows_storage`

- `tests.rs` — Windows protected SQLite storage, journal, locking and publication regression tests.

## `crates/polaris-memory/tests`

- `conversation.rs` — Conversation index scope, publication, provenance, and forgetting contracts.
- `memory.rs` — Persistence, isolation, retrieval budgets and explicit embedding behavior.
- `structured.rs` — Structured-memory schema, publication, identity, and bounded lookup regressions.
- `structured_overlay.rs` — Frozen-scope entity and fact overlay regression checks.
- `structured_retrieval.rs` — Frozen structured retrieval preserves identity, withdrawal and dependencies.

## `crates/polaris-provider`

- `Cargo.toml` — manifest for the polaris-provider crate

## `crates/polaris-provider/examples`

- `jev_probe.rs` — J-5: a real request to the configured decider, with synthetic questions

## `crates/polaris-provider/src`

- `attempts.rs` — Physical provider attempts, durable reservations and conservative settlement.
- `cache_pacing.rs` — Optional per-provider spacing of model request dispatches.
- `cache_prefix.rs` — Fixed, opt-in instructions for the cache-prefix cost experiment.
- `codex.rs` — A provider that speaks the Responses API using ChatGPT subscription
- `codex_metrics.rs` — Opt-in, content-free diagnostics for each actual Codex HTTP attempt.
- `codex_ws.rs` — The websocket transport for the Responses endpoint.
- `decision.rs` — A model that answers typed questions instead of writing text. It takes a
- `lib.rs` — Provider abstraction. Transport-dependent parts live in each
- `local.rs` — Bounded, metadata-only discovery of explicitly configured local runtimes.
- `local_inference.rs` — Inference through an explicitly selected loopback runtime.
- `model_memory.rs` — Planned additional local model allocations, never inferred from weight file size.
- `openai.rs` — OpenAI-compatible chat completions. Swap out `base_url` and you can hit
- `role.rs` — Immutable routing for catalog-validated child roles. No implicit fallback.
- `sse.rs` — Incrementally decodes SSE (text/event-stream). Pushing a byte chunk
- `turn_affinity.rs` — Opaque transport continuity scoped to one logical user turn.
- `web_search.rs` — Hosted web-search request policy and Responses-style output parsing.

## `crates/polaris-provider/src/codex`

- `fold.rs` — Incrementally folds Codex Responses API SSE events into one completion.

## `crates/polaris-provider/src/codex_ws`

- `tests.rs` — The two decisions that decide whether a continuation is safe to use.

## `crates/polaris-provider/src/decision`

- `capabilities.rs` — Adapter contracts, not evidence of a running model or current calibration.
- `fallback.rs` — Ordered typed failover with one request-local deadline and no background retries.
- `jev.rs` — J-4: the HTTP client for a System One decider.
- `observation.rs` — Per-request provenance; no prompt, answer text, credential or shared last-backend slot.
- `semif.rs` — The local typed decider, over a pinned SemIf helper.
- `system_one.rs` — Explicit System One HTTP compatibility contract; never inferred from a name.
- `telemetry.rs` — Request-local lifecycle observations, including dropped stages with unknown usage.
- `tests.rs` — Types and checking only; nothing is sent.
- `wire.rs` — Strict tagged answers and duplicate-key refusal shared by typed protocols.

## `crates/polaris-provider/src/decision/fallback`

- `tests.rs` — Synthetic failover, cancellation and per-request provenance; no model or endpoint.

## `crates/polaris-provider/src/decision/jev`

- `metrics.rs` — Opt-in physical Jev attempts, with a durable start before network dispatch.
- `metrics_tests.rs` — Physical attempts survive invalid answers and cancellation without payloads.
- `tests.rs` — A local stub stands in for the service. No real endpoint is contacted

## `crates/polaris-provider/src/decision/system_one`

- `tests.rs` — Synthetic loopback protocol tests; no real service or credential.

## `crates/polaris-provider/src/decision/telemetry`

- `tests.rs` — Lifecycle checks use a pending fake adapter and metadata-only observer.

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

## `crates/polaris-sandbox/src/controlled/native`

- `managed.rs` — Long-lived stdio uses the same native preparation, reservation and unreaped

## `crates/polaris-sandbox/tests`

- `isolated_macos.rs` — macOS isolated filesystem, environment, scratch, and ordinary tool compatibility tests.
- `isolated_parent_access.rs` — Explicit macOS runtime test. No arbitrary PID or real parent environment is

## `crates/polaris-skills`

- `Cargo.toml` — manifest for the polaris-skills crate

## `crates/polaris-skills/resources/skills/document-writing`

- `SKILL.md` — 文書を書く

## `crates/polaris-skills/resources/skills/envkit`

- `SKILL.md` — SKILL.md

## `crates/polaris-skills/resources/skills/smallest-change`

- `SKILL.md` — 最小の変更を選ぶ

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
- `builtin.rs` — The agent types that ship inside the binary.
- `discovery.rs` — Skill discovery. A single corrupt skill must not take down the whole
- `frontmatter.rs` — SKILL.md frontmatter parsing. Validates only the constraints the specification lays down; adds no constraints of its own.
- `lib.rs` — Loading of skills that conform to the Agent Skills specification. Adds no frontmatter fields of its own.
- `workflow_profile.rs` — Deterministic resolution of the optional shipped workflow skill profile.

## `crates/polaris-skills/tests`

- `workflow_profile.rs` — Mandatory workflow skill resolution, source identity, and token budgets.

## `crates/polaris-tools`

- `Cargo.toml` — manifest for the polaris-tools crate

## `crates/polaris-tools/src`

- `account_home.rs` — Trusted OS account home lookup. Never consults or changes the environment.
- `bash.rs` — `bash` tool. Runs a command inside a confined `/bin/sh`.
- `edit.rs` — `edit` tool. Goes through the same confined path as `write`.
- `entry_kernel.rs` — Shared entry wire and descriptor primitives for trusted host owners.
- `isolated_read.rs` — OS隔離されたhelper内でのみ本文を読む。親側の直読fallbackは用意しない。
- `lib.rs` — polaris's built-in tools. The always-on tool set never exceeds 6 tools.
- `native_mount.rs` — Native mount identity of an actual open descriptor, never an authorization.
- `path_policy.rs` — Decides which paths reading is denied on. Leans toward avoiding missed
- `predicate.rs` — Predicts ahead of time whether a write will be denied.
- `read.rs` — read tool. Line numbers are attached to the output so the model can
- `schema_validate.rs` — subagent の結果を、型が宣言した JSON Schema に照合するだけの薄い
- `skill.rs` — skill tool. Returns the body on an exact name match, otherwise returns a
- `write.rs` — `write` tool. The actual write happens inside a confined child process.

## `crates/polaris-tools/src/entry_kernel`

- `package.rs` — Closed inventory of the actual macOS package used for migration admission.

## `crates/polaris-tools/src/path_policy`

- `tree_admission.rs` — Shared descriptor-based tree inspection for sanitized copies and strict

## `crates/polaris-tools/src/path_policy/tree_admission`

- `registered.rs` — Admission of a host-registered directory without copying or modifying it.

## `crates/polaris-tools/src/skill`

- `bm25.rs` — BM25 ランキング。トークナイズ・語幹化・同義語展開・スコアリングだけを
- `near_universal.rs` — 「ほぼ常に関連する」skill の選定。BM25 のスコアリングを一切知らず、

## `crates/polaris-tui`

- `Cargo.toml` — manifest for the polaris-tui crate

## `crates/polaris-tui/src`

- `account.rs` — Account commands never put OAuth codes or token values into TUI history.
- `approver.rs` — The `Approver` that runs inside the TUI: draws a modal over the current
- `clipboard.rs` — Copies text to the system clipboard. macOS uses `/usr/bin/pbcopy`, whose
- `command_dispatch.rs` — 保存済みsession、memory、MCPのlocal command実行をまとめる。
- `continuation_tests.rs` — Shared continuation recovery through real raw/receipt owners and fake backends.
- `conversation_view.rs` — 会話のtranscript表示だけを所有する。
- `draft.rs` — An unsent draft belongs to its conversation and never enters raw history.
- `environment_commands.rs` — Direct local management commands. Work stays on a retained host worker;
- `file_input.rs` — File links remain ordinary Markdown in admission, drafts, and history.
- `file_reveal.rs` — Reveal an explicitly clicked local file in Finder. No file body is opened.
- `input.rs` — Pure keystroke-to-action mapping for the input box. Kept separate from
- `input_preparation_tests.rs` — TUI initialization and cloned run ownership preserve request-only input preparation.
- `instruction_commands.rs` — Durable next-run instructions in the existing TUI admission family.
- `lib.rs` — The polaris interactive TUI. Entered by `polaris-cli` when `--prompt`
- `local_actions.rs` — User-triggered local actions share the project lease and management journal.
- `mcp_commands.rs` — MCP controls change next-run admission; they never replace a running runtime.
- `memory.rs` — Explicitly enabled project memory and pre-compaction archival.
- `memory_commands.rs` — Explicit host memory commands. Confirmation and results stay in the TUI;
- `onboarding.rs` — The onboarding screen: shown by `polaris-cli` when the interactive TUI
- `persist.rs` — Session persistence: one JSON `Message` per line.
- `render.rs` — Pure rendering: turns a `Session` + input state into terminal cells.
- `resume_effect.rs` — Exact saved-session selection before a host activates a prepared candidate.
- `rewind_ui.rs` — TUI-only rewind interaction state. Checkpoint eligibility and file changes
- `role_commands.rs` — Role bindings select existing fixed tier providers; they create no provider.
- `runtime_commands.rs` — Commands over the live host control plane; no desktop-state mirror.
- `runtime_effects.rs` — Runtime management requests use the bound activity lease, never source mutation.
- `sandbox_commands.rs` — Explicit argv execution under the startup ceiling and durable C38 receipt.
- `selection.rs` — Mouse drag-to-select over the conversation history. Operates entirely
- `session_activation.rs` — 保存会話の設定とプロセス所有サービスを実行中のセッションへ適用する。
- `session_commands.rs` — Saved session controls use the conversation owner and shared project lease.
- `sessions.rs` — Enumerates saved conversations under `~/.polaris/sessions/` for the
- `signal.rs` — Linux process-signal capture for the interactive terminal lifetime.
- `slash.rs` — Slash commands: local, client-side commands recognized when the input
- `source_apply.rs` — Linux terminal adapter for the existing source-bound native owner.
- `time.rs` — A tiny, dependency-free UTC timestamp formatter — just enough to
- `tool_memory.rs` — Immutable, scoped tool output storage behind the reserved memory read path.
- `windows_admission_tests.rs` — Windows TUI admission and slash command tests that preserve rejected work and state.
- `windows_read_only.rs` — Provider-free Windows browsing while native mutation owners are unavailable.
- `workspace_commands.rs` — Workspace projections remain UI-only and bound to the startup source.

## `crates/polaris-tui/src/mcp_commands`

- `journal.rs` — MCP management effects retain the existing irreversible rewind boundary.
- `management_tests.rs` — End-to-end TUI coverage for the MCP management owners.
- `operations.rs` — TUI MCP catalog and registration routes use the admitted domain owners.
- `secret.rs` — Dedicated MCP credential editing never returns entered bytes to the composer.

## `crates/polaris-tui/src/role_commands`

- `tests.rs` — TUI role and tier controls with fixed-provider and permission checks.

## `crates/polaris-tui/src/sandbox_commands`

- `tests.rs` — TUI sandbox controls and startup permission-ceiling regression tests.

## `crates/polaris-tui/src/session_commands`

- `tests.rs` — TUI session settings and history controls without provider requests.

## `crates/polaris-tui/src/windows_read_only`

- `tests.rs` — Windows read-only TUI interaction, secret scrubbing and terminal restoration tests.

## `docs`

- `architecture.md` — アーキテクチャ
- `codex-update-efficiency.md` — Codex更新と使用量削減策の適用
- `command-reference.md` — コマンドの役割と出力
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
- `system-overview.md` — システム構成図
- `testing.md` — テスト
- `tool-preselection.md` — ツールの事前選定
- `usage.md` — 利用ガイド

## `docs/releases`

- `v1.0.0.md` — Polaris 暫定v1.0.0 — macOS TUI

## `scripts/fixtures/tui-parity-ledger`

- `d42-without-addendum.md` — Fixed synthetic parent inventory and Linux ledger
- `duplicate-id.md` — Fixed synthetic parent inventory and Linux ledger
- `valid-129.md` — Fixed synthetic parent inventory and Linux ledger
- `valid-130-with-addendum.md` — Fixed synthetic parent inventory and Linux ledger
- `wrong-set-129.md` — Fixed synthetic parent inventory and Linux ledger

## `skills/mutation-check`

- `SKILL.md` — 変異で確かめる

## `skills/verify-a-change`

- `SKILL.md` — 変更を検証する
