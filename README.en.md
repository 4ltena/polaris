# polaris

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/polaris-banner-white.svg">
    <img src="docs/assets/polaris-banner-ink.svg" alt="Polaris — asterisk and wordmark" width="800" height="200">
  </picture>
</p>

<p align="center">
  <strong>v1.0.0 in development · not yet distributed</strong> · <a href="CHANGELOG.md">Changelog (Japanese)</a>
</p>

<p align="center">
  <a href="README.md" lang="ja">日本語</a> · <strong>English</strong>
</p>

Polaris is a Rust coding agent that selects the context needed for the current task. Its main interfaces are a CLI and TUI, with a separate macOS workspace app. It stores workflow state and assembles instructions and skills for the active phase.

This checkout contains the in-progress v1.0.0 implementation. The TUI connects ordinary conversations, automatic Sidechat routing, MCP, subagents, and conversation/workspace rewind. Some management operations, recovery when MCP prompts and Sidechat routing interact, and real-terminal acceptance remain unfinished. No v1.0.0 distribution has been published. See the [usage guide](docs/usage.md) and [macOS build guide](apps/macos/README.md).

## v0.12.0 comparison with stock Codex

Both tools requested `gpt-6-astra` with `medium` effort; Polaris had workflow enabled. These partial results cover 15 matched PRE02–08 trials with two turns each against Codex CLI 0.154.0. The additional PRE01 measurement was skipped.

| Metric | Polaris | Stock Codex CLI 0.154.0 | Reduction |
| --- | ---: | ---: | ---: |
| Total tokens | **95,139** | 467,901 | **79.7%** |
| Hypothetical API cost | **$2.37915** | $3.600914 | **33.9%** |
| Quality checks | 15/15 passed | 15/15 passed | — |

Total tokens include input and output; cached input is already part of input. Cost uses API prices frozen in the measurement specification, not actual Codex billing. Polaris took 23.5% longer across these 15 trials. These short synthetic tasks were run sequentially and do not establish reductions for general coding work or long conversations. See the [method, quality checks, timing, and limitations](docs/preimplementation-evaluation.md#v0120とcodex-01540の途中結果).

## Using this development checkout

On macOS arm64, a local preview package can be assembled with a compiled Polaris CLI. Once obtained, install it with one command, `sudo installer -pkg polaris-cli-preview-0.12.0-arm64.pkg -target /`, then launch the TUI with `polaris`. Rust is not needed on the user's machine. This package is currently unsigned and unpublished; a real-machine installation check is still pending. See the [macOS guide](apps/macos/README.md) for assembly and verification details.

Building from source requires Rust 1.96.0. To use ChatGPT authentication:

```sh
cargo build --release -p polaris-cli
target/release/polaris login
POLARIS_PROVIDER=codex target/release/polaris
```

With no prompt argument, Polaris starts the interactive TUI; `-p` runs one prompt. The default provider is `openai` and requires an API key, so set `POLARIS_PROVIDER=codex` for ChatGPT authentication as shown. The `polaris` command on the developer's machine is a local source-building wrapper, not a distributed installer.

Use `--phase` to choose the starting phase:

```sh
POLARIS_PROVIDER=codex target/release/polaris --phase specify -p "Define acceptance criteria for an equipment lending service"
```

The API-key `openai` provider currently uses Chat Completions and does not support Astra tool calling, which requires the Responses API. See the [usage guide](docs/usage.md) for authentication, configuration, and resuming conversations.

## Core features

- **Stateful workflow:** Persist the phase and active task, then assemble the instructions and skills needed for each turn.
- **Conversation continuity:** Separate unrelated work into Sidechats; resume, fork, or rewind a conversation and its workspace files from the TUI.
- **Tools and delegation:** Use files, Git, MCP, and subagents through a shared execution core with connection and permission checks.
- **Memory and retrieval:** Preserve source material while retrieving older relevant context and large tool results when needed.
- **Selectable typed decision backends:** Retain Jev and SemIf while adding fixed Clef/Clef-flash profiles and explicit System One compatible connections under one decision contract. Disabled by default. Capacity tiers are recommendations and never change the selected backend automatically. No Clef profile has production acceptance yet, so normal Clef execution is not available. See [configuration and current limits](docs/usage.md#型付き判断器の設定) (Japanese).

The macOS CLI preview package contains no optional resources. The GUI installer has a manifest-verified download path for llama.cpp, the embedding runtime, and multilingual-e5-small/base under `~/.polaris`; Rust is optional. Qwen3.5-4B is a candidate for the SemIf classifier, but its pinned distribution artifacts and acceptance checks are incomplete, so it is not automatically downloaded. Signed distribution assets and real-device acceptance are still missing. See the [macOS guide](apps/macos/README.md) for the verified scope.

## Documentation

The detailed documents below are in Japanese.

| Document | Contents |
| --- | --- |
| [Usage guide](docs/usage.md) | Authentication, configuration, workflow, TUI, resume, and fork. |
| [macOS desktop](apps/macos/README.md) | Local app and installer assembly, storage boundaries, and local models. |
| [Architecture](docs/architecture.md) | Crate dependencies and external integration points. |
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
