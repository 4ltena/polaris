# polaris TUI v2 (v0.4.0 "Regulus") Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add token-usage tracking, a status bar, retrospective tool-call visualization, and minimal markdown/color formatting to the polaris TUI shipped in v0.3.0.

**Architecture:** `polaris_provider::CompletionResponse` gains `usage: Option<Usage>`, parsed from each provider's real API response. `polaris_core::agent::run`'s return type changes from `Result<String, AgentError>` to `Result<AgentOutcome, AgentError>` (`{ text, usage }`), accumulating `Usage` across a turn's internal `provider.complete()` calls. No callback/observer hook is added to `agent::run` — tool-call visibility is retrospective, read back out of `Session.messages` (which already records every call and result) by `polaris-tui`'s renderer. `render.rs` gains a status-bar row, un-filters `Role::Tool` messages into a formatted, truncated display, and adds a hand-rolled inline formatter for `**bold**`, `` `code` ``, and fenced code blocks — no new markdown crate.

**Tech Stack:** Rust 1.96, edition 2024, existing `polaris-core`/`polaris-provider`/`polaris-tui` crates. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-21-polaris-tui-v2-design.md`

## Global Constraints

- Rust 1.96.0, edition 2024.
- No new crate dependency (no `pulldown-cmark` or similar) — the markdown formatting is a hand-rolled scanner limited to `**bold**`, `` `inline code` ``, and fenced ` ``` ` code blocks.
- `Usage` extraction failure is never a hard error — a missing/malformed `usage` field in a provider response yields `None`, not `ProviderError`. Many existing test fixtures across `openai.rs`/`codex.rs` omit `usage` entirely and must keep passing unchanged.
- `agent::run`'s return-type change (`String` → `AgentOutcome`) does not change tool-dispatch behavior, stop-condition behavior, or audit-log behavior — only what's returned on success.
- Every new render path (tool-call names/arguments, tool-result previews, status-bar text, markdown-formatted spans) must go through the existing `sanitize()` in `render.rs` — this continues v0.3.0's terminal-escape-injection fix; a new render path that skips `sanitize()` is a regression, not a new feature.
- `Usage` is a post-hoc report of what a response cost. It must never be fed back into `prompt::assemble_always_on` or otherwise influence what's sent on a future turn — the 990-token always-on budget invariant (`crates/polaris-core/src/budget.rs`) is unrelated to this plan and must stay untouched.
- `crates/polaris-core/tests/filemap.rs` asserts `docs/filemap.md` matches the repository. Any task that adds/removes a `.rs` file must end with `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap` before committing.

---

### Task 1: `Usage` type and OpenAI-side parsing

**Files:**
- Modify: `crates/polaris-provider/src/lib.rs`
- Modify: `crates/polaris-provider/src/openai.rs`

**Interfaces:**
- Produces: `polaris_provider::Usage` (`pub struct { pub input_tokens: u32, pub output_tokens: u32, pub total_tokens: u32 }`, `#[derive(Debug, Clone, Copy, Default)]`), `CompletionResponse.usage: Option<Usage>` (new field). Task 3 (`agent::run`) and Task 2 (`codex.rs`) both consume `Usage`.

- [ ] **Step 1: Add `Usage` and the new field**

In `crates/polaris-provider/src/lib.rs`, add above `CompletionResponse`:

```rust
/// Token usage reported by a single provider response. `None` on
/// `CompletionResponse` means the provider's response didn't carry a
/// usable `usage` field — never a hard error, since this is a
/// after-the-fact report, not something the agent loop depends on to
/// function.
#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
}
```

Change:

```rust
#[derive(Debug, Clone, Default)]
pub struct CompletionResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}
```

to:

```rust
#[derive(Debug, Clone, Default)]
pub struct CompletionResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
}
```

- [ ] **Step 2: Fix every existing `CompletionResponse { .. }` literal that doesn't use `..Default::default()`**

Run: `cargo build --workspace 2>&1 | grep "missing field" | head -40`

This will list every construction site missing the new `usage` field (mock providers in `agent.rs`'s tests, `openai.rs`'s own tests, `codex.rs`'s tests, any other test crate). For each one:
- If it already ends with `..Default::default()`, no change needed (already covered).
- Otherwise add `usage: None,` as a field (or switch the literal to end with `, ..Default::default()` if that reads more naturally in context — your call, but stay consistent within a file rather than mixing both styles in the same file).

Run `cargo build --workspace` again and repeat until it's clean.

- [ ] **Step 3: Write the failing usage-parsing test for `openai.rs`**

Find `openai.rs`'s existing `#[cfg(test)] mod tests` block (it already mocks the chat-completions response shape with `wiremock`). Add:

```rust
    #[tokio::test]
    async fn usage_is_parsed_from_a_normal_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "hi", "tool_calls": null}}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
            })))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::new(server.uri(), "key".into(), "model".into()).expect("client");
        let res = provider
            .complete(CompletionRequest {
                system: String::new(),
                messages: vec![Message::user("hi")],
                tools: vec![],
            })
            .await
            .expect("should succeed");

        let usage = res.usage.expect("usage should be present");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[tokio::test]
    async fn a_response_without_usage_yields_none_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "hi", "tool_calls": null}}]
            })))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::new(server.uri(), "key".into(), "model".into()).expect("client");
        let res = provider
            .complete(CompletionRequest {
                system: String::new(),
                messages: vec![Message::user("hi")],
                tools: vec![],
            })
            .await
            .expect("a missing usage field must not fail the whole response");

        assert!(res.usage.is_none());
    }
```

Adjust the mock-setup style (endpoint path, `MockServer`/`Mock`/`ResponseTemplate` imports) to match whatever pattern the existing tests in this file already use — read a couple of the existing tests in this `mod tests` block first and follow their exact conventions (server setup helper, response-building helper, etc.) rather than introducing a second style.

- [ ] **Step 4: Run to see the new tests fail**

Run: `cargo test -p polaris-provider usage_is_parsed`
Expected: FAIL — `res.usage` doesn't exist as a populated field yet (it's always `None` from Step 1/2's mechanical fix).

- [ ] **Step 5: Parse `usage` from the response JSON**

In `openai.rs`, find the line that builds the final `Ok(CompletionResponse { text, tool_calls })` (or `{ text, tool_calls, usage: None }` after Step 2's mechanical fix) at the end of the response-decoding function — the same function that already extracts `v.get("choices")...`. Immediately before that return, add:

```rust
        let usage = v.get("usage").and_then(|u| {
            let input_tokens = u.get("prompt_tokens")?.as_u64()? as u32;
            let output_tokens = u.get("completion_tokens")?.as_u64()? as u32;
            let total_tokens = u.get("total_tokens")?.as_u64()? as u32;
            Some(crate::Usage {
                input_tokens,
                output_tokens,
                total_tokens,
            })
        });
```

(`v` is already in scope — it's the whole parsed response body used a few lines above for `v.get("choices")`.) Then change the final construction to include `usage`:

```rust
        Ok(CompletionResponse { text, tool_calls, usage })
```

- [ ] **Step 6: Run to see the tests pass**

Run: `cargo test -p polaris-provider usage`
Expected: PASS — both new tests, plus every pre-existing `openai.rs` test still green (they never set a `usage` field in their mock JSON, so they exercise the `None` path and must be unaffected).

- [ ] **Step 7: Run the full provider test suite and commit**

Run: `cargo test -p polaris-provider`
Expected: all pass.

```bash
git add crates/polaris-provider/src/lib.rs crates/polaris-provider/src/openai.rs
git commit -m "feat(polaris-provider): add Usage and parse it from OpenAI-compatible responses"
```

---

### Task 2: Codex-side usage parsing

**Files:**
- Modify: `crates/polaris-provider/src/codex.rs`

**Interfaces:**
- Consumes: `polaris_provider::Usage` (Task 1).
- Produces: `CodexProvider`'s `CompletionResponse.usage` populated from the SSE stream's `response.completed` event.

- [ ] **Step 1: Write the failing test**

`codex.rs`'s tests already build synthetic SSE frames via a `frame(kind, extra)` test helper and feed them through `Folder::push`. Find the test that sends a `"response.completed"` frame with an empty payload (`frame("response.completed", serde_json::json!({}))`) and add a new test alongside it:

```rust
    #[test]
    fn usage_is_captured_from_the_completed_event() {
        let mut f = Folder::new();
        f.push(&frame(
            "response.completed",
            serde_json::json!({
                "response": {
                    "usage": {
                        "input_tokens": 12,
                        "output_tokens": 8,
                        "total_tokens": 20
                    }
                }
            }),
        ))
        .expect("push should succeed");

        let res = f.finish().expect("finish should succeed");
        let usage = res.usage.expect("usage should be present");
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.total_tokens, 20);
    }

    #[test]
    fn a_completed_event_without_usage_yields_none_not_an_error() {
        let mut f = Folder::new();
        f.push(&frame("response.completed", serde_json::json!({})))
            .expect("push should succeed");

        let res = f.finish().expect("finish should succeed");
        assert!(res.usage.is_none());
    }
```

Check the exact shape `frame()` wraps its `extra` argument in (some implementations put `extra` directly as the event's JSON body, others nest it under a `data:` line differently) by reading the `frame` helper's definition in this file, and adjust the nesting in the test's `serde_json::json!({...})` above if `v.get("response")` in your Step 3 code below needs a different path to reach `usage` given how `frame()` actually assembles the event.

- [ ] **Step 2: Run to see the tests fail**

Run: `cargo test -p polaris-provider usage_is_captured`
Expected: FAIL — `Folder` has no `usage` field yet, so `res.usage` doesn't exist / is always `None`.

- [ ] **Step 3: Capture usage on `response.completed`**

In `Folder`'s struct definition, add a field:

```rust
pub struct Folder {
    decoder: sse::SseDecoder,
    text: String,
    tool_calls: Vec<ToolCall>,
    completed: bool,
    usage: Option<crate::Usage>,
}
```

Update `Folder::new()`'s constructor to initialize `usage: None,`.

In `push`'s match on the event type, change:

```rust
                "response.completed" => self.completed = true,
```

to:

```rust
                "response.completed" => {
                    self.completed = true;
                    self.usage = v.pointer("/response/usage").and_then(|u| {
                        let input_tokens = u.get("input_tokens")?.as_u64()? as u32;
                        let output_tokens = u.get("output_tokens")?.as_u64()? as u32;
                        let total_tokens = u.get("total_tokens")?.as_u64()? as u32;
                        Some(crate::Usage {
                            input_tokens,
                            output_tokens,
                            total_tokens,
                        })
                    });
                }
```

Update `finish()`'s final construction:

```rust
        Ok(CompletionResponse {
            text: self.text,
            tool_calls: self.tool_calls,
            usage: self.usage,
        })
```

If Step 1's test needed a different JSON path than `/response/usage` (per that step's note about `frame()`'s actual nesting), use the same path here that the test actually exercises.

- [ ] **Step 4: Run to see the tests pass**

Run: `cargo test -p polaris-provider usage`
Expected: PASS — both new `codex.rs` tests, both `openai.rs` tests from Task 1, and every pre-existing test in this file (none of which set a `usage` payload, so they exercise the `None` path).

- [ ] **Step 5: Run the full provider test suite and commit**

Run: `cargo test -p polaris-provider`
Expected: all pass.

```bash
git add crates/polaris-provider/src/codex.rs
git commit -m "feat(polaris-provider): parse usage from the Codex SSE stream"
```

---

### Task 3: `agent::run` returns `AgentOutcome`

**Files:**
- Modify: `crates/polaris-core/src/agent.rs`

**Interfaces:**
- Consumes: `polaris_provider::Usage` (Task 1).
- Produces: `polaris_core::agent::AgentOutcome` (`pub struct { pub text: String, pub usage: polaris_provider::Usage }`, `pub`). `agent::run`'s signature changes from `-> Result<String, AgentError>` to `-> Result<AgentOutcome, AgentError>`. Task 4 (`polaris-cli`, `polaris-tui`) is the external consumer.

This task touches only `agent.rs` (both the loop and its own test module) — the two external call sites (`polaris-cli`'s one-shot path, `polaris-tui`'s `run()`) are Task 4.

- [ ] **Step 1: Add `AgentOutcome` and change the return type**

Add above `pub async fn run`:

```rust
/// What a successful turn produced: the final text, and the token usage
/// accumulated across every `provider.complete()` call the turn made
/// (a turn that used a tool calls the provider more than once). A
/// response whose `usage` came back `None` contributes nothing to this
/// total rather than failing the turn — usage is a best-effort report,
/// never something the loop depends on to function.
pub struct AgentOutcome {
    pub text: String,
    pub usage: polaris_provider::Usage,
}
```

Change the signature:

```rust
// before
) -> Result<String, AgentError> {

// after
) -> Result<AgentOutcome, AgentError> {
```

Add a running total right after the `loop {` opens (before the `stop.observe_turn()` check, so it starts at zero for the whole call regardless of how many iterations follow):

```rust
    let mut usage = polaris_provider::Usage::default();
    loop {
```

After the existing `let res = provider.complete(...).await?;` line, add:

```rust
        if let Some(u) = res.usage {
            usage.input_tokens += u.input_tokens;
            usage.output_tokens += u.output_tokens;
            usage.total_tokens += u.total_tokens;
        }
```

Change the early-return-on-no-tool-calls branch:

```rust
// before
        if res.tool_calls.is_empty() {
            session.push_assistant(&res.text);
            return Ok(res.text);
        }

// after
        if res.tool_calls.is_empty() {
            session.push_assistant(&res.text);
            return Ok(AgentOutcome {
                text: res.text,
                usage,
            });
        }
```

- [ ] **Step 2: Update every test in this file's `mod tests` block**

Every `run(...).await.expect(...)` call whose result is bound to a variable (`let out = run(...)...`) now returns an `AgentOutcome`, not a `String`. Find every `assert_eq!(out, "...")` in this file (there are 6: in `runs_tool_then_returns_final_text`, `interleaved_success_does_not_trip_the_repeated_error_stop`, `dispatches_the_skill_tool`, `the_write_tool_reads_the_argument_names_its_schema_declares`, `a_denied_write_comes_back_as_a_tool_result_not_a_loop_failure`, `a_write_to_a_hardlinked_path_inside_the_root_is_stopped_by_the_gate_before_anything_runs`, `an_edit_to_a_hardlinked_path_inside_the_root_is_stopped_by_the_gate_before_anything_runs`, `bash_attempts_and_reports_without_ever_consulting_the_approver` — 8 sites, not 6; count them yourself by grepping this file for `assert_eq!(out,`) and change each call site from:

```rust
        let out = run(...)
            .await
            .expect("...");
        assert_eq!(out, "some text");
```

to:

```rust
        let out = run(...)
            .await
            .expect("...")
            .text;
        assert_eq!(out, "some text");
```

(just append `.text` after `.expect(...)`, nothing else changes). The remaining call sites in this file discard the return value entirely (just `run(...).await.expect("should succeed");` with no `let`) — those need no change at all, since they never touch `.text` or `.usage`.

Run: `cargo build -p polaris-core --tests 2>&1 | grep "cannot find\|mismatched types\|no field" | head -40` and fix any remaining call site the above description missed (grep for `.expect(` near every `run(` call in this file to be sure — there may be a call site whose `.expect(...)` message differs from the examples quoted above).

- [ ] **Step 3: Add a test that usage accumulates across a tool-calling turn**

Add to `mod tests`:

```rust
    #[tokio::test]
    async fn usage_accumulates_across_a_tool_calling_turn() {
        let dir = tempfile::tempdir().expect("temp directory");
        let target = dir.path().join("a.txt");
        std::fs::write(&target, "hello\n").expect("cannot write");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({ "path": target.to_str().unwrap() }),
                    }],
                    usage: Some(polaris_provider::Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        total_tokens: 15,
                    }),
                },
                CompletionResponse {
                    text: "it was 1 line".into(),
                    tool_calls: vec![],
                    usage: Some(polaris_provider::Usage {
                        input_tokens: 20,
                        output_tokens: 3,
                        total_tokens: 23,
                    }),
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("how many lines is a.txt");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("cannot open");
        let mut stop = StopTracker::new(10);

        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let outcome = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
        .await
        .expect("should succeed");

        assert_eq!(outcome.text, "it was 1 line");
        assert_eq!(outcome.usage.input_tokens, 30);
        assert_eq!(outcome.usage.output_tokens, 8);
        assert_eq!(outcome.usage.total_tokens, 38);
    }

    #[tokio::test]
    async fn a_response_with_no_usage_contributes_zero_not_a_failure() {
        let dir = tempfile::tempdir().expect("temp directory");
        let p = Scripted {
            replies: Mutex::new(vec![CompletionResponse {
                text: "done".into(),
                tool_calls: vec![],
                usage: None,
            }]),
        };

        let mut session = Session::new();
        session.push_user("hi");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("cannot open");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let outcome = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
        .await
        .expect("should succeed");

        assert_eq!(outcome.usage.total_tokens, 0);
    }
```

Note this requires every OTHER `CompletionResponse { .. }` literal already present in this file's `Scripted` provider setups to compile — Task 1 Step 2 already made sure every such literal across the workspace has a `usage` field (or `..Default::default()`), so this file's existing literals should already compile; this step is only adding two new ones.

- [ ] **Step 4: Run the full polaris-core test suite**

Run: `cargo test -p polaris-core`
Expected: all pass, including the two new tests and every existing `agent.rs` test updated in Step 2.

- [ ] **Step 5: Regenerate the filemap and commit**

This task adds no new `.rs` file, so filemap regeneration should be a no-op — run it anyway to confirm:

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add crates/polaris-core/src/agent.rs
git commit -m "feat(polaris-core): return AgentOutcome (text + accumulated usage) from agent::run"
```

---

### Task 4: Wire `AgentOutcome` through both CLI paths

**Files:**
- Modify: `crates/polaris-cli/src/main.rs`
- Modify: `crates/polaris-tui/src/lib.rs`

**Interfaces:**
- Consumes: `polaris_core::agent::AgentOutcome` (Task 3).
- Produces: `polaris_tui::RunArgs` gains `provider_name: String` and `model_name: String` fields. `run()`'s loop maintains a running `polaris_provider::Usage` total across turns, updated on every successful `agent::run` call.

- [ ] **Step 1: Capture the resolved model name in `main.rs`**

In `crates/polaris-cli/src/main.rs`, the model string is currently resolved and immediately consumed inside each `match provider_name.as_str() { "openai" => {...} "codex" => {...} }` arm (`let model = model.unwrap_or_else(...)`), so there's no single variable holding it after the match. Add a variable before the match and set it inside each arm right after the model is resolved:

```rust
    let mut model_name = String::new();

    let provider: Box<dyn polaris_provider::Provider> = match provider_name.as_str() {
        "openai" => {
            // Keep the existing setup as-is. Behavior does not change.
            let base = std::env::var("POLARIS_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
            let key = match std::env::var("POLARIS_API_KEY") {
                Ok(k) => k,
                Err(_) => {
                    eprintln!("POLARIS_API_KEY is not set");
                    return ExitCode::FAILURE;
                }
            };
            let model = model.unwrap_or_else(|| "gpt-5.4".to_string());
            model_name = model.clone();
            match OpenAiProvider::new(base, key, model) {
                Ok(p) => Box::new(p),
                Err(e) => {
                    eprintln!("Can't build the client: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        "codex" => {
            let store = match polaris_auth::store::default_path() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Can't determine where to store credentials: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let model = model.unwrap_or_else(|| polaris_provider::codex::DEFAULT_MODEL.to_string());
            model_name = model.clone();
            Box::new(polaris_provider::codex::CodexProvider::new(
                polaris_provider::codex::ENDPOINT_BASE.to_string(),
                model,
                std::sync::Arc::new(AuthTokens {
                    issuer: polaris_auth::ISSUER.to_string(),
                    store,
                }),
            ))
        }
        other => {
            eprintln!("POLARIS_PROVIDER is an unknown value {other}. Specify openai or codex");
            return ExitCode::FAILURE;
        }
    };
```

(only the two `model_name = model.clone();` lines and the `let mut model_name = String::new();` line before the match are new — everything else in this block is unchanged.)

- [ ] **Step 2: Update the one-shot path's `Ok` arm**

Find:

```rust
                Ok(text) => {
                    println!("{text}");
                    ExitCode::SUCCESS
                }
```

Change to:

```rust
                Ok(outcome) => {
                    println!("{}", outcome.text);
                    ExitCode::SUCCESS
                }
```

- [ ] **Step 3: Pass `provider_name`/`model_name` into `RunArgs`**

Find the `polaris_tui::RunArgs { ... }` construction in the `None => { ... }` arm (the TUI branch) and add the two new fields:

```rust
            polaris_tui::run(polaris_tui::RunArgs {
                provider: provider.as_ref(),
                provider_name: provider_name.clone(),
                model_name: model_name.clone(),
                state_dir,
                audit_path,
                max_turns: args.max_turns,
                sandbox,
                helper,
                approval_policy,
                always_on: &always_on,
                skills: &discovered.skills,
            })
            .await
```

- [ ] **Step 4: Add the two fields to `RunArgs` and track cumulative usage in `run()`**

In `crates/polaris-tui/src/lib.rs`, add to `RunArgs`:

```rust
pub struct RunArgs<'a> {
    pub provider: &'a dyn Provider,
    pub provider_name: String,
    pub model_name: String,
    pub state_dir: PathBuf,
    pub audit_path: PathBuf,
    pub max_turns: u32,
    pub sandbox: SandboxPolicy,
    pub helper: PathBuf,
    pub approval_policy: ApprovalPolicy,
    pub always_on: &'a AlwaysOn,
    pub skills: &'a [Skill],
}
```

Add a running usage total before the main loop (near `let mut input_buffer = String::new();`):

```rust
    let mut cumulative_usage = polaris_provider::Usage::default();
```

In the `match agent::run(...).await { Ok(_) => { ... } Err(e) => { ... } }` block, change the `Ok(_)` arm to bind the outcome and accumulate:

```rust
            Ok(outcome) => {
                cumulative_usage.input_tokens += outcome.usage.input_tokens;
                cumulative_usage.output_tokens += outcome.usage.output_tokens;
                cumulative_usage.total_tokens += outcome.usage.total_tokens;
                status = Status::Idle;
                if let Some(reply) = session.messages.last()
                    && let Err(e) = persist::append_message(&session_path, reply)
                {
                    fatal_message = Some(format!("Can't persist the reply: {e}"));
                    break 'outer ExitCode::FAILURE;
                }
            }
```

Both `terminal.draw(|f| render_chat(f, &session, &input_buffer, &status))` calls in this file need to pass the new header info through to `render_chat` once Task 5 changes `render_chat`'s signature — leave both call sites as-is for now (Task 5 updates them together with the signature change, so a partial update here would not compile against the still-old `render_chat` and vice versa). Do not modify the `terminal.draw(...)` call sites in this task.

- [ ] **Step 5: Update the one-shot CLI integration tests if needed**

Run: `cargo build --workspace 2>&1 | grep "error" | head -40`

If `crates/polaris-cli/tests/cli.rs` or `crates/polaris-cli/tests/subcommands.rs` assert on stdout text that depended on the old `Ok(text) => println!("{text}")` path, they should be unaffected (the printed text is identical — only the internal binding name changed from `text` to `outcome.text`). Confirm this by running the CLI test suite; fix only if a real compile error or behavioral change surfaces (there should not be one).

- [ ] **Step 6: Run the full workspace build and test suite**

Run: `cargo build --workspace && cargo test --workspace`
Expected: builds clean. Tests will not all pass yet if Task 5 hasn't landed (this task alone doesn't change `render_chat`'s signature, so nothing here should actually break `polaris-tui`'s tests — confirm `cargo test -p polaris-tui` and `cargo test -p polaris-cli` are both green before moving on).

- [ ] **Step 7: Regenerate the filemap and commit**

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add crates/polaris-cli/src/main.rs crates/polaris-tui/src/lib.rs docs/filemap.md
git commit -m "feat: thread provider/model name and cumulative usage through both CLI paths"
```

---

### Task 5: Status bar and tool-call visualization

**Files:**
- Modify: `crates/polaris-tui/src/render.rs`
- Modify: `crates/polaris-tui/src/lib.rs`

**Interfaces:**
- Consumes: `RunArgs.provider_name`/`model_name` (Task 4), `cumulative_usage: polaris_provider::Usage` (Task 4, tracked in `run()`).
- Produces: `render_chat`'s signature changes to take a new `header: &HeaderInfo` parameter (`pub struct HeaderInfo<'a> { pub provider_name: &'a str, pub model_name: &'a str, pub usage: polaris_provider::Usage }`, `pub`, new in `render.rs`). `history_lines` no longer filters out `Role::Tool`.

- [ ] **Step 1: Write the failing tests**

Add to `render.rs`'s `mod tests`:

```rust
    #[test]
    fn the_header_shows_provider_model_and_usage() {
        let session = Session::default();
        let header = HeaderInfo {
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage {
                input_tokens: 100,
                output_tokens: 40,
                total_tokens: 140,
            },
        };

        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
            .expect("draw");

        let content = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(content.contains("openai"));
        assert!(content.contains("gpt-5.4"));
        assert!(content.contains("140"));
    }

    #[test]
    fn a_tool_call_and_its_result_are_shown_in_the_history() {
        let mut session = Session::default();
        session.push_user("what's in a.txt?");
        session.push_assistant_tool_calls(
            "",
            vec![polaris_provider::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "a.txt"}),
            }],
        );
        session.push_tool_result("c1", "hello\n");
        session.push_assistant("a.txt contains \"hello\"");

        let header = HeaderInfo {
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 15);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
            .expect("draw");

        let content = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(content.contains("read"), "the tool name should appear: {content}");
        assert!(content.contains("hello"), "the tool result should appear: {content}");
    }

    #[test]
    fn a_long_tool_result_is_truncated_in_the_display() {
        let mut session = Session::default();
        session.push_assistant_tool_calls(
            "",
            vec![polaris_provider::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "big.txt"}),
            }],
        );
        let long_body: String = "x".repeat(500);
        session.push_tool_result("c1", &long_body);

        let header = HeaderInfo {
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 15);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
            .expect("draw");

        let content = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>();
        // The full 500-character body must not appear verbatim; only a
        // prefix of it should.
        assert!(!content.contains(&long_body));
    }
```

Adjust the exact `TestBackend` height in each test upward if a test fails only because the terminal is too short to show both the new header row and the content under test (the header adds one row of vertical budget consumed away from the history pane) — this is a mechanical sizing fix, not a design change.

- [ ] **Step 2: Run to see the tests fail**

Run: `cargo test -p polaris-tui`
Expected: FAIL to compile — `HeaderInfo` doesn't exist yet, `render_chat` doesn't take a 5th argument yet.

- [ ] **Step 3: Add `HeaderInfo` and the header row**

In `render.rs`, add:

```rust
/// What the status-bar header shows: which provider/model is in use, and
/// the token usage accumulated so far this session. This is a read-only
/// snapshot handed in by the caller each frame — `render.rs` never tracks
/// state itself.
pub struct HeaderInfo<'a> {
    pub provider_name: &'a str,
    pub model_name: &'a str,
    pub usage: polaris_provider::Usage,
}
```

Change `render_chat`'s signature and layout:

```rust
pub fn render_chat(
    frame: &mut Frame,
    session: &Session,
    input: &str,
    status: &Status,
    header: &HeaderInfo,
) {
    let area = frame.area();
    let [header_area, history_area, status_area, input_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .areas(area);

    let header_text = format!(
        "{} / {} — tokens: in {} / out {} / total {}",
        sanitize(header.provider_name),
        sanitize(header.model_name),
        header.usage.input_tokens,
        header.usage.output_tokens,
        header.usage.total_tokens,
    );
    frame.render_widget(Paragraph::new(header_text), header_area);

    let lines = history_lines(session);
    // ... (rest of the function body is unchanged from before this task —
    // the scroll-offset math, the status-line rendering, and the input-box
    // rendering all stay exactly as they are today)
```

Everything from `let lines = history_lines(session);` onward in the existing function body stays byte-for-byte the same — only the new `header_area`/`header_text` block above it, and the destructured `[header_area, history_area, status_area, input_area]` (one more element than before) are new.

- [ ] **Step 4: Render tool calls and results**

Change `history_lines` to stop excluding `Role::Tool`, and to format tool calls/results specially:

```rust
const TOOL_RESULT_PREVIEW_CHARS: usize = 200;

fn format_tool_calls(calls: &[polaris_provider::ToolCall]) -> Vec<String> {
    calls
        .iter()
        .map(|c| {
            let args = serde_json::to_string(&c.arguments).unwrap_or_default();
            let args_preview: String = args.chars().take(TOOL_RESULT_PREVIEW_CHARS).collect();
            format!("⚙ {}({})", c.name, args_preview)
        })
        .collect()
}

fn format_tool_result(content: &str) -> String {
    let preview: String = content.chars().take(TOOL_RESULT_PREVIEW_CHARS).collect();
    if content.chars().count() > TOOL_RESULT_PREVIEW_CHARS {
        format!("→ {preview}...")
    } else {
        format!("→ {preview}")
    }
}

fn history_lines(session: &Session) -> Vec<Line<'static>> {
    session
        .messages
        .iter()
        .flat_map(|m| -> Vec<Line<'static>> {
            match m.role {
                Role::Tool => vec![Line::from(sanitize(&format_tool_result(&m.content)))],
                Role::User | Role::Assistant => {
                    let mut lines = Vec::new();
                    if !m.tool_calls.is_empty() {
                        for call_line in format_tool_calls(&m.tool_calls) {
                            lines.push(Line::from(sanitize(&call_line)));
                        }
                    }
                    if !m.content.is_empty() {
                        let sanitized = sanitize(&m.content);
                        let prefix = format!("{}: ", label(m.role));
                        for (i, line) in sanitized.split('\n').enumerate() {
                            if i == 0 {
                                lines.push(Line::from(format!("{prefix}{line}")));
                            } else {
                                lines.push(Line::from(line.to_string()));
                            }
                        }
                    }
                    lines
                }
            }
        })
        .collect()
}
```

This replaces the entire previous `history_lines` function. Note the `m.content.is_empty()` guard: a tool-calling assistant turn's own `content` is often empty (`agent.rs` pushes `session.push_assistant_tool_calls(&res.text, ...)` where `res.text` can be `""` when the model only called a tool) — without this guard, such a turn would render a bare `"polaris: "` line with nothing after it. Skipping the empty case means only the `⚙` tool-call line(s) show for that turn, which is the actually useful signal.

- [ ] **Step 5: Update the two `terminal.draw` call sites in `lib.rs`**

In `crates/polaris-tui/src/lib.rs`, both `terminal.draw(|f| render_chat(f, &session, &input_buffer, &status))` call sites need the new 5th argument. Build a `HeaderInfo` from `args.provider_name`, `args.model_name`, and the running `cumulative_usage` (from Task 4 Step 4) at each call site:

```rust
        if terminal
            .draw(|f| {
                render_chat(
                    f,
                    &session,
                    &input_buffer,
                    &status,
                    &render::HeaderInfo {
                        provider_name: &args.provider_name,
                        model_name: &args.model_name,
                        usage: cumulative_usage,
                    },
                )
            })
            .is_err()
        {
            break ExitCode::FAILURE;
        }
```

Apply the same change to both occurrences of this pattern in the file (there are two: the top-of-loop draw and the post-submit "thinking" draw).

- [ ] **Step 6: Run the full workspace build and test suite**

Run: `cargo build --workspace && cargo test --workspace`
Expected: builds clean, all tests pass — including the 3 new tests from Step 1 and every pre-existing `render.rs` test (which will need their own `render_chat` calls updated with a `&HeaderInfo` argument; go through `render.rs`'s existing tests from Task-1-era work and add a minimal `HeaderInfo` literal to each `render_chat(...)` call that doesn't already have one, matching the pattern the new tests use).

- [ ] **Step 7: Regenerate the filemap and commit**

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add crates/polaris-tui/src/render.rs crates/polaris-tui/src/lib.rs docs/filemap.md
git commit -m "feat(polaris-tui): add a status bar and inline tool-call visualization"
```

---

### Task 6: Minimal markdown formatting and role colors

**Files:**
- Modify: `crates/polaris-tui/src/render.rs`

**Interfaces:**
- Consumes: nothing new externally — this task only changes how `history_lines`' already-sanitized text becomes `Span`s within each `Line`.
- Produces: `render.rs`'s history lines carry styled `Span`s (bold, inline-code background, fenced-code-block background, per-role color) instead of plain unstyled text.

- [ ] **Step 1: Write the failing tests**

Add to `render.rs`'s `mod tests`:

```rust
    #[test]
    fn bold_text_is_rendered_with_the_bold_modifier() {
        let mut session = Session::default();
        session.push_assistant("this is **bold** text");

        let header = HeaderInfo {
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let bold_cell = (0..buffer.area.width)
            .flat_map(|x| (0..buffer.area.height).map(move |y| (x, y)))
            .find(|&(x, y)| buffer[(x, y)].symbol() == "b" && {
                let row: String = (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect();
                row.contains("bold")
            });
        let (x, y) = bold_cell.expect("the word 'bold' should appear somewhere");
        assert!(
            buffer[(x, y)].modifier.contains(ratatui::style::Modifier::BOLD),
            "the 'b' in 'bold' should carry the BOLD modifier"
        );
    }

    #[test]
    fn inline_code_and_surrounding_text_both_render_without_the_backticks() {
        let mut session = Session::default();
        session.push_assistant("run `cargo test` now");

        let header = HeaderInfo {
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
            .expect("draw");

        let content = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>();
        assert!(content.contains("cargo test"));
        assert!(!content.contains('`'));
    }

    #[test]
    fn user_and_assistant_lines_use_different_colors() {
        let mut session = Session::default();
        session.push_user("hello");
        session.push_assistant("hi there");

        let header = HeaderInfo {
            provider_name: "openai",
            model_name: "gpt-5.4",
            usage: polaris_provider::Usage::default(),
        };
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render_chat(f, &session, "", &Status::Idle, &header))
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let find_row_color = |needle: &str| {
            for y in 0..buffer.area.height {
                let row: String = (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect();
                if row.contains(needle) {
                    return buffer[(0, y)].fg;
                }
            }
            panic!("row containing {needle:?} not found");
        };
        assert_ne!(find_row_color("hello"), find_row_color("hi there"));
    }
```

- [ ] **Step 2: Run to see the tests fail**

Run: `cargo test -p polaris-tui`
Expected: FAIL — bold/backtick tests fail because there's currently no inline formatting at all (plain `Line::from(String)`); the color test fails because every line currently uses the terminal's default foreground color.

- [ ] **Step 3: Implement the inline formatter**

Add a small scanner that turns one already-sanitized line of text into a `Vec<Span<'static>>`, recognizing `**bold**` and `` `code` `` (fenced code blocks are handled at the block level in Step 4, not here):

```rust
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

fn role_color(role: Role) -> Color {
    match role {
        Role::User => Color::Cyan,
        Role::Assistant => Color::Green,
        Role::Tool => Color::Yellow,
    }
}

/// Turns one line of already-sanitized text into styled spans, recognizing
/// `**bold**` and `` `inline code` ``. Malformed markdown (an unclosed `**`
/// or `` ` ``) is not an error — whatever's left over after the last
/// successfully matched marker is emitted as plain text, so a stray
/// backtick never breaks rendering.
fn format_inline(text: &str, base_color: Color) -> Vec<Span<'static>> {
    let base_style = Style::default().fg(base_color);
    let mut spans = Vec::new();
    let mut rest = text;

    loop {
        // Find whichever marker comes first: **bold** or `code`.
        let bold_pos = rest.find("**");
        let code_pos = rest.find('`');

        match (bold_pos, code_pos) {
            (None, None) => {
                if !rest.is_empty() {
                    spans.push(Span::styled(rest.to_string(), base_style));
                }
                break;
            }
            (bold, code) if bold.is_some() && (code.is_none() || bold.unwrap() <= code.unwrap()) => {
                let start = bold.unwrap();
                if let Some(end) = rest[start + 2..].find("**") {
                    let end = start + 2 + end;
                    if start > 0 {
                        spans.push(Span::styled(rest[..start].to_string(), base_style));
                    }
                    spans.push(Span::styled(
                        rest[start + 2..end].to_string(),
                        base_style.add_modifier(Modifier::BOLD),
                    ));
                    rest = &rest[end + 2..];
                } else {
                    // Unclosed `**`: emit the rest as plain text.
                    spans.push(Span::styled(rest.to_string(), base_style));
                    break;
                }
            }
            (_, Some(start)) => {
                if let Some(end) = rest[start + 1..].find('`') {
                    let end = start + 1 + end;
                    if start > 0 {
                        spans.push(Span::styled(rest[..start].to_string(), base_style));
                    }
                    spans.push(Span::styled(
                        rest[start + 1..end].to_string(),
                        base_style.bg(Color::DarkGray),
                    ));
                    rest = &rest[end + 1..];
                } else {
                    // Unclosed backtick: emit the rest as plain text.
                    spans.push(Span::styled(rest.to_string(), base_style));
                    break;
                }
            }
        }
    }

    spans
}
```

- [ ] **Step 4: Wire the formatter into `history_lines` and handle fenced code blocks**

Change `history_lines` so every produced `Line` carries styled spans instead of plain strings, and so a fenced ` ``` ` block gets a background color applied to its whole lines rather than inline-formatted:

```rust
fn history_lines(session: &Session) -> Vec<Line<'static>> {
    session
        .messages
        .iter()
        .flat_map(|m| -> Vec<Line<'static>> {
            let color = role_color(m.role);
            match m.role {
                Role::Tool => vec![Line::from(Span::styled(
                    sanitize(&format_tool_result(&m.content)),
                    Style::default().fg(color),
                ))],
                Role::User | Role::Assistant => {
                    let mut lines = Vec::new();
                    if !m.tool_calls.is_empty() {
                        for call_line in format_tool_calls(&m.tool_calls) {
                            lines.push(Line::from(Span::styled(
                                sanitize(&call_line),
                                Style::default().fg(color),
                            )));
                        }
                    }
                    if !m.content.is_empty() {
                        let sanitized = sanitize(&m.content);
                        let prefix = format!("{}: ", label(m.role));
                        let mut in_code_block = false;
                        for (i, raw_line) in sanitized.split('\n').enumerate() {
                            if raw_line.trim_start().starts_with("```") {
                                in_code_block = !in_code_block;
                                lines.push(Line::from(Span::styled(
                                    String::new(),
                                    Style::default().bg(Color::DarkGray),
                                )));
                                continue;
                            }
                            let text = if i == 0 {
                                format!("{prefix}{raw_line}")
                            } else {
                                raw_line.to_string()
                            };
                            if in_code_block {
                                lines.push(Line::from(Span::styled(
                                    text,
                                    Style::default().fg(color).bg(Color::DarkGray),
                                )));
                            } else {
                                lines.push(Line::from(format_inline(&text, color)));
                            }
                        }
                    }
                    lines
                }
            }
        })
        .collect()
}
```

- [ ] **Step 5: Run the full workspace build and test suite**

Run: `cargo build --workspace && cargo test --workspace`
Expected: builds clean, all tests pass, including the 3 new tests from Step 1 and every pre-existing `render.rs` test (the multi-line and scroll-offset tests from earlier tasks assert on rendered *text content* via `.symbol()`, which is unaffected by adding color/bold styling — only cell-level style attributes change, not the characters themselves, so those tests should keep passing unmodified; if one fails, read why before changing the test, since a content-visibility regression here would be a real bug).

- [ ] **Step 6: Regenerate the filemap and commit**

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add crates/polaris-tui/src/render.rs docs/filemap.md
git commit -m "feat(polaris-tui): add role colors and minimal bold/code markdown formatting"
```

---

### Task 7: Manual verification and docs

**Files:**
- Modify: `README.md`

**Interfaces:**
- Consumes: nothing new — this task documents and manually exercises what Tasks 1-6 built.

Same constraint as v0.3.0's Task 6: the actual visual result (colors, bold rendering, status-bar layout) needs a real terminal to judge and can't be verified by an automated subagent. Follow the same pattern used there.

- [ ] **Step 1: Update the README's `## TUI` section**

In `README.md`, extend the existing `## TUI` section (added in v0.3.0) with a short paragraph describing the new elements, in the same terse Japanese style as the rest of that section:

```markdown
画面上部にはプロバイダー名・モデル名・累積トークン使用量を表示する常時ステータスバーがある。
ツール呼び出しは会話履歴内に `⚙ ツール名(引数)` として、その結果は `→ 要約` として表示される
(結果が長い場合は200文字程度で打ち切られる。打ち切られるのは表示だけで、モデルへ送る内容には
影響しない)。`**太字**` と `` `インラインコード` ``、フェンス付きコードブロックは整形して表示する。
```

Place it directly after the existing opening paragraph of the `## TUI` section (before the `会話は ... 保存され` paragraph), or wherever it reads most naturally alongside the existing content — your judgment on exact placement.

- [ ] **Step 2: Extend the manual-verification checklist**

In the same section's `### 手で確かめる` list, add these bullets to the existing list:

```markdown
- ステータスバーにプロバイダー名・モデル名・トークン使用量(累計)が表示され、ターンが進むごとに数値が増えること
- ツールを使う指示(例:「Cargo.tomlを読んで」)で、履歴に `⚙ read(...)` と `→ ...` が表示されること
- `**太字**` を含む応答が太字で表示され、`` `コード` `` がバックティック無しで区別可能な見た目になること
```

- [ ] **Step 3: Manually run the checks**

Run: `cargo build --release && POLARIS_API_KEY=sk-... ./target/release/polaris` (or `POLARIS_PROVIDER=codex ./target/release/polaris` after `polaris login`)

Work through every bullet in the updated README section, including the ones carried over from v0.3.0. If any interactive check fails, this task is not done — go back and fix the relevant Task 1-6 code before proceeding. As with v0.3.0, this step needs a real terminal and a real provider connection; if you're an automated agent without one, run the same non-interactive proxy check v0.3.0's Task 6 used (`cargo build --release`, then `./target/release/polaris < /dev/null` and confirm it fails gracefully with the non-interactive-terminal message rather than hanging) and report the full interactive walkthrough as deferred to a human, exactly as before — do not fabricate having done it.

- [ ] **Step 4: Run the full workspace build and test suite one more time**

Run: `cargo build --workspace && cargo test --workspace`
Expected: builds clean, all tests pass.

- [ ] **Step 5: Regenerate the filemap and commit**

Run: `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap`

```bash
git add README.md docs/filemap.md
git commit -m "docs: document the v0.4.0 status bar, tool visualization, and markdown formatting"
```
