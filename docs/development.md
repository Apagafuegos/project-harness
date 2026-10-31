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

`rustfmt.toml` pins `style_edition = "2024"` for both `cargo fmt` and bare
`rustfmt` (including editor/background formatters), keeping import ordering consistent.

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
CLI help/version smoke checks, a `harness context` smoke test that must
print the compaction fixture's golden report, and two runs of the
`scripted_session`, `tool_session` and `provider_session` examples whose output
must match. Provider examples use attributed loopback fixtures, not credentials.
It can also be invoked
from another working directory. CI runs the same script with the repository
toolchain.

The checks exercise J01–J03 storage behavior, the runtime core against real
storage including crashes and recovery, the context builder and its golden
reports, and the Rust API usage
examples, including the specification journals and injected durability
failures. Native fixture conformance, real workspace tools and the mandatory
S02 targets also run. Live provider compatibility is separate opt-in evidence;
see [Milestone 4 acceptance](milestone-4-acceptance.md).

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
no request gets a second reply, and that event `seq` values are contiguous.
While the core is recovering it also checks that the step starts no stream and
persists no assistant entry and no executor result. At the end it checks that
every request got one reply, then reopens the journal and compares the reopened
snapshot with the core's last published one. A run left faulted or unfinished
is then restarted, so recovery closes it: every started turn must end with
exactly one `turn_finished`, `interrupted` only with `process_restart`, every
assistant entry must come from a stream the core started, and each uncertainty
must be recorded once. An injected
I/O failure after a complete write leaves the failed commit durable. In that case
the reopened journal must be exactly one record ahead and contain that commit.
For a torn partial write, the inspected valid prefix must equal the published
snapshot.

The driver also simulates a killed process. `kill(crash)` decides the fate of
the commit in flight: `Lost` (never written), `Durable` (written, never
acknowledged) or `Torn` (half written). Requests the dead process held are
never answered. `restart()` inspects the journal, repairs a torn tail with the
inspected fingerprint and size, opens it under a new runtime with the next
identity stream and feeds `Start`. `inject` appends records through the dying
process's writer, as a tool runtime would have: an authorization, an
execution intent. `runtime/tests/recovery.rs` uses these for a crash in every
phase and for the uncertain cases.

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
is then driven to quiescence. Each step may also kill the process (2%, or 60%
while a healthy runtime has calls waiting for answers) with a lost, durable or
torn commit in flight. Before such a crash a tool runtime sometimes wrote its
authorization or execution intent. The process restarts at once, repairs a
torn tail, recovers, and resolves each uncertain invocation with a
PRNG-chosen statement (sometimes an invalid one the core must refuse). A
faulted runtime is restarted too. Every started turn must have exactly one
`turn_finished` in the journal, and storage must reject nothing. A second generator derived from the seed picks a context limit
(none in one seed in eight, two small ones that overflow after a few turns, or
a large one) and whether switch target `b` is listed, without changing the
seed's interleavings. Every context the core built is rebuilt from the reopened
journal and must have the same fingerprint and flags, every started stream's
context must have a known budget that fits, a build at a random head (and a
preview there) must pass the builder's checks, and no run may fault with an
internal cause. With `--nocapture` it prints the outcomes and paths it reached. A
failing run prints its seed. Rerun one seed, or run more:

```sh
RUNTIME_SEED=123 cargo test -p harness-core --locked randomized -- --nocapture
RUNTIME_SEEDS=5000 cargo test --release -p harness-core --locked randomized
```

### Async driver tests

`runtime/driver/tests.rs` drives `SessionHandle` through the public
`ModelProvider` seam: the scripted mock, or a test-only channel adapter where a
test paces events against observed views. Every behavioral test uses
`#[tokio::test(flavor = "current_thread", start_paused = true)]`,
`DeterministicEnv`, real worker threads and real temporary journals. No test
reads the wall clock or sleeps for real. Cancellation timers fire only through
explicit `tokio::time::advance`; a yielding task prevents automatic time
advancement while a worker is doing real filesystem I/O. Test-only I/O gates
hold the worker at a durability boundary and notify the test by oneshot;
releasing the gate uses a standard channel, never a blocking receive on Tokio.
The injected I/O trace records worker thread names and sync ordering.
Subscription convergence checks use quiescent views: an epoch reset fetches the
current view and may skip an intermediate recovery view returned by `reopen()`.
Waiting for exact equality with that historical view can hang after recovery.

Coverage includes tool loops/limits, cancellation, upload limits and media
preflight, joining mid-stream, broadcast lag, epoch changes, unacknowledged
records, torn writes, poison, panic, dropped replies, overload and concurrent
shutdown during a commit. A torn tail blocks open and reopen with
`PersistError::TornTail` until an explicit repair bound to its fingerprint and
size; stale values, a locked session and corrupt or headless journals are
refused. Recovery on open closes an interrupted turn without opening a stream,
and an uncertain invocation waits for `ResolveUncertain` through the handle.

```sh
cargo test -p harness-core --locked runtime::driver::tests
```

## End-to-end example

`crates/core/examples/scripted_session.rs` runs Milestone 2 in one place,
offline and deterministically: a session with a context limit and an
instruction, a turn that stops, a turn whose tool call is answered
`tools_unavailable`, a process killed while a third turn streams, a reopen that
recovers on its own, and the final context report. `check.sh` runs it twice
and diffs the output.

```sh
cargo run --locked -p harness-core --example scripted_session
```

## Headless `harness ask`

Implemented in M4 step 08 ([ADR 0020](decisions/0020-headless-ask.md)):

```sh
cargo run --locked -p harness -- ask --workspace "$PWD" "Explain this project"
printf 'Continue the explanation' | harness ask --workspace "$PWD" --session ID -
```

The default is OpenAI on your ChatGPT subscription (`--provider openai
--auth codex`), using the read-only Codex CLI login. On authentication failure,
run `codex login`. `--auth api-key` reads `OPENAI_API_KEY` instead. Model defaults
are `gpt-6.1-sol` (Codex) and `gpt-5.5` (platform); `--model MODEL` overrides
these, with sourced catalog limits only (unknown budgets fail explicitly).

Only assistant text goes to stdout, flushed as it arrives, with no added newline.
Status, bounded tool proposals/results, displayable reasoning summaries, final
per-response usage and errors go to stderr. Raw reasoning/encrypted state is never
displayed. Reasoning is dimmed only when stderr is a terminal.

Approvals use `/dev/tty`, not stdin: a literal `y` approves; everything else,
EOF or terminal failure denies. Without a controlling terminal **every call is
denied**, including with `--approve-read-only`. With a terminal that flag approves
only registry-declared read-only filesystem tools. Writes, edits and shell calls
always require a human `y`. No remembered grants are written. Unavailable shell
is reported once; file tools remain usable, with no unrestricted fallback.

A new session ID is printed on stderr. Continue with `--session ID` and the same
workspace, data root, auth profile and summary policy. The saved model is used
unless `--provider` or `--model` explicitly requests a switch. Switches default
to reject; `--switch-policy omit-optional` permits reported native-state loss,
never required media/tool incompatibilities. `--provider anthropic` uses
`ANTHROPIC_API_KEY`, not consumer subscription credentials.
Imported policies with remembered grants or different approval defaults are refused.
Uncertain side effects require explicit resolution through the runtime API; this
minimal client never guesses, replays tools or automatically repairs a torn tail.

`--data-root DIR` must be absolute and outside the workspace, following the
existing inspection commands' private storage rules. Its default is
`$XDG_DATA_HOME/harness`, or `$HOME/.local/share/harness` when XDG_DATA_HOME is
unset; parents are created as needed, and unsafe storage paths are still refused.
`--summary-model MODEL` explicitly enables automatic compaction with an independent
model. Without it automatic compaction is off, and overflow is
`context_budget_exceeded`. Repeat the same summary flag when continuing a session.
The process drains/shuts down after the turn, cancelling leftover idle summary
work; pre-start overflow compaction finishes before a turn can open.

First Ctrl-C sends runtime Cancel and waits for the durable cancelled finish;
second Ctrl-C exits immediately, leaving normal startup recovery to the next open.
Runtime timeout defaults apply: 300-second provider idle timeout, two-second cancel
grace, and no absolute inference deadline (the configured default is `None`).
Exit codes: **0** completed; **1** turn/provider/budget/infrastructure failure;
**2** usage/configuration/storage-open refusal; **3** authentication;
**130** cancelled. Errors include stable codes, and Codex authentication prints
`run \`codex login\``.

Offline tests inject `ScriptedProvider` and fake I/O into the client module.
`scripts/live-check.sh ask` is an explicit, quota-spending Codex smoke in a temporary
workspace; it checks exit 0 and stdout containing `pong`, printing only a sanitized
summary. It is never part of `check.sh` or CI.

New sessions record a factual built-in system instruction (workspace and available
tools) and, when present, the workspace root `AGENTS.md` at developer level.
Project text is read through descriptor-rooted resolution, capped at 64 KiB,
and must be UTF-8. Unsafe/symlinked, oversized or unreadable files are skipped
with one stderr warning. Nested `AGENTS.md` files are not read in v0.1. Edits
affect new sessions only; continuations keep their recorded instructions.
See [instructions](decisions/0022-recorded-model-instructions.md).

## Session catalog

```sh
harness sessions [--data-root DIR]
```

Prints the [read-only catalog](session-storage.md#read-only-session-catalog) as
a pretty JSON array, newest committed activity first, with IDs breaking ties.
The default root matches `ask`; nothing is created or repaired. Missing storage
prints `[]`. Busy and broken sessions remain listed with status and a short
reason, and unavailable metadata is null. Titles are first-user-message text,
first line, at most 120 Unicode scalar values. This is a linear journal scan,
not a persisted index. A live session remains available through its handle;
listing does not wait for its writer lock.

## Journal inspection and repair

`harness journal inspect` and `harness journal repair` are described in
[session storage](session-storage.md#journal-commands); `crates/cli/tests/journal.rs`
smoke-tests both on a torn copy of the compaction fixture.

## Context inspection

`harness context` prints the [context report](context.md) of a stored session
as JSON. It is read-only and takes a shared lock, so it refuses a session a
running engine holds, at once and with a message saying so:

```sh
cargo run --locked -p harness -- context <data-root> <session-id> \
  [--head ENTRY_ID] [--model PROVIDER/MODEL] [--context-tokens N]
```

`--head` inspects any entry, including one on another branch. `--model` builds
a preview for another model. `--context-tokens` configures the projected
model's context limit; without it the budget is reported as unknown (a running
engine would refuse such a turn with `context_budget_unknown`). The
command reads the session's configuration artifact for its instructions.

### Golden reports

`crates/core/src/context/golden/` holds the expected reports of the three
RFC-001 fixtures, exactly as `harness context --context-tokens 128000` prints
them. `context::tests::golden_*` compare them byte for byte, and
`scripts/check.sh` diffs the CLI's output for the compaction fixture against
its file. When a deliberate change alters a report:

```sh
UPDATE_GOLDEN=1 cargo test -p harness-core --locked context::tests::golden
git diff crates/core/src/context/golden/
```

Review the diff before committing it. A changed `fingerprint` with no other
change means the projection's bytes changed: bump `PROJECTION_VERSION` unless
nothing using the old fingerprints has been released. Never update golden
files to make an unexplained difference pass.

### Context benchmark

An opt-in benchmark appends a 10,000-entry branch (2,500 tool turns) with the
oracle off, then builds its context 21 times and prints the median, the time
spent fetching ancestry, hashing and checking, and the report's size:

```sh
TMPDIR=/dev/shm CARGO_NET_OFFLINE=true cargo test --release -p harness-core --locked \
  context_build_benchmark -- --ignored --nocapture
```

On 2026-10-21, Rust 1.99.1, aarch64, tmpfs: median **59.6 ms** per build
(ancestry 2.4 ms, fingerprint 17.8 ms, checks 14.9 ms) for 10,000 items; the
report is **1.81 MB** compact and 2.93 MB pretty-printed. This is a local
measurement, not a production promise.

## Host capabilities

`harness doctor` probes the host for the implemented file and sandbox tools and
prints JSON. It never changes host configuration. Tool-host assembly uses the
same capability checks and fails closed when enforcement is unavailable.

- **openat2:** resolution beneath a directory with `RESOLVE_BENEATH`,
  `RESOLVE_NO_SYMLINKS`, `RESOLVE_NO_MAGICLINKS` and `RESOLVE_NO_XDEV`; an
  in-tree open must succeed and `..`, absolute and symlink paths must fail.
- **Bubblewrap:** an absolute `--bwrap` path (default `/usr/bin/bwrap`, never
  searched in `PATH`), version 0.5.0 or later (`--clearenv`), and a probe
  sandbox with `--unshare-all --unshare-user --die-with-parent --new-session
  --clearenv`. Inside it there must be only `lo`, only the fixed environment,
  a new session, a writable workspace and refused writes outside it. From the
  host, each sandbox process's `uid_map` and `gid_map` must be exactly one
  line mapping one id to the invoking effective uid and gid.
- **Process limit:** `systemd-run --user --scope -p TasksMax=64` must start
  and its cgroup must show that `pids.max`. The user bus comes from
  `XDG_RUNTIME_DIR` (`bus_source: env`) or, when that is unset, from
  `/run/user/<euid>` if the directory is owned by the effective uid and not
  group- or world-writable and its `bus` is a socket owned by the same uid
  (`derived`). The derived value is passed to the `systemd-run` child only.
  Otherwise `bus_source` is `none`, with the reason, and the limit is
  reported as a gap.

CI runs it in the required Host sandbox security (S02) job, before and after loading the
committed AppArmor profile, and uploads the reports as the `harness-doctor`
artifact. [Decision 0008](decisions/0008-host-sandbox.md) records where the
shell security suite runs.

### When the shell sandbox is unavailable

The doctor's `bubblewrap.failures` says what failed. The most common cause on
Ubuntu 23.10 and later is `kernel.apparmor_restrict_unprivileged_userns=1`
with no AppArmor profile for `bwrap`: the user namespace is created without
capabilities and `bwrap` fails with `loopback: Failed RTM_NEWADDR: Operation
not permitted`. `host.apparmor_profiles_naming_bwrap` lists the profiles
in `/etc/apparmor.d` that name the `bwrap` path.

The harness never installs a profile. Allowing `bwrap` to create user
namespaces is a host security decision; `ci/apparmor/harness-ci-bwrap` is the
reviewed profile CI uses, and its grant matches Ubuntu's `usr.bin.bwrap`.
Without a working sandbox the shell tool stays unregistered, and the file
tools are unaffected.

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

Clap supplies CLI help, version, and argument validation. The CLI depends on
`harness-core`, `harness-providers`, Serde JSON and Tokio (including signal
handling); its tests use Tempfile. Storage uses Serde and
Serde JSON, the bundled JSON Schema validator with remote resolution disabled,
SHA-256, UUIDv7, UTC timestamps, Rustix filesystem/locking primitives, typed
errors and imbl persistent collections for sharing session snapshots. Tempfile
is test-only. Tokio is enabled with `default-features = false` and only `rt`,
`sync`, `time`, `macros` in core; tests additionally enable `test-util`.
Providers use reqwest with rustls, no default features, for HTTP/SSE; their
fixture servers enable Tokio networking/I/O. See ADR 0016 for that boundary.
No multithread runtime is required. Ratatui remains M5.

Commit `Cargo.lock`. Use `--locked` in routine builds and CI. To intentionally
change dependencies, edit the manifest or run a targeted `cargo update -p NAME`,
review the manifest and lockfile changes, and run the checks. When changing Rust,
update `rust-toolchain.toml` and the workspace `rust-version` together, and update
the version mentioned in these docs. CI reads the toolchain file directly.

Network access is needed only for provisioning Rust, OS packages and locked
dependencies (`cargo fetch --locked`). `check.sh` exports `CARGO_NET_OFFLINE=true`
and checks that no live entry point joined validation. CI explicitly fetches
before validation and then runs tests in a loopback-only network namespace.
Reproduce that enforcement (after provisioning) with a timeout of at least 60 minutes:

```sh
./scripts/with-offline-network.sh ./scripts/with-clean-fds.sh \
  env CARGO_NET_OFFLINE=true ./scripts/check.sh
```

`with-offline-network.sh` requires noninteractive sudo, `unshare`, `ip` and
`setpriv`, and has no fallback. It creates only a network namespace, brings up
loopback, then drops to the caller's UID/groups before running validation.
It preserves the host user namespace so UID-map security tests remain meaningful.
Loopback fixture sockets are intentional; “offline” means no external network,
not no sockets. CI checkout/toolchain/apt/dependency provisioning does use the
network; it is not part of the offline validation claim.

## Secrets and runtime data

Ordinary tests/examples require no credentials and never read the real Codex
login. The live CLI reads the explicitly selected API-key environment variable
or the read-only Codex login; it does not load `.env` files. See
[providers](providers.md) for configuration and separate opt-in live checks.
Keep credentials out of Git, fixtures, logs, session entries, context snapshots,
and tool subprocess environments. Tool environment inheritance is allowlisted.

The library and `harness ask` create private state roots, checksummed journals
and content-addressed artifacts. `harness context` only reads an existing one. See [session storage](session-storage.md) for API usage and limits.
Retention/deletion UI remains future work.

## Specification artifacts

[RFC-001](rfcs/001-core-types-state-persistence.md) has a bundled JSON Schema,
checksummed example journals, and conformance cases. Storage tests validate all
three examples and their artifacts and cover J01–J03. Context tests build
golden reports from all three and cover C01. Native model conformance and C02
run in the gate; [M4 acceptance](milestone-4-acceptance.md) maps each case.

Bubblewrap and Linux namespace support are required for external execution and
the full security gate. A missing backend fails closed; the explicit local
`HARNESS_ALLOW_NO_SANDBOX=1` opt-out is not acceptance evidence.

## References

- [Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html)
- [Cargo dependency resolver](https://doc.rust-lang.org/cargo/reference/resolver.html)
- [rustup profiles](https://rust-lang.github.io/rustup/concepts/profiles.html)
- [Tokio setup and feature selection](https://tokio.rs/tokio/tutorial/hello-tokio)
- [Ratatui installation](https://ratatui.rs/installation/)
