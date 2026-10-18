# Development

## Environment

Linux is the initial development and validation target. This foundation is
checked in Linux CI; other operating systems are not yet supported targets.

Install Git, a C compiler/linker and standard development headers, and
[rustup](https://www.rust-lang.org/tools/install). On Debian/Ubuntu the system
prerequisites are `git`, `curl`, `ca-certificates`, and `build-essential`.

The repository pins Rust 1.99.1 with the minimal profile plus rustfmt and Clippy.
Inside the checkout, `rustup toolchain install` installs the toolchain from
`rust-toolchain.toml`, which also selects it for Cargo. If rustup was just
installed, open a new shell or source `"$HOME/.cargo/env"` so Cargo is on `PATH`.

There is no separate bootstrap script, container requirement, or mandatory editor.
An editor with rust-analyzer is optional; let its extension manage its language
server or install it separately through rustup.

## Commands

Run these from the repository root:

```sh
cargo build --workspace --locked
cargo run --locked -p harness -- --help
cargo run --locked -p harness -- --version
./scripts/check.sh
```

The check script verifies formatting, Clippy with warnings denied, workspace
tests and doctests, a development build, documentation with warnings denied,
and CLI help/version smoke checks. It can also be invoked from another working
directory. CI runs the same script with the repository toolchain.

The checks exercise J01–J03 storage behavior, the runtime core against real
storage, and the Rust API usage examples, including the specification journals
and injected durability failures. They do not prove provider compatibility,
workspace tools or sandbox enforcement.

For narrower checks while working:

```sh
cargo fmt --all
cargo check --workspace --locked
cargo test -p harness-core --locked
cargo doc --workspace --no-deps --locked --open
```

## Runtime tests

`crates/core/src/runtime/tests` drives the [runtime core](runtime.md) with a
synchronous test driver. The driver owns a `Runtime`, a real `JournalStore` in a
temporary directory with the oracle on, and a `DeterministicEnv`. It runs each
`Persist` as the storage worker will: `bind_command` when a command is attached,
then `append`. It answers with `Persisted` or `PersistFailed`. A storage
rejection fails the test: a commit the core believed legal is drift. Tests feed
provider events, timer firings and storage failures directly; injected failures
use the storage I/O failure points. Nothing reads the wall clock.

After every step the driver checks that at most one `Persist` is in flight, that
no request gets a second reply, and that event `seq` values are contiguous. At
the end it checks that every request got one reply, then reopens the journal and
compares the reopened snapshot with the core's last published one. An injected
I/O failure after a complete write leaves the failed commit durable. In that case
the reopened journal must be exactly one record ahead and contain that commit.
For a torn partial write, the inspected valid prefix must equal the published
snapshot.

Each scenario runs twice with fresh environments, and the effects are compared
with storage-chosen record IDs and checksums abstracted. The first run's
recorded inputs are also replayed into a fresh runtime and environment, and the
effects must be byte-identical (`Debug` output). Every recorded view plus the
events after it must reproduce the final view.

`randomized_interleavings_keep_storage_and_events_consistent` runs 512 seeds of
a SplitMix64 PRNG across threads. Each seed interleaves commands (including
retries, stale command IDs and media), PRNG-chosen artifact verification
answers, scripted streams with occasional corrupt
events, stale stream IDs, timer firings and injected persist failures. Each run
is then driven to quiescence. Every started turn must have exactly one
`turn_finished` in the journal; only a turn stopped by an injected fault may
have none. With `--nocapture` it prints the outcomes and paths it reached. A
failing run prints its seed. Rerun one seed, or run more:

```sh
RUNTIME_SEED=123 cargo test -p harness-core --locked randomized -- --nocapture
RUNTIME_SEEDS=5000 cargo test --release -p harness-core --locked randomized
```

### Async driver tests

`runtime/driver/tests.rs` drives `SessionHandle` with a private mpsc provider
stand-in. Every behavioral test uses
`#[tokio::test(flavor = "current_thread", start_paused = true)]`,
`DeterministicEnv`, real worker threads and real temporary journals. No test
reads the wall clock or sleeps for real. Cancellation timers fire only through
explicit `tokio::time::advance`; a yielding task prevents automatic time
advancement while a worker is doing real filesystem I/O. Test-only I/O gates
hold the worker at a durability boundary and notify the test by oneshot;
releasing the gate uses a standard channel, never a blocking receive on Tokio.
The injected I/O trace records worker thread names and sync ordering.

Coverage includes tool loops/limits, cancellation, upload limits and media
preflight, joining mid-stream, broadcast lag, epoch changes, unacknowledged
records, torn writes, poison, panic, dropped replies, overload and concurrent
shutdown during a commit. A torn tail blocks reopen until explicit repair;
reopen never repairs history or performs step-6 recovery.

```sh
cargo test -p harness-core --locked runtime::driver::tests
```

## Scaling benchmark

An opt-in, ignored test measures storage cost against history length. It runs
three single-chain workloads at 1,000, 2,000, 5,000 and 10,000 commits:

- **Plain turns:** one 4 KiB user entry and a failed turn per commit.
- **Tool turns:** a complete authorized tool exchange spread over twelve
  commits per turn.
- **Command tool turns:** tool turns whose opening commit binds a command, as
  the runtime's `StartTurn` does.

Each run prints the mean append time per commit, for the whole run and its last
10%, the tool workloads' time per turn, and the reopen time in total and per
record. Use a release build:

```sh
TMPDIR=/dev/shm cargo test --release -p harness-core --locked \
  session_storage_scaling_benchmark -- --ignored --nocapture
```

`TMPDIR=/dev/shm` keeps journals on tmpfs, so the figures exclude storage sync
latency; run once without it to see the on-disk cost. It performs 54,000 durable
commits and takes a few minutes. The command workload's extra cost per turn is the
command artifact's write, file and directory syncs, and its read-back during
validation.

Test builds run an equivalence oracle after every applied record. It compares
the cached branch facts and tool-state indexes with the scanning definitions
they replace, which makes each commit linear in history. The benchmark switches
it off for its own thread with `state::without_oracle`; every other test keeps it
on, including each thread of the randomized runtime test. The switch is
thread-local. `WorkerOptions::without_oracle` is a test-only startup hook:
it wraps the worker's open and entire receive loop in `state::without_oracle`
on that worker thread. Disabling the oracle on the Tokio/test thread has no
effect on the worker. Ordinary worker tests leave the hook off and the oracle on.

An additional opt-in benchmark drives 1,000 tool turns through `SessionHandle`
and compares them to a direct `JournalStore` run of the **same** six-commit
workload (C1, C2, proposal, unavailable-tool result, C2, stop), with command
artifacts in both. This differs from the older twelve-commit authorized-tool
workload above: comparing those directly would mix scheduling cost with
different storage work. Both benchmark paths disable the oracle on their own
storage thread. Only opt-in benchmarks measure real monotonic elapsed time;
behavioral driver tests use virtual time exclusively.

```sh
TMPDIR=/dev/shm CARGO_NET_OFFLINE=true cargo test --release -p harness-core --locked \
  session_handle_tool_turn_benchmark -- --ignored --nocapture
```

On 2026-10-18, Rust 1.99.1, tmpfs: direct store **0.558 ms/turn**;
`SessionHandle` **0.844 ms/turn**, a **0.286 ms/turn (+51.3%)** delta for the
driver, channels, extra thread and subscription delivery. This is a local
measurement without device sync latency, not a production throughput promise.

## Dependency and toolchain changes

The workspace uses Rust 2024 and Cargo resolver 3. Shared package metadata,
dependencies, and lints live in the root manifest. Each crate opts into them.
First-party unsafe code is forbidden; this does not audit dependencies or create
an execution sandbox.

Clap supplies CLI help, version, and argument validation. Storage uses Serde and
Serde JSON, the bundled JSON Schema validator with remote resolution disabled,
SHA-256, UUIDv7, UTC timestamps, Rustix filesystem/locking primitives, typed
errors and imbl persistent collections for sharing session snapshots. Tempfile
is test-only. Tokio is enabled with `default-features = false` and only `rt`,
`sync`, `time`, `macros`; tests additionally enable `test-util`. No Tokio
multithread runtime, I/O features or tokio-util are enabled. Ratatui and HTTP
clients will be added when their implementations need them.

Commit `Cargo.lock`. Use `--locked` in routine builds and CI. To intentionally
change dependencies, edit the manifest or run a targeted `cargo update -p NAME`,
review the manifest and lockfile changes, and run the checks. When changing Rust,
update `rust-toolchain.toml` and the workspace `rust-version` together, and update
the version mentioned in these docs. CI reads the toolchain file directly.

Network access is needed to install Rust and fetch uncached dependencies. Once
fetched, checks must not contact model providers or other services. To verify
this locally, set `CARGO_NET_OFFLINE=true` when running the check script.

## Secrets and runtime data

The current library/CLI do not read credentials or `.env` files. No API keys are required.
When providers are implemented, document their configuration and opt-in live
checks. Keep credentials out of Git, fixtures, logs, session entries, context
snapshots, and tool subprocess environments. Environment inheritance must be
explicit when process execution is introduced.

The storage library can create a private state root, checksummed journals and
content-addressed artifacts when explicitly called. The CLI creates no runtime
storage. See [session storage](session-storage.md) for API usage and limits.
Retention/deletion UI remains future work.

## Specification artifacts

[RFC-001](rfcs/001-core-types-state-persistence.md) has a bundled JSON Schema,
checksummed example journals, and conformance cases. Storage tests validate all
three examples and their artifacts and cover J01–J03. Runtime, security, model
and context gates remain future work.

Bubblewrap and Linux namespace support will be prerequisites for external tool
execution, not for the current storage/CLI checks. Do not install or
claim a working sandbox merely because it is the chosen backend.

## References

- [Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html)
- [Cargo dependency resolver](https://doc.rust-lang.org/cargo/reference/resolver.html)
- [rustup profiles](https://rust-lang.github.io/rustup/concepts/profiles.html)
- [Tokio setup and feature selection](https://tokio.rs/tokio/tutorial/hello-tokio)
- [Ratatui installation](https://ratatui.rs/installation/)
