# polaris

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/polaris-banner-white.svg">
    <img src="docs/assets/polaris-banner-ink.svg" alt="Polaris — asterisk and wordmark" width="800" height="200">
  </picture>
</p>

<p align="center">
  <strong>v1.0.0-rc · macOS TUI only · package not yet published</strong> · <a href="CHANGELOG.md">Changelog (Japanese)</a>
</p>

<p align="center">
  <a href="README.md" lang="ja">日本語</a> · <strong>English</strong>
</p>

Polaris is a Rust coding agent that selects the context needed for the current task. Its main interfaces are a CLI and TUI, with a separate macOS workspace app. It stores workflow state and assembles instructions and skills for the active phase.

v1.0.0-rc targets the macOS arm64 TUI. GUI and other-platform sources remain in the repository. See the [system overview](docs/system-overview.md), [release scope](docs/releases/v1.0.0.md), and [command reference](docs/command-reference.md) (Japanese). No RC tag, release, or package has been published.

## Using this development checkout

Start from a project directory. Starting from your home directory currently fails helper preparation with `RuntimeDenied`; see [startup troubleshooting](docs/usage.md#起動時のトラブル).

On macOS arm64, a local preview package can be assembled with a compiled Polaris CLI. Once obtained, install it with one command, `sudo installer -pkg polaris-cli-preview-1.0.0-rc-arm64.pkg -target /`, then launch the TUI with `polaris`. Rust is not needed on the user's machine. This package is currently unsigned and unpublished; a real-machine installation check is still pending. See the [macOS guide](apps/macos/README.md) for assembly and verification details.

Building from source requires Rust 1.96.0. To use ChatGPT authentication:

```sh
cargo build --release -p polaris-cli
target/release/polaris login
POLARIS_PROVIDER=codex target/release/polaris
```

With no prompt argument, Polaris starts the interactive TUI; `-p` runs one prompt. Without an explicit provider, an available OpenAI-compatible API key selects `openai`; saved ChatGPT authentication selects `codex` when no API key is available. With neither, interactive startup shows the login screen. The example explicitly selects `codex`. Use `target/release/polaris` when testing from source to distinguish it from another installed version.

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

## v0.12.0 comparison with stock Codex

These are historical v0.12.0 measurements, not results for v1.0.0-rc. The current-version comparison has not been run.

Both tools requested `gpt-6-astra` with `medium` effort; Polaris had workflow enabled. These partial results cover 15 matched PRE02–08 trials with two turns each against Codex CLI 0.154.0. The additional PRE01 measurement was skipped.

| Metric | Polaris | Stock Codex CLI 0.154.0 | Reduction |
| --- | ---: | ---: | ---: |
| Total tokens | **95,139** | 467,901 | **79.7%** |
| Hypothetical API cost | **$2.37915** | $3.600914 | **33.9%** |
| Quality checks | 15/15 passed | 15/15 passed | — |

Total tokens include input and output; cached input is already part of input. Cost uses API prices frozen in the measurement specification, not actual Codex billing. Polaris took 23.5% longer across these 15 trials. These short synthetic tasks were run sequentially and do not establish reductions for general coding work or long conversations. See the [method, quality checks, timing, and limitations](docs/preimplementation-evaluation.md#v0120とcodex-01540の途中結果).

## Documentation

Start with the [documentation index](docs/README.md). Detailed documents below are in Japanese.

| Document | Contents |
| --- | --- |
| [Documentation index](docs/README.md) | Current guides, implementation references, and historical measurements. |
| [Usage guide](docs/usage.md) | Authentication, startup troubleshooting, configuration, envkit, and conversations. |
| [Command reference](docs/command-reference.md) | CLI/TUI commands and their output. |
| [System overview](docs/system-overview.md) | Execution, storage, isolation, and crate roles. |
| [v1.0.0-rc](docs/releases/v1.0.0.md) | Distribution scope, candidate checksums, verification, and limits. |


## Development and verification

```sh
cargo test --locked --offline --workspace
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## License

Released under the [MIT License](LICENSE). See the [TLS fixture notes](crates/polaris-http/src/fixtures/README.md) for the test material's source and license.
