<img src="assets/icon.svg" width="64" height="64" alt="">

# project-harness

A small, extensible coding-agent harness built around transparent session and
context management. Inspired by Pi's minimalist philosophy, with its foundations
implemented independently in Rust.

Rust only: Tokio for asynchronous runtime work, Ratatui for the terminal UI, and
Cargo for the build. The engine stays separate from the client so it can run
headlessly and support other clients later.

**Status: Milestones 1–6 complete within their recorded scope.** `harness-core`
implements validated session trees, checksummed JSONL transactions, immutable
snapshots, private content-addressed artifacts, single-writer locking, and
explicit torn-tail repair. J01–J03 tests cover replay, corruption rejection and
injected durability failures. Its `runtime` module is a pure, synchronous turn
state machine: typed inputs and effects, one commit in flight, journal-backed
command deduplication, cancellation and an event-sequence resync view. Every
commit it emits in tests is appended to real storage. A dedicated storage worker and
Tokio driver now expose an async command handle, media uploads, cancellation,
shutdown/reopen and subscriptions that resync after lag or runtime replacement.
Sessions stream inferences from a public `ModelProvider` seam: a deterministic
mock plus native OpenAI Responses and Anthropic Messages adapters behind a Router. The `context` module is a pure builder: for
any head it projects that branch for the model, honoring recorded summaries,
and returns a report that accounts for every entry on the branch, with
labelled estimates, an honest budget and a reproducible fingerprint. The core
builds it before every inference and fails an overflowing turn before calling
the provider. `SessionHandle::inspect_context` and the read-only
`harness context` command expose the report. The library now runs authorized
read/write/edit via descriptor-rooted Rust operations and bash only through
probed Bubblewrap, with sequential scheduling, durable intent/results, approvals
and explicit uncertainty recovery with advisory file-image hints. Real Linux
security tests are required in CI. Rolling summaries support automatic triggers,
manual preview/commit and exact source manifests without rewriting history.
`harness ask` streams one native turn with terminal tool approvals, and plain
`harness` opens an interactive Ratatui client over the same engine: streaming,
navigation, approvals, cancellation and a read-only context inspector, with a
durable terminal lifecycle. Whole-turn pins and exclusions are append-only branch
entries (record version 3); compaction skips controlled turns, keeps pinned turns
verbatim and never sends excluded ones to a summary model. Three selectable
strategies share one registry, including the deterministic, model-free
`observation_masking`, which is the recorded default for new sessions; decoding
an omitted strategy in older journals still means `rolling_summary`. A dev-only
offline evaluation crate and two separate, opt-in DeepSeek Flash matrices measure
the strategies. Validation is offline after provisioning; live checks are
separate. Anthropic live evidence still needs an API key, and the live TUI smoke
met the Codex quota limit; both are recorded as partial in the acceptance
documents.

## Getting started

Install [Rust through rustup](https://www.rust-lang.org/tools/install). On Linux,
also install a C compiler/linker and standard development headers (for example,
`build-essential` on Debian/Ubuntu). Git is needed to clone the repository.

From the repository root:

```sh
rustup toolchain install
cargo fetch --locked
cargo build --workspace --locked
cargo run --locked -p harness -- --help
./scripts/check.sh
```

`rust-toolchain.toml` selects Rust **1.99.1**, rustfmt, and Clippy automatically.
`Cargo.lock` fixes dependency resolution. No API keys, Docker, Node.js, Go, or
external services are needed for storage tests or the CLI checks.

### Run a session

`harness` is the terminal client. Run it inside the workspace you want the agent
to work on; the canonical current directory becomes the authorized workspace, and
both stdin and stdout must be terminals:

```sh
cd ~/src/my-project
/path/to/project-harness/target/debug/harness
```

Sessions live outside the workspace in a private data root:
`$XDG_DATA_HOME/harness` when that variable is set, otherwise
`~/.local/share/harness`. Pass `--data-root DIR` to choose another absolute
location. Sessions are JSONL journals named by session ID; list them with
`harness sessions` and inspect one with `harness context ROOT SESSION`.

For pipes, scripts and CI use the headless client instead — it never needs a
terminal and writes only assistant text to stdout:

```sh
harness ask --workspace ~/src/my-project "Explain the build script"
printf 'Now the tests' | harness ask --workspace ~/src/my-project --session SESSION_ID -
```

A new session records a built-in instruction plus the workspace's root
`AGENTS.md`. The default provider is OpenAI through the read-only Codex login
(`codex login` once); `--provider anthropic` uses `ANTHROPIC_API_KEY`, and
`--provider deepseek` uses `DEEPSEEK_API_KEY` on DeepSeek's
Anthropic-compatible endpoint. New sessions record `observation_masking` as
their compaction strategy (`--compaction-strategy` overrides it; a continuation
keeps its recorded strategy unless explicitly changed, and naming a different one
is refused). Press Ctrl-K in the TUI for the context inspector, `p`/`x` in the
branch tree (Ctrl-T) to pin or exclude a turn, and Ctrl-D to quit.

## Repository

```text
crates/core/       Headless session storage, runtime core and context builder (harness-core)
crates/cli/        Terminal client and headless ask/context/journal/doctor CLI (harness)
crates/providers/  Native adapters, router, HTTP/SSE and credentials (harness-providers)
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

See [session storage usage](docs/session-storage.md), the
[storage decision](docs/decisions/0003-jsonl-storage.md), the
[runtime core](docs/runtime.md) and its [decision](docs/decisions/0004-pure-runtime-core.md),
the [context builder](docs/context.md) and its
[decision](docs/decisions/0006-context-builder.md), and
[startup recovery](docs/decisions/0007-startup-recovery.md). Milestone 2 is
complete; [its acceptance check](docs/milestone-2-acceptance.md) maps each
criterion to tests, and `cargo run --example scripted_session` runs it end to
end. [Milestone 3 acceptance](docs/milestone-3-acceptance.md) maps tools, real
security and recovery to tests; `cargo run --example tool_session` demonstrates
real files, approvals and crash resolution offline.
[Milestone 4 acceptance](docs/milestone-4-acceptance.md) maps native provider,
compaction and CLI evidence; `cargo run --example provider_session` demonstrates
both native adapters and compaction offline. [Milestone 5
acceptance](docs/milestone-5-acceptance.md) maps the terminal client: its
rendering, navigation, streaming, cancellation, approvals, inspector, terminal
lifecycle, lag coherence and client/engine boundary; `scripts/live-check.sh tui`
is the separate, opt-in live smoke. [Milestone 6
acceptance](docs/milestone-6-acceptance.md) maps whole-turn pins/exclusions,
record version 3, the three strategies, the DeepSeek profile, the offline
evaluation harness and the chosen default; [context
experiments](docs/context-experiments.md) records the two bounded live matrices
and their limits. See the [roadmap](docs/roadmap.md).

## Security

Report vulnerabilities privately; see [SECURITY.md](SECURITY.md).

## License

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
Copyright (c) 2026 Apagafuegos.

Created in [T3 Code](https://t3.codes).
