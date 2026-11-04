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
toolchain. The workspace `dev` profile (also used by tests) gives workspace
crates debug level 1, gives dependencies no debug information and disables
incremental artifacts, so the default/conformance/test-provider matrix fits
hosted-runner disk bounds. Line-level debugging remains available and no test
or security gate is skipped. `check.sh` sets no profile overrides, so ordinary
`cargo build` and `cargo test` share its artifacts instead of building a second
set; avoid `CARGO_PROFILE_*` and `CARGO_INCREMENTAL` overrides for the same
reason.

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

## Using the TUI

Run `harness` in the workspace directory (or `cargo run -p harness --` from
this repository). Plain `harness` opens terminal chat; there is no `tui`
subcommand. Both stdin and stdout must be terminals; otherwise it exits 2 with
`use harness ask` before touching the terminal. For pipes and scripts use
`harness ask --workspace DIR PROMPT` instead.

```sh
harness --data-root /absolute/private/state
harness --provider openai --auth codex --model gpt-6.1-sol
harness --provider anthropic --model claude-sonnet-5-5
harness --provider deepseek --model deepseek-flash
harness sessions
harness --session SESSION_ID
```

The workspace is the canonical current directory and must be authorized and
outside the private storage root. The seven options `--data-root`, `--provider`,
`--auth`, `--model`, `--summary-model`, `--compaction-strategy`, and `--session`
have ask's defaults, including the user-level config file described below.
Global `--config PATH` and `--no-config` control config loading.
Without `--data-root`, storage is `$XDG_DATA_HOME/harness`, otherwise
`$HOME/.local/share/harness`. New sessions record the built-in, user and
root/nested project instructions; edits affect new sessions only. Continuations keep their saved
instructions and model unless explicitly overridden, and enforce the shared
policy/credential checks. An explicit switch uses the loss-rejecting policy;
refusal is visible in the banner and retains the saved model. Repeat the saved
`--summary-model` choice when continuing a session that enabled compaction.

A **new** session records `observation_masking` as its compaction strategy by
default, in both the TUI and `ask`; `--compaction-strategy` overrides it. Masking
is deterministic and needs no summary model, so its compaction entries omit
`Compaction.model`. Without `--summary-model`, new sessions leave automatic
compaction off; supplying that option still enables it. A continuation keeps the strategy already
recorded when the flag is omitted, and old configurations that never named a
strategy keep their `rolling_summary` meaning; naming a different strategy on
continuation is refused with `session_policy_mismatch`. Changing it is otherwise
the recorded idle switch, not a flag: the clients do not send it in this
milestone.

- Enter sends; Shift+Enter or Alt+Enter inserts a newline. Bracketed paste is
  one prompt edit, never a series of commands.
- Esc cancels the active turn. Ctrl-C cancels; a second Ctrl-C during that turn
  quits only after its durable finish. While idle, Ctrl-C asks for confirmation.
- Ctrl-D with an empty prompt quits, cancelling any active turn durably. With a
  nonempty prompt it does nothing. In a quit confirmation, Enter/y confirms;
  n/Esc returns to editing.
- Approval captures the keyboard: `a` allows once, `d` denies, `r` allows and
  remembers, and Esc dismisses **by denying**. There is no automatic approval.
  `v` switches the preview between the readable summary and the raw JSON
  arguments; it never answers. F1 opens help and Up/Down or PgUp/PgDn scrolls
  the frame; no other key answers a ticket, and a paste never answers one.
  Remembered mutation grants follow the existing continuation policy checks;
  a session with changed allowances may be refused on reopen.
- On uncertain recovery, Up/Down selects an invocation, Left/Right or Tab
  selects a resolution, and Enter submits the user's statement. Observed
  success/failure needs text. Nothing reruns the uncertain operation.
- Ctrl-O opens the session picker; Up/Down selects, Enter opens, `n` creates a
  session with the current workspace and flags, `d` deletes, `e` exports and Esc
  closes. Rows show title, model, activity, durable state and catalog status.
  Busy/unreadable sessions cannot be opened; needs-repair rows direct you to
  `harness journal repair`.
- In the picker, `d` asks for confirmation in the dialog frame, naming the
  session's title and ID and stating that deletion is permanent and removes only
  the journal; `y` or Enter deletes, `n` or Esc cancels. The session this client
  owns cannot be deleted: the dialog says to switch first, and the shell refuses
  the same action anyway. A session another engine holds shows `session_locked`
  verbatim and is left alone. The list refreshes after a delete.
- `e` opens a one-line path prompt prefilled with `./harness-<short id>.md`.
  Enter writes Markdown for the selected session and shows the written path; an
  existing file is refused with `export_exists` and nothing is overwritten. The
  current session's export comes from the client's own durable snapshot, since
  its writer lock excludes the shared read.
  Other workspaces are listed but refused, never silently authorized. Handover
  requires idle, durably shuts down the old handle and uses the same continuation
  checks. Listing and opening are sequential; a raced `session_locked` gets one
  retry after 30 ms, then a busy banner. A failed open attempts to restore the
  previous session rather than creating a replacement silently.
- Ctrl-T opens the branch tree. Up/Down selects a turn; Enter selects its head
  and re-projects the transcript. `*` marks the current branch; rows show prompt
  titles, branch indentation and models. `e` prefills the prompt for editing;
  Enter selects the original message's parent, then starts the edited turn as a
  sibling. An already-selected parent needs no redundant navigation. Original
  history remains immutable. Core refusals are shown without submitting a later
  StartTurn after a failed SelectHead.
- In the branch tree, `p` pins the selected turn and `x` excludes it; pressing
  the same key again resets it to the default mode. A pinned turn becomes
  excluded in one step, and the reverse. Each key sends exactly one
  `SetContextControl` for the whole turn, and its mode marker (`[pin]` or
  `[excl]`) leads the row. A turn needs a finished outcome and an idle session,
  so a key pressed on the active turn writes nothing and shows the engine's own
  refusal; other refusals (`context_control_covered`, `context_control_noop`,
  `context_control_not_ancestral`) arrive from the engine and appear verbatim in
  the banner. `p` and `x` are ordinary prompt characters outside the tree.
- Ctrl-L lists models from the registered providers' merged catalog, including
  sourced limits and capabilities. Enter always attempts `Reject` first. On
  portability loss, Up/Down scrolls the classified entry/part/owner/schema/byte
  report (never native payloads); `o` explicitly confirms omission of optional
  state. Required incompatibilities cannot be omitted. Esc cancels. Success
  updates the status model and its catalog budget; busy switching is refused.
  Navigation captures paste without sending it; approval and uncertainty dialogs
  retain input precedence. Ctrl-C/Ctrl-D retain their cancellation/quit rules.
- PageUp/PageDown scroll; Ctrl-Up/Down scroll one line; Ctrl-R toggles expanded
  reasoning summaries and Ctrl-E expands every tool result. End with an empty
  prompt or Ctrl-End follows output.
  Scrolling up keeps the viewport fixed as output arrives and shows a new-lines
  counter and position percentage. Resize relays out immediately.
- The prompt is a multi-line editor with a real terminal cursor. Up/Down keep
  the column across logical lines; on the first/last line they recall prompt
  history (current branch plus this process's sends), then restore the draft.
  Left/Right and Home/End move the cursor; Ctrl-Left/Right move by word,
  Ctrl-W deletes the previous word and Ctrl-U deletes to the line start.
  Backspace/Delete remove the character before/at the cursor. Ctrl-D never
  deletes. A paste over 20 lines labels the prompt title, but stays editable.
- Esc closes any open navigation or inspector dialog; the quit confirmation and
  the approval dialog have their own Esc behavior above. Keys are read only from
  the terminal event stream and the Ctrl-C signal; no timer or tick can type,
  send or approve.

### UX and key reference

F1, or `?` with an empty prompt, opens the generated key reference. It groups
editing, turn, navigation, context and dialog keys from the same table that
admits input. Up/Down scrolls, PgUp/PgDn pages, and Esc/F1 closes back to the
underlying screen. `?` with text in the editor remains ordinary text.

| Area | Keys |
| --- | --- |
| Editing | Enter send; Shift+Enter / Alt+Enter newline; arrows, Home/End; Backspace/Delete; Ctrl-Left/Right words; Ctrl-W previous word; Ctrl-U line start; Up/Down history at first/last line |
| Turn | Esc cancel; Ctrl-C cancel then durable quit (idle confirmation); empty Ctrl-D quit |
| Navigation | PgUp/PgDn page; Ctrl-Up/Down line; empty End / Ctrl-End follow; Ctrl-O sessions (then `d` delete, `e` export); Ctrl-T branches; Ctrl-L models |
| Context | Ctrl-K inspector; Ctrl-R reasoning; Ctrl-E full tool output; tree `p` pin / `x` exclude |
| Dialogs | F1 help; Up/Down selection/scroll; PgUp/PgDn scroll; Enter confirm; Esc close (approval **denies**, recovery stays open); approval `v` raw arguments; recovery Left/Right or Tab/Shift-Tab choices |

The chrome is intentionally minimal: a dim header, role-coloured gutters and
bold labels, identical user/assistant indentation, a mode-titled prompt rule
and `>` marker, state-specific key hints and a full-width reversed status row.
Assistant labels carry dim right-aligned model, observed duration when known,
and human-readable input/output token counts. Reopened turn durations are
omitted because the exposed view has no record timestamps. Ordinary completion
adds no noise row; failed/cancelled/interrupted outcomes retain their codes.
The empty editor shows `Ask anything · F1 help`; an empty session welcomes you
with its workspace/model and four useful keys. Header, hints and prompt rule
collapse before content at small sizes (minimum 20×5).

The status row shows workspace/provider/model, activity/elapsed time/streamed
bytes, and estimated context with the catalog limit and percentage. It drops
right-hand details at narrow widths. A 10 Hz injected tick drives active-turn
activity; idle clients have no perpetual timer. Any **nonempty** `NO_COLOR`
disables all colours while retaining meaningful modifiers; `TERM=dumb` uses
an ASCII spinner and gutter. Theme detection is once in the shell, not during
drawing. Dialogs size to their content within the viewport and show `more ↓`
when scrollable. Errors wrap to three lines plus an ellipsis: Esc dismisses
when idle and not in a dialog; active-turn Esc still cancels. Sending also
clears the old banner. There is no mouse capture; terminal selection still works.
Assistant text is GitHub-flavoured Markdown and user text is plain.
Typography does the work: H1 is bold accent and underlined, H2 bold, deeper
headings bold and dim, emphasis italic, strong bold, strikethrough crossed out,
inline code a distinct dim yellow with no backticks, link text underlined with a
dim ` (url)` suffix unless the text is the URL. Lists hang their continuation
lines under the item text and use `•`/`◦`/`▪` (ASCII `-`/`*`/`+`) by depth,
keeping ordered start numbers and `[x]`/`[ ]` task marks. Block quotes nest a
dim `│ ` gutter, rules span the pane, images show `[image: alt]`, raw HTML stays
literal, and footnote references `[^n]` list their definitions at the end. GFM
tables use the delimiter row's alignment, a bold header and single-rule
separators; a table wider than the pane hard-wraps rather than reflowing into a
different shape. User messages keep their newlines and leading spaces, wrap
instead of truncating, and treat fenced blocks as code.

Fenced code is a dim language label (`code` when untagged) over a dim `│ `
gutter, never reflowed: an overlong row hard-wraps with `↪`. Highlighting is
`syntect` with bundled grammars and the pure-Rust `fancy-regex` backend, mapped
to the theme's ANSI roles (keyword magenta, string green, number/constant cyan,
function blue, type/tag yellow, comment dim). It stops past 2,000 lines or
256 KiB per block, renders unknown languages plain, and is off under
`NO_COLOR`. Nothing downloads grammars.

Tool calls are one compact header line: a status mark (`✓` ok, `✗` failed,
`…` running, `?` uncertain, `⊘` denied or cancelled; ASCII `ok`, `x`, `..`,
`?`, `-`), the tool name and a per-tool summary — the command for `bash`, the
path and byte range for `read`, the path and byte count for `write`, the path
plus a small red/green diff for `edit` — with the exit code or failure code
always visible. Result bodies collapse to three lines plus `… +N lines`; Ctrl-E
shows everything. Recorded approvals stay as one dim line under the call.
Reasoning is dim italic behind `│ `, collapsed to a line until Ctrl-R. Model
errors show their stable code and message, and provider state is one dim line.

The approval dialog asks in human terms: `bash` shows the command, its
resolved working directory, timeout and `network: denied`; `edit` shows the
path and the scrollable diff; `write` shows the path, size and first ten lines;
`read` shows the path and byte range; other tools pretty-print their arguments.
The plan digest stays visible.

### Context inspector and compaction

Ctrl-K opens/closes the read-only context inspector. PgUp/PgDn scrolls its
included items in projection order, exclusions with reason codes, fingerprints
and configuration identity. Budgets label used/remaining counts as **estimates**;
opaque bytes are not tokens. Items of a pinned turn are labelled `pinned`,
excluded entries carry `user_excluded`, and a `Controls (effective on this
branch)` section lists each non-default turn's mode with the control entry that
set it. The panel refreshes after every control, because a control entry
advances the head. Reports refresh asynchronously when the published
session/head/sequence/model changes, without waiting in the input loop.

An excluded turn stays in the transcript, in history and in this report; it is
only kept out of the model's request. Such a turn is tagged with one dim
`excluded turn (kept in history; never sent to the model)` row, so it is
visibly excluded and still readable. The durable `Pinned turn …`, `Excluded
turn …` and `Reset turn …` markers are unchanged.

Inside the inspector, `m` opens the merged catalog's model list. Up/Down selects
and Enter previews that model's context; the panel explicitly says **nothing
switched**. Select “Session model” to clear the override. This never sends a
switch command or writes history. Esc dismisses the chooser.

`c` asks the engine for a compaction preview using the recorded strategy. A
model-based strategy uses the independently configured `--summary-model` and
fails with `summary_model_unconfigured` if none is recorded; the default
`observation_masking` makes no provider call at all and shows its deterministic
rendering. Progress is visible and Esc cancels generation. An ineligible session
displays the core's reason without calling a provider. The proposal
shows the summary, covered/superseded source manifest and provenance, and
`CompactionDiff` (new coverage, supersession, per-category before/after estimates
and whether it fits). Scroll to read the full proposal; it may increase tokens.
Enter commits **only after the preview has been rendered**; Esc discards it
without changing journal bytes. Ctrl-K also cancels/discards when closing.
Commit cannot be cancelled once submitted. A `stale_preview` rejection identifies
the changed binding; press `c` for a new preview, never an automatic retry.

Automatic compaction is visible in the transcript and refreshes an open report.
Failures show their stable code in the banner; budget refusals retain the core's
ineligibility reason, or suggest Ctrl-K then `c` when compaction is eligible.
Approvals and recovery dialogs retain input precedence over the inspector.

SIGTERM/SIGHUP restore the main screen immediately, then perform durable
shutdown. Panic restoration runs before the panic diagnostic. Faults remain
visible and Ctrl-D can leave cleanly. A torn journal still requires explicit
`harness journal inspect` / `repair`; the TUI does not silently repair it.

### TUI verification

`cargo test -p harness --test tui_inspector` runs 17 offline inspector tests:
real storage and ScriptedProvider summary requests, pure TestBackend buffers,
model previews without switching, preview-first commitment, cancellation byte
identity, stale fields, eligibility, automatic compaction, budget diagnostics,
slow inspection responsiveness, live/stale preview reads and
opaque-state/schema sentinel redaction.

`cargo test -p harness --test tui_controls` runs nine offline context-control
tests: the tree's `[pin]`/`[excl]` markers at 80×24 and 20×5, the tagged and
dimmed excluded transcript (asserted from cell modifiers), the full `p`/`x`
toggle mapping through the real journal, and the production loop for pin,
exclude (with the next recorded request omitting the turn), reset, a
`context_control_covered` refusal after a committed compaction, a `busy`
refusal on the active turn and pinned-to-excluded in one control.

`cargo test -p harness --test tui_runtime` exercises the injectable loop with
TestBackend, scripted input/provider streams, and real storage.
`cargo test -p harness --test tui_navigation` additionally covers picker status
refusals, durable session handover/new sessions, one bounded lock retry,
sequential list/open under injected catalog latency, branch re-projection and
immutable siblings, busy commands, reject-first switching, explicit optional
omission and required incompatibilities. The separate
`tui_recovery` target kills a child after a durable write intent, then resolves
it through the running app without replay. The separate `tui_pty` target tests
non-TTY refusal, the shared flags plus global config CLI surface, and restores the
terminal after a returned loop error; enabling the default-off `test-provider`
feature also runs the real binary under its own PTY for quit, panic, SIGTERM and
that returned error, checking termios and restoration sequences. `check.sh` runs
both feature configurations. The fixture
environment hooks are compiled only with that feature **and debug assertions**;
they cannot activate in a release binary. `portable-pty` is test/example-only.

`cargo test -p harness --test tui_boundary` enforces the client/engine boundary
over every file under `crates/cli/src/tui/`: no provider adapter or request type,
no executor/workspace/sandbox/policy type, no code inclusion, `SubmitApproval`
only in the modal key handler, and the key reducer reachable only from the
terminal event arm and the Ctrl-C signal arm. It is a conservative source scan,
not a security sandbox, and it self-tests that each mutation fails. The shell
`crates/cli/src/tui_cli.rs` may use `wiring`; the model and loop may not.

`scripts/eval.sh` wraps the opt-in compaction evaluation in `crates/eval`
(live mode spends money; offline mode is in `scripts/check.sh`).

`scripts/live-check.sh tui [--provider openai|deepseek]` is a separate, opt-in
live smoke. The default `openai` uses the read-only Codex subscription login;
`deepseek` reads `DEEPSEEK_API_KEY` and skips with exit 0 when it is absent.
Both start the real binary under a PTY, type the fixed pong request, observe the
completed answer, send Ctrl-D, and verify exit zero and terminal restoration.
The smoke prints the selected provider and model plus sanitized evidence only.
It spends quota once, never prints credentials, and does not retry rate limits.
Live results are recorded in [M5 acceptance](milestone-5-acceptance.md).

#### TUI known limits

- User-only config defaults (ADR 0028); no workspace/project config. Ask-only
  approval and startup-switch settings never automate TUI approval or change its
  existing model-dialog policy.
- The new-session strategy default is `observation_masking`. It is lossy for tool
  observations, does not guarantee a smaller projection, and provides no
  workspace rollback. Old and continued sessions keep their recorded strategy;
  the clients expose no mid-session strategy command in this milestone.
- User and root/nested project `AGENTS.md` are read only at creation, under
  the per-file, aggregate and discovery bounds described below.
- Highlighting is capped per block (2,000 lines or 256 KiB) and unknown
  languages stay plain; a table wider than the pane hard-wraps. Raw HTML is
  inert text and user messages are never interpreted as Markdown.
- One workspace per process: the picker lists other workspaces and refuses to
  open them. There is no session delete/retention/export UI and no credential UI.
- A model outside the merged catalog that differs from the opened model shows an
  unknown context budget. Inspector budgets are labelled heuristic estimates.
- The TUI never repairs a torn tail; use `harness journal inspect`/`repair`.
- Live Anthropic use and cross-provider live switching still need
  `ANTHROPIC_API_KEY`; the TUI supports the same provider flags as `ask`.

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

Storage refusals are the one outcome the harness must never see. A reopen or an
inspection that reports `session_locked` after the writer closed fails with the
journal path and every descriptor in the process that still resolves to it, so
the log distinguishes a leaked descriptor from a forked child that still holds
an inherited copy (ADR 0012). Journal lock acquisition and subprocess spawning
take one test-only gate, `crates/core/src/spawn_gate.rs`, so the two cannot
overlap and such a refusal is always a real defect.

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
`ANTHROPIC_API_KEY` and `--provider deepseek` uses `DEEPSEEK_API_KEY`, never
consumer subscription credentials.
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

New sessions record the instructions described below. Edits affect new
sessions only; continuations keep their recorded instructions.

## User configuration

M7 supersedes M5's flags-only decision ([ADR 0028](decisions/0028-user-config.md)).
Both clients load `$XDG_CONFIG_HOME/harness/config.toml`, falling back to
`$HOME/.config/harness/config.toml`. There is no workspace/project config.
Precedence is **flag > config > built-in default**; clap's displayed defaults
never mask a config entry. `--config PATH` selects another user file, and
`--no-config` ignores it. They are mutually exclusive global flags.

```toml
provider = "openai"
auth = "codex"
model = "gpt-6.1-sol"
# summary_model = "gpt-6.1-sol" # enables automatic compaction
compaction_strategy = "observation_masking"
data_root = "/absolute/private/harness-state"
# Ask only; the TUI retains explicit approval keys and its existing switch policy:
switch_policy = "reject"
approve_read_only = false
```

The six common keys apply to both clients; `switch_policy` and
`approve_read_only` apply to **ask only**, and may coexist harmlessly in a shared
file used by the TUI. Values use the same parsers as their flags. No environment
interpolation or tilde expansion occurs. Config selections count like flags in
all existing continuation checks (including `auth_not_applicable`, model
switching and `session_policy_mismatch`); no persisted field or policy rule is
added. To keep a saved model/strategy without an explicit override, leave the
corresponding config key and flag absent. Summary policy must still match.

```sh
harness config path
harness config show
harness --config /path/to/user.toml config show --model override-model
harness --no-config ask --workspace "$PWD" "Explain this project"
```

`path` prints the resolved path and whether it exists. `show` prints every
key's effective value and source (`flag`, `config`, `default`); ask-only keys
are labelled. It does not load credentials or open storage. A missing default
file is normal; missing explicit, unreadable, over-64-KiB, non-UTF-8, malformed,
unknown-key and wrong-type files exit 2 with content-free `config_invalid`
diagnostics. Credential-like keys (`api_key`, `*_key`, `token`, `secret`,
`password`) are refused with `config_credential_refused`; put credentials only
in the existing environment variables or Codex login, never in TOML.

## Recorded instructions

At creation both clients record, in order:

1. Built-in factual system prose naming the workspace and available tools.
2. Optional user `$XDG_CONFIG_HOME/harness/AGENTS.md` (or
   `$HOME/.config/harness/AGENTS.md`) at developer level, framed
   `User instructions from ~/.config/harness/AGENTS.md:` (real path, HOME
   abbreviated). This path is independent of `--config` and `--no-config`.
3. Root `AGENTS.md`, framed `Project instructions from AGENTS.md:`.
4. Nested files, sorted lexicographically, framed for example
   `Project instructions from crates/cli/AGENTS.md (apply to files under crates/cli/):`.

User text uses std fs; project text and directory discovery are descriptor-rooted
with strict openat2 resolution and no symlink following. Dot directories,
`target`, `node_modules`, `dist`, `build`, `vendor` are skipped. The walk visits
at most 4,000 directories including the root, down to depth 8 (files at that
depth are included), warning once per reached limit. Files must be UTF-8 and no
larger than 64 KiB. Unsafe/unreadable/oversized files are skipped with
content-free warnings. User plus project content is capped at 256 KiB; files
that would overflow it are skipped whole, never truncated, with one total-cap
warning. Later smaller files can still fit. The built-in prose/framing do not
count against that content cap. No file is re-read on continuation; edits affect
new sessions only. See [ADR 0022](decisions/0022-recorded-model-instructions.md).

## Session catalog

```sh
harness sessions [list] [--data-root DIR]
```

Prints the [read-only catalog](session-storage.md#read-only-session-catalog) as
a pretty JSON array, newest committed activity first, with IDs breaking ties.
The default root matches `ask`; nothing is created or repaired. Missing storage
prints `[]`. Busy and broken sessions remain listed with status and a short
reason, and unavailable metadata is null. Titles are first-user-message text,
first line, at most 120 Unicode scalar values. This is a linear journal scan,
not a persisted index. A live session remains available through its handle;
listing does not wait for its writer lock. The bare command and `list` print the
same bytes.

### Session export, deletion and collection

```sh
harness sessions export <ID> --format markdown|journal --output PATH [--all-branches]
harness sessions delete <ID> [--yes]
harness sessions gc [--dry-run] [--yes]
```

`export --format markdown` writes a deterministic readable transcript of the
active branch, or of every branch with `--all-branches`; `--format journal`
creates a directory that is itself a data root, with a byte copy of the journal
and every artifact it references, verified while copying. Both are read-only
under the shared catalog lock and refuse an existing output path
(`export_exists`). `delete` takes the exclusive writer lock, removes only the
journal and says that `harness sessions gc` reclaims artifact space; it is
irreversible and needs `--yes` outside a terminal. `gc` removes artifacts that no
readable journal references and that are more than 24 hours old, refuses the
whole run if any journal cannot be validated (`gc_journal_unreadable`), and lists
candidates with `--dry-run`. Exit status is 0 on success, 2 for a refusal or a
missing confirmation, and 1 for a storage failure; refusals are stable codes on
stderr. See
[session storage](session-storage.md#export-deletion-and-artifact-collection)
and [ADR 0030](decisions/0030-session-export-deletion-and-gc.md).

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

## Repository policy

The GitHub repository is public. Its settings, which are not in the tree, are
recorded here:

- `main` has an active ruleset with no bypass actors. It cannot be deleted or
  force-pushed, its history must be linear, and the two CI checks `Rust
  workspace` and `Host sandbox security (S02)` (from GitHub Actions) must pass
  on the exact commit, which must already contain `main`. Land work by pushing
  a branch, waiting for green CI on its head, then fast-forwarding `main` to
  that SHA or rebase-merging its pull request. A push of a commit without
  passing checks is rejected. Rebase is the only pull request merge method.
- Merged head branches are deleted automatically. Branches finished without a
  pull request are deleted when they land.
- Only repository administrators can create `v*` release tags; once created,
  such tags cannot be moved or deleted.
- CI also runs weekly on `main` and on manual dispatch. Workflow actions must be
  GitHub-owned and pinned to a full commit SHA. Workflow runs from outside
  contributors' pull requests wait for maintainer approval. The default workflow
  token is read-only.
- Secret scanning with push protection, Dependabot alerts and security updates,
  CodeQL analysis of the workflows, and private vulnerability reporting are on.
  Dependabot proposes weekly updates for the pinned actions only
  (`.github/dependabot.yml`). See `SECURITY.md` for how to report a vulnerability.

## Releases

The release pipeline is defined in `.github/workflows/release.yml` and
[ADR 0029](decisions/0029-release-pipeline.md). It ships prebuilt static Linux
musl binaries for x86_64 and aarch64, not crates.io packages. The native runners
are pinned to `ubuntu-24.04` and `ubuntu-24.04-arm`; apt `musl-tools` supplies
the native musl C compiler for ring. No OpenSSL or aws-lc occurs in the locked
normal/build dependency tree. Rust's target libraries complete static linking.

The distributable profile is thin LTO, one codegen unit, stripped symbols and
`panic = "unwind"`. Inspection of `TerminalGuard` in `tui/runtime.rs` confirms
both the pre-diagnostic restoration hook and its `Drop` cleanup; the existing
feature-enabled PTY panic test runs in `check.sh`. Do not change this to abort.

### Local candidate measurement (M7-07)

On this aarch64 Linux host with Rust 1.99.1, the **only** local release build was:

```sh
CARGO_NET_OFFLINE=true cargo build --release --locked -p harness --bin harness \
  --target aarch64-unknown-linux-musl
```

It completed in **266 seconds** (Cargo: 4m 26s), with a binary of
**10,427,912 bytes**. `file` reports a stripped, statically linked AArch64 ELF;
`ldd` reports `not a dynamic executable`, and `readelf -d` finds no dynamic
section. Smoke checks passed: `harness 0.1.0`, `--help`, and `doctor --help`.
Before-profile and aarch64-gnu baselines are **unavailable**: decision 15 limits
local validation to this one musl build. No comparative size/time delta is
claimed. Release dependency/incremental artifacts and test archives are removed
after measuring; only this worktree's `target/` is used.

### Accepted dry-run evidence

Both native targets passed after default-branch registration:
[41625756606](https://github.com/Apagafuegos/project-harness/actions/runs/41625756606)
at `9223c52c0ea1fd98d26f006009fb32b7d78e0adc`, then
[41628110546](https://github.com/Apagafuegos/project-harness/actions/runs/41628110546)
at `305305b35601f41200c93dc2be3dfd8a43b20981`. In the latter, version,
`verify / Rust workspace`, `verify / Host sandbox security (S02)`, both native
musl builds and package succeeded; publish was skipped. Static-link, version/
help/doctor-help and twice-packaged byte comparisons passed for each target,
and aggregation produced sorted SHA256SUMS. These predate audit notices and
the mandatory guide, and are not final-asset or publication evidence.

### Release operations

The v0.1 candidate already has workspace version 0.1.0; no version/dependency
bump is needed for its documentation acceptance. After all milestone acceptance,
the orchestrator integrates the reviewed candidate only after exact-head CI
passes. To cut a release (not part of M7-07 validation):

```sh
# Example only: use the agreed version, and only after acceptance.
git tag -a v0.1.0 -m 'Release v0.1.0'
git push origin v0.1.0
```

`v0.1.0` was cut this way at `b51a309` after dispatch 41747234468 verified both
targets; publish run 41748617731 created the release.

A tag must exactly equal `v` plus the workspace version. The version job refuses
mismatches before building; `verify` reuses both ordinary CI jobs without
forking their provisioning or gates. The build matrix compiles offline, checks
static linking, runs version/help/doctor-help, and packages twice to prove
byte-identical tarballs. The package job generates `SHA256SUMS` and uploads the
complete asset set. Only a tag-push publish job can write repository contents;
it uses preinstalled `gh release create --verify-tag` and never creates a tag.
Dispatch is always non-publishing, even if `dry_run=false` or the ref is a tag:

```sh
gh workflow run release.yml --ref m7-release-pipeline -f dry_run=true
```

For a newly introduced workflow, GitHub may require default-branch registration
first. In that case the implementer hands off local and ordinary exact-head CI
results with dispatch **pending**. Under decision 15 the orchestrator integrates
the exact reviewed SHA after review, then dispatches and verifies both targets
and artifacts **before accepting M7-07**; registration creates no tag or Release.
If the native ARM job is unavailable or queues longer than 20 minutes, arrange
and prove the apt-only cross-compile fallback described in the brief before
acceptance. No unproven fallback or reduced target set is accepted.

Archives require `harness`, both licenses, README, CHANGELOG,
`docs/user-guide.md` and the generated `third-party-notices/` beneath
`harness-VERSION-TARGET/`. Missing/empty guide refuses packaging with exit 2;
`test-release.sh` tests guide bytes and an isolated missing/empty-guide fixture.
Packaging uses sorted GNU tar entries,
normalized modes/ownership, commit-time `SOURCE_DATE_EPOCH`, and `gzip -n`.
For the same binary/documents/epoch/tool versions, the tarballs are reproducible;
this does not claim cross-toolchain binary reproducibility. To package locally:

```sh
export SOURCE_DATE_EPOCH=$(git show -s --format=%ct HEAD)
./scripts/package-release.sh 0.1.0 aarch64-unknown-linux-musl \
  target/aarch64-unknown-linux-musl/release/harness target/release-assets
```

Download `SHA256SUMS` and both archives from the same Release, then verify:

```sh
sha256sum -c SHA256SUMS
tar tzvf harness-0.1.0-aarch64-unknown-linux-musl.tar.gz
```

Dry-run artifacts expire after one day. After inspection, delete each run's
artifacts with `gh api -X DELETE repos/Apagafuegos/project-harness/actions/artifacts/ID`
to limit Actions storage usage. Build/smoke network is restricted;
checkout, provisioning, artifact transfers and publishing necessarily use the
network. Never supply credentials to offline tests or tarballs.

## Secrets and runtime data

Ordinary tests/examples require no credentials and never read the real Codex
login. The live CLI reads the explicitly selected API-key environment variable
or the read-only Codex login; it does not load `.env` files. See
[providers](providers.md) for configuration and separate opt-in live checks.
Keep credentials out of Git, fixtures, logs, session entries, context snapshots,
and tool subprocess environments. Tool environment inheritance is allowlisted.

The library and `harness ask` create private state roots, checksummed journals
and content-addressed artifacts. `harness context` only reads an existing one. See [session storage](session-storage.md) for API usage and limits.
Session export and journal-only deletion are implemented, including TUI keys;
explicit GC reclaims old unreferenced artifacts under ADR 0030. Automatic
retention remains future work. See the [user guide](user-guide.md).

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
