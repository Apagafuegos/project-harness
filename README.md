<img src="assets/icon.svg" width="64" height="64" alt="">

# project-harness

A small, extensible coding-agent harness built around transparent session and
context management. Inspired by Pi's minimalist philosophy, with its foundations
implemented independently in Rust.

Rust only: Tokio for asynchronous runtime work, Ratatui for the terminal UI, and
Cargo for the build. The engine stays separate from the client so it can run
headlessly and support other clients later.

**Status: first headless storage milestone.** `harness-core` implements validated
session trees, checksummed JSONL transactions, immutable snapshots, private
content-addressed artifacts, single-writer locking, and explicit torn-tail repair.
J01–J03 tests cover replay, corruption rejection and injected durability failures.
The `harness` command still shows only help and version information. Agent
execution, provider adapters, workspace tools, context projection, and the TUI
are not implemented. Persisted runtime types/events do not execute an agent.

## Getting started

Install [Rust through rustup](https://www.rust-lang.org/tools/install). On Linux,
also install a C compiler/linker and standard development headers (for example,
`build-essential` on Debian/Ubuntu). Git is needed to clone the repository.

From the repository root:

```sh
rustup toolchain install
cargo build --workspace --locked
cargo run --locked -p harness -- --help
./scripts/check.sh
```

`rust-toolchain.toml` selects Rust **1.99.1**, rustfmt, and Clippy automatically.
`Cargo.lock` fixes dependency resolution. No API keys, Docker, Node.js, Go, or
external services are needed for storage tests or the CLI checks.

## Repository

```text
crates/core/       Headless session storage library (harness-core)
crates/cli/        CLI entry point and future Ratatui client (harness)
docs/             Architecture, contract drafts, decisions, and roadmap
scripts/check.sh  Shared local/CI checks
```

Start with the [documentation index](docs/README.md),
[development guide](docs/development.md), and
[RFC-001](docs/rfcs/001-core-types-state-persistence.md).

The v0.1 constraints are tree-based append-only JSONL sessions, independent
context projection with rolling summaries, authorized trusted Rust file tools,
and Bubblewrap for every external executable. Tool calls run sequentially.
The context inspection API belongs to the headless engine from the first slice.

See [session storage usage](docs/session-storage.md) and the
[storage decision](docs/decisions/0003-jsonl-storage.md). The next milestone is a
deterministic mock-provider lifecycle and headless context inspection, with
Tokio orchestration over the blocking store. See the [roadmap](docs/roadmap.md).

Created in [T3 Code](https://t3.codes).
