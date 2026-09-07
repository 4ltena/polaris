# polaris

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/polaris-banner-white.svg">
    <img src="docs/assets/polaris-banner-ink.svg" alt="Polaris — asterisk and wordmark" width="800" height="200">
  </picture>
</p>

<p align="center">
  <strong>v0.11.0 “Sadalmelik”</strong> · <a href="CHANGELOG.md">Changelog (Japanese)</a>
</p>

<p align="center">
  <a href="README.md" lang="ja">日本語</a> · <strong>English</strong>
</p>

Polaris is a Rust CLI/TUI agent that sends the model the instructions and material needed for the current task. Its built-in workflow records the working phase and loads shared rules and phase-required skills separately from ordinary skill search.

## Comparison with stock Codex

Eight synthetic task types cover planning, requirements, data design, implementation planning, and plan review. Both tools requested `gpt-6-astra` with `medium` effort. The table totals 16 matched trials with two turns each; Polaris had workflow enabled.

| Metric | Polaris | Stock Codex CLI 0.153.4 | Reduction |
| --- | ---: | ---: | ---: |
| Total tokens | **99,530** | 493,518 | **79.8%** |
| Hypothetical API cost | **$2.46658** | $4.16582 | **40.8%** |
| Quality checks | 16/16 passed | 16/16 passed | — |

Total tokens include input and output; cached input is already part of input. Cost uses API prices frozen in the measurement specification, not actual Codex billing. These short synthetic tasks were measured at different times and do not establish reductions for general coding work or long conversations. See the [method, quality checks, timing, and limitations](docs/preimplementation-evaluation.md).

## Getting started

Install Rust 1.96.0, build, and authenticate with a ChatGPT subscription.

```sh
cargo build --release
target/release/polaris login
POLARIS_PROVIDER=codex target/release/polaris -p "How many lines are in Cargo.toml?"
```

Omit `-p` to start the interactive TUI. During development, use `cargo run -p polaris-cli -- -p "..."`. The default model is `gpt-6-astra`, with `medium` reasoning effort.

Use `--phase` to choose the starting phase:

```sh
POLARIS_PROVIDER=codex target/release/polaris --phase specify -p "Define acceptance criteria for an equipment lending service"
```

The API-key `openai` provider currently uses Chat Completions and does not support Astra tool calling, which requires the Responses API. See the [usage guide](docs/usage.md) for authentication, configuration, and resuming conversations.

## Core features

- **Phase-aware workflow:** Shared and phase-required skills are fixed for each turn. Ordinary skill search remains available on demand without expanding the entire catalog into every request.
- **Small base context:** Base instructions and at most six tool definitions stay within 990 tokens under `o200k_base`. Workflow skills and identifying wrappers have a separate 384-token budget. History, retrieved material, and tool results are outside this fixed budget.
- **Retention and retrieval:** Large tool results can be stored and retrieved by selected lines or paragraphs. Enable this with `--tool-memory history|retrieval`.
- **Conversation continuity:** Saved conversations preserve workflow state across resume and fork. Subagents run in one parallel wave with output validated against per-type schemas.

v0.11.0 “Sadalmelik” adopts workflow by default and adds durable conversation state, resume, and fork. The recent-ten-turn `strict10` mode with summary retrieval and cache request pacing remain opt-in. Real-model evaluation of strict10 is incomplete, and Web search cannot be enabled in the current CLI; see the [validation report](docs/sadalmelik-validation.md) for requirements and limits, and the [changelog](CHANGELOG.md) for release history.

## Documentation

The detailed documents below are in Japanese.

| Document | Contents |
| --- | --- |
| [Usage guide](docs/usage.md) | Authentication, configuration, workflow, TUI, resume, and fork. |
| [Context efficiency](docs/context-efficiency.md) | Compaction, archival, retrieval, embeddings, and strict10. |
| [Subagents](docs/subagents.md) | Parallel execution, schemas, and execution limits. |
| [Preimplementation measurements](docs/preimplementation-evaluation.md) | Planning quality and the stock Codex comparison. |
| [v0.11.0 validation](docs/sadalmelik-validation.md) | Control tests, real-model tests, and unverified behavior. |
| [Testing](docs/testing.md) | Automated checks and manual TUI verification. |
| [Earlier GPT-6 measurements](docs/gpt6-efficiency-results.md) | Synthetic tasks and measurement conditions at v0.9.0. |
| [Large skill/plugin comparison](docs/skill-scaling-benchmark.md) | Conditions behind the earlier 547-token initial input. |
| [Request pacing and caching](docs/gpt6-cache-pacing-results.md) | The v0.10.0 long-conversation cache experiment. |
| [Codex update investigation](docs/codex-update-efficiency.md) | Public changes and control of automatic model requests. |

## Development and verification

```sh
cargo test --locked --offline --workspace
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## License

Released under the [MIT License](LICENSE). See the [TLS fixture notes](crates/polaris-http/src/fixtures/README.md) for the test material's source and license.
