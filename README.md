<img src="assets/icon.svg" width="64" height="64" alt="">

# project-harness

A small, extensible coding-agent harness built around transparent session and
context management. Inspired by Pi's minimalist philosophy, with its foundations
implemented independently in Rust.

Rust only: Tokio for asynchronous runtime work, Ratatui for the terminal UI, and
Cargo for the build. The engine stays separate from the client so it can run
headlessly and support other clients later.

**Status: foundation scaffold.** The workspace builds and the `harness` command
shows help and version information. Agent execution, provider adapters, tools,
session storage, context management, and the TUI are not implemented. Six
high-level contracts are provisionally agreed; RFC-001 proposes their concrete
types, state transitions, persistence format, and conformance requirements.

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
external services are needed for the scaffold or its checks.

## Repository

```text
crates/core/       Headless engine library boundary (harness-core)
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

The next step is reviewing RFC-001 before implementation. Then build validated
core types and persistence, followed by a deterministic mock-provider lifecycle.
See the [roadmap](docs/roadmap.md) for acceptance criteria.

Created in [T3 Code](https://t3.codes).
