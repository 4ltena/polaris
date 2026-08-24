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
- `README.md` — polaris
- `rust-toolchain.toml` — pinned toolchain

## `agents/file-inspector`

- `SKILL.md` — SKILL.md

## `crates/polaris-auth`

- `Cargo.toml` — manifest for the polaris-auth crate

## `crates/polaris-auth/src`

- `api_key.rs` — Storage for a plain OpenAI API key at `~/.polaris/api_key.json`.
- `lib.rs` — Lifecycle of ChatGPT subscription auth (OAuth) and storage of credentials.
- `login.rs` — Building the authorization URL, and receiving the callback exactly once.
- `pkce.rs` — PKCE (RFC 7636) verifier and challenge.
- `store.rs` — Storage for credentials. `~/.polaris/auth.json`, 0600, atomic writes.
- `token.rs` — Exchange and refresh against `/oauth/token`.

## `crates/polaris-cli`

- `Cargo.toml` — manifest for the polaris-cli crate

## `crates/polaris-cli/src`

- `main.rs` — Entry point for the `polaris` binary. Decides the endpoint from environment

## `crates/polaris-cli/tests`

- `cli.rs` — Integration tests for the CLI binary. Launches the real process to verify behavior.
- `confined_helper.rs` — Integration test that launches the real `polaris` binary as a helper
- `subcommands.rs` — Pins down that subcommands are actually reachable.

## `crates/polaris-core`

- `Cargo.toml` — manifest for the polaris-core crate

## `crates/polaris-core/src`

- `agent.rs` — The agent loop. Returns the body text at the point tool calls stop.
- `approval.rs` — Approval boundary. `sandbox_mode` sets the technical boundary;
- `audit.rs` — Append-only audit log. Not signed: in-process, the entity signing and the
- `budget.rs` — Measurement of the always-on context. Numbers are backed by measurement,
- `config.rs` — Loads config files. Not existing is normal; being malformed is not.
- `constitution.rs` — The part of the always-on context that the harness does not own. The
- `lib.rs` — Entry point for polaris-core. Ties together the budget, constitution, and prompt modules.
- `project.rs` — Resolves the project root.
- `prompt.rs` — The single place that assembles the set of things loaded every turn.
- `session.rs` — Message history. In M1, this is append-only — no compaction, no
- `spawn.rs` — The `spawn` tool's implementation. Type discovery reuses
- `stop.rs` — Stop conditions. No automatic recovery is attempted. Continuing to spin

## `crates/polaris-core/src/secret_screen`

- `mod.rs` — Remna's privacy filter. Pure logic that runs **before** a captured event

## `crates/polaris-core/tests`

- `filemap.rs` — A snapshot test that confirms `docs/filemap.md` matches the actual state of the repository.

## `crates/polaris-provider`

- `Cargo.toml` — manifest for the polaris-provider crate

## `crates/polaris-provider/src`

- `codex.rs` — A provider that speaks the Responses API using ChatGPT subscription
- `lib.rs` — Provider abstraction. Transport-dependent parts live in each
- `openai.rs` — OpenAI-compatible chat completions. Swap out `base_url` and you can hit
- `sse.rs` — Incrementally decodes SSE (text/event-stream). Pushing a byte chunk

## `crates/polaris-sandbox`

- `Cargo.toml` — manifest for the polaris-sandbox crate

## `crates/polaris-sandbox/src`

- `confine.rs` — Launching a process under confinement. Dispatches to a per-platform
- `helper.rs` — The mutation operation executed inside the confined child.
- `lib.rs` — Sandbox policy definitions, and delegation to OS mechanisms.
- `linux.rs` — Linux enforcement. Applies a landlock ruleset to the process itself,
- `macos.rs` — macOS enforcement. Builds a Seatbelt profile at runtime and launches the
- `policy.rs` — Policy and writable roots. Roots are normalized at construction time.
- `stage.rs` — Stage the helper binary outside the writable roots.

## `crates/polaris-skills`

- `Cargo.toml` — manifest for the polaris-skills crate

## `crates/polaris-skills/src`

- `agent_type.rs` — subagent 型の定義（`agents/<type>/SKILL.md`）の解析と discovery。
- `discovery.rs` — Skill discovery. A single corrupt skill must not take down the whole
- `frontmatter.rs` — SKILL.md frontmatter parsing. Validates only the constraints the specification lays down; adds no constraints of its own.
- `lib.rs` — Loading of skills that conform to the Agent Skills specification. Adds no frontmatter fields of its own.

## `crates/polaris-tools`

- `Cargo.toml` — manifest for the polaris-tools crate

## `crates/polaris-tools/src`

- `bash.rs` — `bash` tool. Runs a command inside a confined `/bin/sh`.
- `edit.rs` — `edit` tool. Goes through the same confined path as `write`.
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
- `input.rs` — Pure keystroke-to-action mapping for the input box. Kept separate from
- `lib.rs` — The polaris interactive TUI. Entered by `polaris-cli` when `--prompt`
- `onboarding.rs` — The onboarding screen: shown by `polaris-cli` when the interactive TUI
- `persist.rs` — Session persistence: one JSON `Message` per line.
- `render.rs` — Pure rendering: turns a `Session` + input state into terminal cells.
- `sessions.rs` — Enumerates saved conversations under `~/.polaris/sessions/` for the
- `slash.rs` — Slash commands: local, client-side commands recognized when the input
- `time.rs` — A tiny, dependency-free UTC timestamp formatter — just enough to

## `docs`

- `filemap.md` — File map

## `docs/superpowers`

- `CURRENT.md` — polaris 現況

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

## `docs/superpowers/specs`

- `2026-08-16-polaris-harness-design.md` — polaris 設計仕様
- `2026-08-18-polaris-codex-provider-design.md` — polaris Codex プロバイダ 設計
- `2026-08-20-polaris-skill-bm25-router-design.md` — polaris skill ルータ BM25 化 設計
- `2026-08-21-polaris-tui-design.md` — polaris TUI 設計
- `2026-08-21-polaris-tui-onboarding-design.md` — polaris TUI 初回起動オンボーディング画面 設計
- `2026-08-21-polaris-tui-v2-design.md` — polaris TUI v2 設計
- `2026-08-24-polaris-files-md-autogen-design.md` — polaris ディレクトリ別 `files.md` 自動生成 設計書
