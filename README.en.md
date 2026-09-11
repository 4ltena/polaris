# polaris

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/polaris-banner-white.svg">
    <img src="docs/assets/polaris-banner-ink.svg" alt="Polaris — asterisk and wordmark" width="800" height="200">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/4ltena/polaris/releases/tag/v0.12.0"><strong>v0.12.0 “Alrescha”</strong></a> · <a href="CHANGELOG.md">Changelog (Japanese)</a>
</p>

<p align="center">
  <a href="README.md" lang="ja">日本語</a> · <strong>English</strong>
</p>

Polaris is a Rust coding agent with a macOS workspace and CLI/TUI. It sends the model the instructions and material needed for the current task, combining a workflow that tracks the working phase with isolated execution, saved conversations, and retrieval.

v0.12.0 “Alrescha” is available as a [macOS DMG and source release](https://github.com/4ltena/polaris/releases/tag/v0.12.0). It integrates model execution, files, Git, task state, local models, recovery, and `strict10` conversation memory into the macOS desktop.

## Comparison with stock Codex

Both tools requested `gpt-6-astra` with `medium` effort in the September 10, 2026 measurement; Polaris had workflow enabled. These partial results cover 15 matched PRE02–08 trials with two turns each against Codex CLI 0.154.0. The additional PRE01 measurement was skipped.

| Metric | Polaris | Stock Codex CLI 0.154.0 | Polaris change |
| --- | ---: | ---: | ---: |
| Total tokens | **95,139** | 467,901 | **79.7% lower** |
| Hypothetical API cost | **$2.37915** | $3.600914 | **33.9% lower** |
| Total turn time | 1,179.26 s | 954.94 s | 23.5% higher |
| Quality checks | 15/15 passed | 15/15 passed | — |

Total tokens include input and output; cached input is already part of input. Cost uses API prices frozen in the measurement specification, not actual Codex billing. These short synthetic tasks were run sequentially and do not establish reductions for general coding work or long conversations. See the [method, quality checks, and limitations](docs/preimplementation-evaluation.md#v0120とcodex-01540の途中結果).

## Getting started

### macOS app

| Download | Architecture | Requirements |
| --- | --- | --- |
| [polaris-0.12.0-arm64.dmg](https://github.com/4ltena/polaris/releases/download/v0.12.0/polaris-0.12.0-arm64.dmg) | Apple Silicon (arm64) | macOS 13.5 or later |
| [SHA-256 checksums](https://github.com/4ltena/polaris/releases/download/v0.12.0/polaris-0.12.0-SHA256SUMS.txt) | — | Verify the downloaded DMG |

Open the DMG and drag Polaris from space at the top into Applications on Earth below. Once copying finishes, launch Polaris from Applications and follow the six-page setup. Do not launch it directly from the DMG.

The app has no Developer ID signature or notarization. If macOS cannot verify the developer, see [Apple's guidance](https://support.apple.com/guide/mac-help/open-a-mac-app-from-an-unknown-developer-mh40616/mac). The package includes the app, execution helpers, and Node.js 24.11.1. npm, other language toolchains, and `strict10` embedding resources require separate setup. See the [package details](apps/macos/README.md#配布パッケージ).

### CLI from source

Install Rust 1.96.0 and Git. Check out the v0.12.0 source, build the CLI, and authenticate with a ChatGPT subscription.

```sh
git clone --branch v0.12.0 --depth 1 https://github.com/4ltena/polaris.git
cd polaris
cargo build --locked --release -p polaris-cli
target/release/polaris login
POLARIS_PROVIDER=codex target/release/polaris -p "How many lines are in Cargo.toml?"
```

Omit `-p` to start the interactive TUI. During development, use `cargo run -p polaris-cli -- -p "..."`. The default model is `gpt-6-astra`, with `medium` reasoning effort.

Use `--phase` to choose the starting phase:

```sh
POLARIS_PROVIDER=codex target/release/polaris --phase specify -p "Define acceptance criteria for an equipment lending service"
```

The API-key `openai` provider currently uses Chat Completions and does not support Astra tool calling, which requires the Responses API. See the [usage guide](docs/usage.md) for authentication, configuration, and resuming conversations.

Building the desktop workspace from source requires macOS 13 or later and Swift 6. See the [macOS build guide](apps/macos/README.md#組立て) for instructions.

## Core features

- **macOS workspace:** View conversations, files, Git, and execution state; attach verified text; configure models and permissions; and connect to Ollama or LM Studio. Model edits stay in an isolated workspace until source-file changes are separately approved.
- **Phase-aware workflow:** Shared and phase-required skills are fixed for each turn. Ordinary skill search remains available on demand without expanding the entire catalog into every request.
- **Small base context:** Base instructions and at most six tool definitions stay within 990 tokens under `o200k_base`. Workflow skills and identifying wrappers have a separate 384-token budget. History, retrieved material, and tool results are outside this fixed budget.
- **Retention and retrieval:** Large tool results can be stored and retrieved by selected lines or paragraphs. Enable this with `--tool-memory history|retrieval`.
- **Conversation continuity:** Saved conversations preserve workflow state across resume and fork. Subagents run in one parallel wave with output validated against per-type schemas.
- **strict10 memory:** Select it in the GUI history settings. All original messages remain stored; the latest ten turns are included directly, while older context is retrieved through summaries and local embeddings. Pinned embedding resources require separate configuration.

## Validation status

The v0.12.0 source passed 1,689 Rust tests, 236 Swift tests, and six packaging tests, along with Clippy, formatting, and filemap checks. The macOS app was also built and assembled without local user settings or an existing Swift build cache.

Distribution checks passed all seven packaging tests, including release configuration, and verified the DMG integrity, 533 app file hashes, ownership and permissions after extraction, and the vertical Finder layout. No additional real-model request was sent from the distribution app.

The `strict10` retrieval defect is fixed, with automated regression checks and history restoration verified. The additional real-model check after that fix and the PRE01 comparison rerun were skipped. Speech input has synthetic-test coverage only; Windows and Linux device checks were outside scope. Web search cannot be enabled in the current CLI. See the [macOS checks](apps/macos/README.md#検査) and [changelog](CHANGELOG.md) for details.

## Documentation

The detailed documents below are in Japanese.

| Document | Contents |
| --- | --- |
| [Usage guide](docs/usage.md) | Authentication, configuration, workflow, TUI, resume, and fork. |
| [macOS desktop](apps/macos/README.md) | App assembly, storage boundaries, local models, and strict10. |
| [Context efficiency](docs/context-efficiency.md) | Compaction, archival, retrieval, embeddings, and strict10. |
| [Subagents](docs/subagents.md) | Parallel execution, schemas, and execution limits. |
| [Preimplementation measurements](docs/preimplementation-evaluation.md) | Planning quality and the stock Codex comparison. |
| [Testing](docs/testing.md) | Automated checks and manual TUI verification. |

The [changelog](CHANGELOG.md) links to validation and measurements for earlier versions.

## Development and verification

```sh
cargo test --locked --offline --workspace
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## License

Released under the [MIT License](LICENSE). See the [TLS fixture notes](crates/polaris-http/src/fixtures/README.md) for the test material's source and license.
