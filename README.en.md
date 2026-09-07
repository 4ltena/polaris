# polaris

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/polaris-banner-white.svg">
    <img src="docs/assets/polaris-banner-ink.svg" alt="Polaris — asterisk and wordmark" width="800" height="200">
  </picture>
</p>

<p align="center">
  <strong>v0.10.0 “Algedi”</strong> · <a href="CHANGELOG.md">Changelog (Japanese)</a>
</p>

<p align="center">
  <a href="README.md" lang="ja">日本語</a> · <strong>English</strong>
</p>

Polaris is a Rust CLI/TUI coding agent that keeps always-on instructions and tool definitions small, retrieving additional material when needed. It supports conversation compaction, local archival of pre-compaction history, and storage and retrieval of large tool results.

## Getting started

Install Rust 1.96.0, pinned in `rust-toolchain.toml`, and build:

```sh
cargo build --release
POLARIS_API_KEY=sk-... target/release/polaris -p "How many lines are in Cargo.toml?"
```

During development, use `cargo run -p polaris-cli -- -p "..."`. Omit `--prompt` to start the interactive TUI.

To use a ChatGPT subscription, sign in first:

```sh
target/release/polaris login
POLARIS_PROVIDER=codex target/release/polaris -p "How many lines are in Cargo.toml?"
```

The default model is `gpt-6-astra` with `medium` reasoning effort. To select them explicitly, pass the model and effort separately:

```sh
POLARIS_PROVIDER=codex POLARIS_MODEL=gpt-6-astra target/release/polaris --effort medium -p "How many lines are in Cargo.toml?"
```

See the [usage guide](docs/usage.md) for providers, audit logs, sandboxing, the TUI, saved conversations, and subcommands. The detailed documentation linked below is currently in Japanese.

## Core behavior

- Always-on context is limited to 990 tokens with the reference tokenizer `o200k_base`, with at most six tools. Tests measure the serialized wire format.
- Skill catalogs are not expanded into every request. The `skill` tool searches them when needed. Conversation history, retrieved material, tool results, and loaded skill bodies are outside the fixed budget.
- `--tool-memory off|history|retrieval` controls retention of large tool results. It defaults to `off` and is independent of `--remember`, which archives pre-compaction history.
- `retrieval` searches the stored original by keyword and retrieves selected lines or paragraphs. Semantic search is enabled only when a local embedding URL and model are explicitly configured.
- `spawn` runs independent subagents in one wave and validates their output against each agent type's JSON Schema. Delegation depth is limited to one.

The fixed context budget is not a cap on total input: history, retrieved material, and tool results add to each request. Retained results are snapshots from the time they were obtained. Read the ordinary file path when current contents are needed.

See [context efficiency](docs/context-efficiency.md) for storage, retrieval URIs, compaction, and measurement, and [subagents](docs/subagents.md) for configuration and constraints.

## Changes in v0.10.0

- Added searches within a retained tool-result record, filemap guidance, and support for communication and authentication in isolated environments.
- Unified the default model and reasoning effort as `gpt-6-astra` and `medium`.
- Added opt-in cache experiments that preserve request content, plus quality checks covering long conversations, skill and tool counts, and example counts. `POLARIS_CACHE_PACING=on` spaces model request starts at least five seconds apart. It defaults to `off`.

In one completed 36-turn pair, total token usage was identical while the cache rate increased from 54.11% to 63.83% and hypothetical API cost fell by 15.38%. Another task's control run stopped on a quality failure, so the complete comparison and reproducibility remain unverified. See the [conditions, results, and limitations](docs/gpt6-cache-pacing-results.md).

## Documentation

The following detailed documents are in Japanese.

| Document | Contents |
| --- | --- |
| [Usage guide](docs/usage.md) | Building, authentication, audit logs, TUI, conversations, and slash commands. |
| [Context efficiency](docs/context-efficiency.md) | Compaction, archival, retrieval, embeddings, and measurement. |
| [Subagents](docs/subagents.md) | `spawn` configuration, agent types, output schemas, and execution limits. |
| [Testing](docs/testing.md) | Fixed-context checks and manual TUI verification. |
| [Large skill/plugin comparison](docs/skill-scaling-benchmark.md) | Earlier measurements of initial input and elapsed time. |
| [GPT-6 medium measurements](docs/gpt6-efficiency-results.md) | Synthetic tasks, comparison with Codex CLI, and measurement limits. |
| [Cache and quality checks](docs/gpt6-cache-affinity-results.md) | Transport changes preserving content, long conversations, and varying skill, tool, and example counts. |
| [Request pacing and caching](docs/gpt6-cache-pacing-results.md) | An experiment changing request spacing without changing request content. |

## Comparison with Codex CLI

At v0.9.0, Codex CLI and a Polaris candidate were each run three times with the same synthetic material, prompt, and GPT-6 medium model. Codex retained the user's configuration and skills from the measurement environment.

| Metric (three runs combined) | Codex CLI | Polaris | Reduction |
| --- | ---: | ---: | ---: |
| Total tokens | 612,027 | 38,620 | 93.7% |
| Elapsed time | 93.678 s | 72.096 s | 23.0% |
| Answer and scope checks | 3/3 passed | 3/3 passed | — |

Total tokens are input plus output; cached tokens are already part of input. This is not a cost comparison. The runs used one synthetic task at different times; these reductions do not establish performance for v0.10.0 as a whole or for general coding work. See the [details and limitations](docs/gpt6-efficiency-results.md).

In an earlier large skill/plugin comparison, Polaris used 547 input tokens on the first request. That figure was API-reported usage at the time, not a measurement of the current fixed context. See the [measurement conditions and tables](docs/skill-scaling-benchmark.md).

## Development and verification

```sh
cargo fmt --all -- --check
cargo test --locked --offline -p polaris-core budget
cargo test --locked --offline -p polaris-cli tool_memory_defaults_off_and_is_independent_of_remember
```

See [testing](docs/testing.md) for coverage, manual TUI checks, and environment-dependent limitations.

## License

Released under the [MIT License](LICENSE). See the [TLS fixture notes](crates/polaris-http/src/fixtures/README.md) for the test material's source and license.
