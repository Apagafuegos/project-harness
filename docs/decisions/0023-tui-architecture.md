# 0023: TUI architecture — the pure model and the `Action` boundary

Status: Parts 1 and 2 implemented for Milestone 5 steps 02 and 03; Parts 3 to 5
implemented for Milestone 5 steps 04 and 05 and Milestone 6 step 03; Parts 6 and
7 implemented for Milestone 7 steps 05 and 06.

Date: 2026-10-31

## Decision and boundary

`harness::tui` owns the client's screen state: the session and workspace it was
opened for, the current `ClientView`, a rendered transcript, a multi-line prompt
editor, scroll position, one modal, a status line and a last-error banner.
`App::on_subscription` reduces `SubscriptionItem`s; `App::on_key` turns one
`KeyEvent` into `Vec<Action>`; `App::render` draws into a Ratatui `Frame`. The
module has no terminal access, no Tokio runtime and no `std::process`, and no
filesystem, network or clock use beyond generating a `CommandId` for a submitted
turn. It never decides an approval, builds a provider request, authorizes or
executes a tool, or reads an artifact.

The shell (m5-03) owns the terminal guard, the crossterm event stream, the
`SessionHandle`, command submission, rejection reporting and the durable
cancellation before exit. The boundary is one-way in each direction: the shell
pushes subscription items and key events in, and carries `Action`s out. A
`Reset` is the only accepted starting point, because events cannot be reduced
without a view.

## Dependencies

`harness` (the existing CLI crate) adds three direct dependencies. No new crate
is created: the important boundary, client versus engine, is already the crate
boundary.

| Crate | Version | Why |
| --- | --- | --- |
| `ratatui` | 0.30, `default-features = false`, features `crossterm` and `layout-cache` | The selected TUI. Only the crossterm backend and the layout cache are enabled; `all-widgets`, `macros` and `underline-color` are off because the client doesn't use them, which keeps 57 lockfile entries out. `TestBackend`, `Frame` and `Terminal` come from `ratatui-core`. |
| `crossterm` | 0.29, `default-features = false`, `events` | Key-event types only. Ratatui's default backend already depends on the same 0.29 line, so there is one crossterm in the graph; the direct declaration makes the key types explicit instead of relying on a re-export. No terminal I/O is used here. |
| `pulldown-cmark` | 0.13, `default-features = false` | Minimal CommonMark parsing (decision 4). The default `html` and `getopts` features are unneeded; nothing is escaped to HTML and nothing is highlighted. |

Added to `[workspace.dependencies]` for version consistency with every other
dependency, but referenced only by `crates/cli`. `cargo tree` shows no
`ratatui`, `crossterm` or `pulldown-cmark` in `harness-core` or
`harness-providers`. `Cargo.lock` gains 51 package entries (232 → 283); they
are ratatui's widget/geometry surface, crossterm's event dependencies and the
Markdown parser, all normal dependencies rather than new workspace crates.

`harness::text` now holds the two single-line helpers `ask` already used
(control characters become spaces; bounded text gains an ellipsis). `ask`
imports them unchanged, so headless output and its tests are unaffected; the TUI
uses the same bounding for tool arguments and result text.

## Subscription reduction

`App::on_subscription` is the only way state enters from the engine:

- `Reset(client)` replaces the entire view. The transcript is recomputed from
  `snapshot.ancestry(snapshot.head())` plus `view.partial`; it is never appended
  to or patched. Lag, an epoch change, a sequence gap and the initial view are
  all resets, so they take the same path. Scroll returns to the tail and
  ephemeral notices are dropped, because the reset is authoritative.
- `Event(event)` is applied by the same `ClientView` reducer the engine and
  `Subscription` use. Only `Applied` mutates state; `Duplicate` and
  `NeedsResync` are ignored. A gap never corrupts the transcript because the
  shell will send a new reset.
- Ephemeral, non-durable facts that belong in the log (diagnostics, faults,
  failed compaction attempts, recovery closure, a built-context estimate) are
  kept only in the app. A reset starts them empty; durable turn outcomes,
  approvals and compaction summaries are rebuilt from the snapshot.
- The modal follows the view: a pending approval opens and binds the approval
  modal; `AwaitingResolution` with a non-empty list opens the resolution modal;
  otherwise it closes, except for a confirm-quit dialog that is still waiting.
- The Ctrl-C busy period ends when the view has no active turn, so a second
  Ctrl-C quits only within the same turn.

## `Action` boundary

```rust
enum Action {
    Command(harness_core::runtime::Command),
    Quit { cancel: Option<TurnId> },
}
```

`Command` is submitted verbatim; the app never inspects a reply. `Quit` with a
turn names the turn the shell must cancel durably (`Cancel` and wait for
`Accepted::Cancelling`, or observe its durable finish) before restoring the
terminal and exiting. `Quit` with no turn exits immediately. Command rejection is
reported to the app through `App::set_error`, which keeps the modal open and
shows a bounded banner; there is no automatic retry.

The app itself never writes a `StartTurn` `CommandId` into durable state, since
it produces no state; a rejected command is the shell's report.

## Key map

Normal editing (no modal):

| Key | Effect |
| --- | --- |
| Enter | `Command(StartTurn { content: [Text(prompt)] })` only when the view is durably `Idle` with no active turn and the prompt is non-empty; clears the prompt. Otherwise nothing. |
| Shift+Enter, Alt+Enter | Insert a newline. |
| Esc | `Command(Cancel { turn_id })` when a turn is active; otherwise nothing. |
| Ctrl-C | With an active turn: first press `Command(Cancel)`, armed for the busy period; a second press `Quit { cancel: Some(turn) }`. With no active turn: opens the confirm-quit modal. |
| Ctrl-D | On an empty prompt `Quit { cancel: active_turn }`; with text, nothing (Ctrl-D never deletes). |
| PgUp / PgDn | Scroll the transcript by one viewport. |
| Ctrl-Up / Ctrl-Down | Scroll one line. |
| Up / Down, Left / Right, Home / End | Move the prompt cursor; Up/Down keep the column across logical lines. |
| Backspace / Delete | Delete before / at the cursor. |
| Ctrl-R | Toggle collapsed and expanded reasoning summaries. |
| Other characters | Insert into the prompt when not a control character. |

Approval modal (captures every key while the view has the ticket):

| Key | Effect |
| --- | --- |
| `a` | `SubmitApproval { decision: Allow, remember: false }` |
| `d` | `SubmitApproval { decision: Deny, remember: false }` |
| `r` | `SubmitApproval { decision: Allow, remember: true }` |
| Esc | `SubmitApproval { decision: Deny, remember: false }` — dismissing denies |

The `approval_id` and `plan_digest` are read from the view's pending ticket, so
the answer is bound to its `call`. Every listed key produces exactly one action;
all other keys are ignored rather than risk a second answer.

Uncertain-invocation modal:

| Key | Effect |
| --- | --- |
| Up / Down | Select one of the view's awaiting invocations. |
| Left / Right (and BackTab / Tab) | Cycle `AcknowledgedUnknown`, `ObservedSucceeded`, `ObservedFailed`. |
| Characters / Backspace | Edit the user's statement. |
| Enter | `ResolveUncertain { invocation_id, resolution }`; `Observed*` needs a non-empty statement. |
| Esc | Nothing: there is no safe default statement. |

Confirm-quit modal: `y`, Enter or Ctrl-C quits; `n` or Esc cancels.

## Transcript projection

The projection is width-independent rows (`Line<'static>`). It walks the
snapshot branch in conversation order:

- **User text** as plain bounded lines; media as bracketed placeholders.
- **Assistant text** as minimal Markdown with `pulldown-cmark`: headings,
  emphasis/strong, ordered and unordered lists, block quotes, inline code
  (styled, no backticks) and fenced code as a distinct bordered block with no
  syntax highlighting. Raw HTML is shown as inert text.
- **Reasoning summaries** as one bounded line with a line count by default;
  Ctrl-R expands every line. Raw reasoning is never in the event stream and is
  never rendered.
- **Tool calls** as the name plus bounded single-line JSON arguments, using the
  same bounding as `ask`.
- **Tool results** as status and code, up to six bounded text lines, and notes
  for truncated output and captured artifacts.
- **Approvals** as a request row after the proposing call and the durable
  allowed/denied outcome from `authorization_decided`.
- **Compaction summaries** as a bordered, distinct block with strategy, covered
  entry count, model and bounded summary lines.
- **Model switches** as the durable target and portability policy (`reject`,
  `omit optional`, `convert supported`). The live per-part `PreviewReport` is
  available to a later switch dialog, not reconstructed after a reset.
- **Turn outcomes** (`completed`, `cancelled`, `failed`/`interrupted` with the
  stable code) after the commit that finished the turn, or after the branch when
  the finish commit carried no entry.
- **Diagnostics and faults** as bounded, code-only notices; a fault cause's I/O
  text is classified, never copied. A `Faulted` phase always shows a fault row.

The live `PartialResponse` is appended as `assistant (streaming …)` followed by
its blocks in index order; partial tool calls are marked until `ready`. A reset
rebuilds this section from the partial in the new view, so resyncing mid-stream
neither loses observed text nor duplicates it.

**Redaction.** `ProviderState` appears only as its schema and byte count — the
payload is not part of `RuntimeEvent` or `RuntimeView`, so no renderer can print
it. Reasoning summaries are the provider-designated displayable subset. The app
never reads artifacts, credentials or journal bytes. The sentinel test runs a
real session whose opaque payload is a sentinel and asserts it is absent from
80×24, 20×5 and expanded-reasoning buffers.

## Rendering

The vertical layout is transcript (all remaining rows), prompt (wrapped, one to
six rows), an optional error banner, and a one-row reversed status bar. At tiny
sizes the panes saturate; a zero-height pane draws nothing, and modals are
clipped to the available area, so 20×5 renders without panic.

Rows are wrapped by display-cell width before they reach `Paragraph`, which
also makes the scroll offset a count of visible physical lines. `scroll == 0`
follows the tail; scrolling up preserves the offset and the render clamps it to
the content. The prompt shows a visible cursor block and grows to six wrapped
rows. The status bar shows the workspace name, the branch's current model (or
the streaming model), the phase, the last usage (live partial first, then the
most recent durable response) and the built-context estimate. The context limit
comes from the sourced catalog entry the client passed in at construction; after
a switch the limit is shown as unknown until the shell supplies another catalog.

Modals are a centered, cleared rectangle with a bordered title: the approval
ticket (tool, bounded arguments, plan digest, keys), the uncertain invocation
(tool, invocation prefix, advisory hint, choices, statement) or the quit
confirmation.

## Evidence and consequences

`crates/cli/tests/tui.rs` records real `SubscriptionItem`s from an in-process
`SessionHandle` with an injected `ScriptedProvider`, real JSONL storage and real
file tools, then reduces and renders them synchronously with `TestBackend`; no
snapshot crate is used. Thirteen tests cover a streamed Markdown answer with
usage, the approval modal and all four answers, a denied tool, cancellation, a
failed turn with `rate_limit`, a committed compaction summary, a model switch, a
mid-stream reset (the rebuilt transcript must equal its pre-reset form), narrow
and wide layouts, every decision-9 key, diagnostics and a fault with the error
banner, the uncertain-resolution modal and the opaque-state sentinel.

The uncertain-resolution fixture attaches an `awaiting` entry to a view read
from the completed real session: a resolution requires a process death between
the execution intent and its result, which an in-process test cannot produce
without an engine change. The snapshot, call identity and turn are real; only
the waiting state is attached. The modal logic depends only on that view.

## Part 2: terminal lifecycle and asynchronous shell (m5-03)

Plain `harness` is the interactive entry point, with the canonical cwd as its
workspace. It takes the six shared provider/session options and assembles the
session only through `wiring::open_session`; `ask` remains unchanged. There is
no TUI subcommand or read-only auto-approval toggle. The earlier Part 1 closing
paragraph's planned toggle was not an implemented feature; the settled M5
approval decision and step-03 brief explicitly exclude it.

`TerminalGuard` is the one terminal owner. It refuses nonterminal stdin **or**
stdout before mutation, installs a once-only panic hook, then enables raw mode,
the alternate screen and bracketed paste. A guard already exists during setup,
so `?` cleans up partial setup as well as later terminal errors. Restoration
first disables raw mode, then disables paste, leaves the alternate screen,
shows the cursor and flushes. An atomic active flag makes repeated restoration
harmless. The panic hook restores before invoking the previous hook, preserving
panic diagnostics on the main screen; it does not attempt to replace hooks
while unwinding. No process-exit path is used inside the TUI.

The process runs one current-thread Tokio runtime. Storage and tool execution
already use their engine-owned threads; terminal drawing and input reduction
are short synchronous work. Adding another executor thread would provide no
new ownership boundary. The shell's one `select!` polls crossterm `EventStream`,
the subscription, a `FuturesUnordered` of command replies, Unix signals and a
16 ms render tick. `EventStream` is never mixed with blocking `event::read` or
`poll`. crossterm's `event-stream` and `bracketed-paste` features are enabled;
workspace `futures-util` and `async-stream` are reused, not a new runtime or
transport. Ratatui 0.30's backend error is generic (TestBackend is Infallible,
CrosstermBackend uses io::Error), so the loop classifies backend errors instead
of requiring every backend to use io::Error.

`drive` injects a Ratatui terminal, event/subscription/signal streams and a typed
`Commands` endpoint. Each command's owned future is polled in the select, never
awaited inline. Accepted replies clear the error banner; rejected replies show
bounded classifications without provider payloads. Initial input waits for the
first reset, avoiding lost keystrokes before a view exists. Every subscription
item goes to App. Resets replace rather than append transcript state. Dirty
frames draw on ticks; clean ticks do not draw. Resize explicitly resizes and
draws immediately. Paste calls `App::on_paste` once, neutralizes unsafe control
characters, and cannot accept an approval or send a turn. The merged model
catalog is supplied to App for current-model budget lookup, including after a
switch. A continuation's explicit switch waits for recovery to reach idle,
ignores identical models, and runs through the same nonblocking command path.
Refusals keep the saved model and show a banner; no optional state is omitted.

On quit, keys stop submitting new work. Pending starts are drained, and a start
that wins the race is cancelled. `Cancel` acknowledgement is **not** treated as
a turn finish: the loop waits for the subscription's durable closure (or the
fault/resolution boundary), then the caller always invokes `shutdown`, including
on loop errors. Shutdown drains tool cleanup and storage before returning.
The provider cancellation grace belongs to the engine; the UI adds no silent
retry or independent cancellation timeout. SIGINT is a Ctrl-C action. SIGTERM
and SIGHUP restore immediately and stop drawing, then follow durable quit and
shutdown. Signals are installed before storage/terminal setup so setup-time
signals are retained. The guard can restore again on drop safely.

### Offline PTY and crash seams

`test-provider` is off by default. Only `all(feature = "test-provider",
debug_assertions)` compiles `HARNESS_TUI_TEST_PROVIDER` (scripted pong or a
stalled stream) and `HARNESS_TUI_TEST_PANIC`. Release binaries ignore these
variables even if someone explicitly enables the feature. No network or
credentials are needed for ordinary tests. `scripts/check.sh` runs the ordinary
workspace tests and separately the feature-enabled PTY target.

`portable-pty` 0.9 is a dev-dependency, shared with the opt-in live example.
It encapsulates openpty, controlling-terminal setup, safe child spawning and
termios queries; this avoids any unsafe code or shell/Python PTY toolchain in
the repository. The real-binary tests are in their own integration target so
spawn cannot overlap another target's journal-reopen assertions. They compare
complete termios before/after and check enter/leave-alternate, enable/disable-
paste and show-cursor sequences. The panic assertion additionally requires
restoration **before** the panic message (drop-only cleanup is insufficient).
SIGTERM is sent during a stalled inference, and journal reopen must find a
cancelled turn. Non-TTY coverage includes genuine terminal stdin with piped
stdout and verifies unchanged termios, exit 2 and no escape sequences.

`tui_runtime` uses ScriptedProvider, real storage, scripted event streams and
TestBackend for streaming/paste, approvals (allow/remember/deny/dismiss), Esc,
double Ctrl-C durable quit, Ctrl-D, slow-command ticks, rejection banners,
resize, faulted resets and lag reset deduplication. `tui_recovery` follows the
M3 current-executable child pattern: a test executor holds after the durable
write intent, the parent SIGKILLs the child, then the loop resolves the reopened
uncertainty and starts another turn. No handle drop is called a crash and no
uncertain write is replayed. The test executor is fixture-only, never an
alternate production host assembly.

`scripts/live-check.sh tui` builds the ordinary binary and drives it through
the same PTY verifier with a real Codex subscription, the fixed pong prompt and
Ctrl-D. It verifies answer, exit and restoration, prints only sanitized evidence,
and never retries rate limits. It is absent from check.sh and CI.

### Small App amendments

The model adds catalog injection/current-model budget lookup, one atomic paste
method, a generic fault banner for faulted resets, and the documented Enter key
in quit confirmation. State reduction, approvals, uncertainty and transcript
projection remain pure. No engine/provider code, journal format, transport or
workspace crate is added.

## Part 3: session, branch and model navigation (m5-04)

Navigation state, keys and rendering live in `tui/navigation.rs`; durable
handover and sequential navigation work live in `tui/runtime_nav.rs`. The small
App/loop hooks preserve the pure-model/thin-shell boundary. Ctrl-O, Ctrl-T and
Ctrl-L belong to navigation; Ctrl-K and the inspector/compaction action names
are untouched. `Action::{ListSessions, OpenSession(SessionId), NewSession}` are
shell-only; branch/model/edit actions use existing typed engine commands.

The session picker receives owned `DataRoot::sessions()` rows after a blocking
scan has finished and released every shared lock. The shell polls navigation
futures strictly in FIFO order, never concurrently. `SessionCommands` also
serializes commands, catalog scans and handover with a single-permit semaphore, checking a
fresh engine view under that gate before shutdown. Active turns, recovery,
uncertainty and faults refuse handover with stable reasons. Bad catalog rows
and other workspaces are refused before releasing the current writer. No
workspace switch, repair, tool retry or policy bypass is introduced.

Idle handover awaits `shutdown`, then calls `wiring::open_session` with the same
workspace, flags, provider and catalog. Only `session_locked` is retried, exactly
once after 30 ms, then reported as busy. If opening fails after shutdown, the
shell attempts to reopen the previous durable session and reports the original
failure. A replacement view resets the transcript and editor; subscriptions
follow the new handle and begin with the ordinary engine reset. Explicit
continuation overrides run through the same idle/recovery-aware switch path as
startup, never against the previous session's model. Quit drains outstanding
navigation as well as commands, then the caller shuts down the current handle.

The branch tree derives user nodes and turn heads from immutable snapshot
entries, ancestry and turns, including sibling branches. Ancestry paths order
the tree; indentation shows branch depth, `*` identifies the current branch,
and every turn names its model. Enter submits `SelectHead`. Editing pre-fills
only the user text and remembers its parent. Send submits `SelectHead(parent)`
before `StartTurn(edited)` and waits for acceptance; if the parent is already
selected, the redundant selection is omitted to avoid `head_unchanged`. A
refusal stops the sequence. No original entry is replaced or hidden.

The model dialog enumerates the wiring catalog through decision 14's read-only
`ModelCatalog::iter()` (ADR 0015 amendment). It always submits `Reject` first.
A `PortabilityLoss` reply supplies the classified report directly to App,
including entry IDs, parts, owners, schemas, byte counts and required
capabilities, never payloads or artifact reads. Optional omission needs a
separate `o` confirmation bound to that report's target. Required losses have
no omission action. Each attempt is still validated by core; stale reports
cannot authorize a now-required loss. Success uses the existing catalog-aware
status bar for the new model and budget.

Offline `tui_navigation` tests use real storage, `ScriptedProvider`, TestBackend
and the production loop. They cover catalog status/workspace refusals, durable
handover/back/new, writer contention and its one retry, delayed catalog/open
serialization, branch re-projection, sibling preservation, busy refusal,
reject-first reports, explicit omission with no foreign state in the next
recorded request, required incompatibilities and small/80×24 rendering. No
engine behavior, provider implementation, dependency or wiring helper is added.

## Part 4 amendment: inspector and compaction (m5-05)

`inspector.rs` owns the pure panel state, rendering, input and typed results.
The existing approval/recovery/quit modals retain precedence. Ctrl-K toggles
the inspector; it captures paste and ordinary editor keys while open. PgUp/PgDn
scrolls the entire report/proposal. Ctrl-C/D keep their existing global behavior.
No m5-04 picker keys or action names are used.

The only new actions are `InspectContext { head, model_override }` and
`InspectPreview(EntryId)`; compaction uses the existing `Command` action.
`App` emits `InspectContext` when its projection identity changes; a manual
proposal arrives in the `CompactionPreviewed` reply, so the App never needs
`InspectPreview`. That variant and its `Commands::inspect_preview` method are
reserved for clients that retain an ID across resets and are covered by a
direct test asserting a live read and a stale read with no new generation.
The shell's injectable `Commands` endpoint has read-only inspection methods.
`inspector_shell.rs` holds the futures and mechanical delivery; the loop selects
inspection replies alongside commands, keys, subscriptions and ticks, never
awaiting an inspection inline. One request is scheduled per changed published
session/sequence/head/model/override identity while open (not per streamed
text delta). Results are checked against the current identity before display;
a response for a former head/session/model cannot overwrite the new report.
These reads and the model chooser neither write storage nor call a provider.

`m` chooses from the **merged wiring catalog** via `ModelCatalog::iter()`
(decision 14, the separately landed ADR 0015 accessor amendment). The report
uses `model_override`, explicitly labels the preview and says nothing switched.
Selecting “Session model” removes the override. There is no `SwitchModel` action
in this feature. Neither provider lists nor model limits are duplicated here.

Reports show labelled estimated usage/headroom, configured reserves/limits,
overflow/recommendation/unknown flags, ordered included items, part exclusions,
estimator identity, configuration and projection fingerprints. Text previews
use an explicit bounded displayable-content allowlist; native state schemas,
continuations, portability metadata and opaque payloads are **not** preview
content. Instructions use a recorded-instructions label (their text is not in
the report/view), rather than reading their artifact in the App. Tool calls are
labelled separately within assistant items. Opaque bytes remain bytes, never
estimated tokens. This is narrower than the existing transcript's schema/size
placeholder: the inspector never renders even a native schema.

`c` submits `PreviewCompaction`, shows progress and allows Esc cancellation.
The core retains selection, provider dispatch, independent summary model,
manifest construction, diff and validation authority. Replies supply manual
proposals; automatic preview events are informational only, never an Enter
commit target. The panel shows exact summary text, covered/superseded IDs,
source/configuration fingerprints and manifest checksums/indices, then the
core's full category-token diff and fits-budget result. Generating a proposal
changes no journal bytes. A sufficiently large rendered preview frame arms
Enter; mere arrival or a clipped tiny pane does not. Enter submits the exact
preview ID once. Once submitted, commit cannot be cancelled by closing the UI.
Esc/Ctrl-K discards an unsubmitted proposal or cancels generation; late answers
cannot revive a closed/cancelled proposal. Quit cancels outstanding generation
before draining pending replies, avoiding a stalled-summary shutdown deadlock.

`stale_preview` clears the displayed proposal, names the changed field and
offers `c` for a new one; the UI never silently retries. NotEligible and all
other compaction errors use the core's stable code. Automatic preview/commit
notifications appear in the transcript; summary durability already appears
through its snapshot projection. An open inspector refreshes after that commit.
Compaction failures also set the banner. `context_budget_exceeded` retains an
ineligibility reason, or suggests compaction when the rejection has no such
reason. The associated failure event cannot erase this richer reply diagnostic.

### Offline evidence and mutations

`crates/cli/tests/tui_inspector.rs` adds 16 tests with real JSONL storage and
ScriptedProvider, including independent summary-model request verification,
80×24 and tiny TestBackend renders, ordered budget/item/exclusion/fingerprint
rows, non-switching model overrides, refresh after a committed turn, explicit
preview-first commitment, rendered summary/manifest/full diff, exact journal
bytes on preview cancellation and generation cancellation, quit during a
stalled summary, stale head with retry guidance, zero-provider-call eligibility,
automatic compaction visibility, budget refusals with and without eligibility,
slow-inspection key/resize/tick responsiveness, `inspect_context_preview`
returning a live proposal and then `stale_preview` without regenerating, and an
opaque payload/schema sentinel under both ordinary and override reports.

Each mutation was applied alone, the named test failed (exit 101), and it was
reverted before the complete gate:

| Mutation | Test | Failure |
| --- | --- | --- |
| Await `inspect_context` inline in the loop | `slow_inspection_never_blocks_keys_resize_or_render_ticks` | loop deadline expired before keys/renders could proceed |
| Remove the shown-preview Enter guard | `preview_first_requires_a_render_and_explicit_enter` | arrival alone incorrectly authorized commit |
| Render native schema as the item preview | `opaque_schema_payload_and_portability_sentinel_never_render_in_panel` | opaque state leaked |
| Ignore `StalePreview` | `loop_stale_preview_names_the_changed_field_and_offers_retry` | no changed-field/retry diagnostic; old proposal remained shown |

No engine/provider behavior, persistence schema, dependency, toolchain or
transport was changed by this step. The only core accessor in its ancestry is
the orchestrator's separately reviewed decision-14 change.

## Part 5: context controls in the branch tree (m6-03)

The client can now pin, exclude and reset whole turns (decision
[0025](0025-context-controls.md)). `p` and `x` live in the branch tree, not in
the prompt editor: the editor owns printable keys, and the tree already lists
the turns with a selection, so decision 9's key map is unchanged outside the
tree. `p` and `x` are active only while `Ctrl-T`'s screen is open; everywhere
else they are still prompt characters. No new action variant was needed: each
key emits exactly one `Action::Command(SetContextControl { turn_id, mode })`
and the app never inspects the reply.

**Effective modes are folded in the client.** `tui::turn_modes` walks the
selected ancestry's `context_control` entries and keeps the last mode per
target turn, which is decision 5's rule; a turn with no control in effect is
`default`. Core folds the same map for storage, the builder and the report, but
exposes no public accessor for it, and this step may not change core. The
client needs no journal bytes: entries are immutable and already in the
snapshot, and controls on sibling branches are not on the ancestry, so they
never apply. The fold is read at render time from the current snapshot, so a
reset or an applied event is enough to redraw every marker.

**Keys follow the current mode.** `p` pins a default turn and resets a pinned
one; `x` excludes a default turn and resets an excluded one. A pinned turn
becomes excluded with one `x` and an excluded turn becomes pinned with one `p`,
because both name a different mode than the effective one; the core refuses only
a control that would not change the mode (`context_control_noop`). Every key
sends one command and nothing else; a refusal is never retried.

**What the tree shows.** Each row carries its mode as a fixed-width ASCII
marker before the title: `[pin]`, `[excl]`, or six spaces, so titles stay
aligned. An emoji pin was rejected: `📌` renders two cells wide in most
terminals, is not in every font, and cannot be asserted at 20×5 the way plain
brackets can. Markers sit at the start of the row, before the title, because
the tree chunks long rows at the pane width.

**Refusals the app already knows.** A control is idle-only and needs a finished
turn, so the app checks exactly those two rules before sending, in the engine's
own order: a non-idle view (an active turn, recovery, a fault) is `busy`, and an
idle view whose target turn has no terminal outcome is
`context_control_unfinished_turn`. Both are shown as the engine would show them
(`command rejected: <code>`) and nothing is written. Every other rule stays the
engine's — `context_control_covered`, `context_control_not_ancestral` and
`context_control_noop` arrive from the core and appear verbatim in the banner.
The tree stops accepting keys until the reply (`navigation.waiting`), so a
double press cannot race the fold the second press computed.

**Transcript.** The durable one-line `Pinned turn …`, `Excluded turn …` and
`Reset turn …` markers from m6-01 are unchanged. An excluded turn also gets one
dim tag row, `── excluded turn (kept in history; never sent to the model) ──`,
and every row that turn owns is patched with `DIM`. Exclusion is a projection
choice (decision 7), so nothing is hidden or truncated: the tag and the dimming
are the only difference, and the text stays readable. Both the line style and
each span style are patched because `wrap_lines` keeps span styles when it
splits a row.

**Inspector.** The report already carried what the panel needed: included items
belonging to a pinned turn are labelled `pinned`, and excluded entries appear in
the exclusion list with `user_excluded`. The panel gains a
`Controls (effective on this branch)` section listing each non-default turn's
mode, turn prefix and the control entry that sets it, or `none`. No new
invalidation was needed: a control entry advances the head and the journal
sequence, and the existing request identity is
`(session, sequence, head, model)`, so an open panel refreshes on its own after
every control. A test keeps the inspector open across a pin to prove it.

**`harness control` was not added.** A scriptable subcommand would have to open
the session through `wiring::open_session` to get a `SessionHandle`, which needs
a provider and a full tool-host assembly; this step may edit neither `wiring.rs`
nor `providers`, and a stub provider or a sandbox re-probe inside a journal
append would be worse than the gap it fills. The m6-05 evaluation can drive the
same typed command in-process, as `crates/cli/tests/tui_controls.rs` does.
`harness ask` gained no flags.

### Evidence

`crates/cli/tests/tui_controls.rs` adds nine offline tests with real JSONL
storage and `ScriptedProvider`; no provider network, credential or clock is
used. Pure `App` buffers cover the 80×24 tree with `[pin]`/`[excl]`, its 20×5
form, the tagged and dimmed excluded transcript (asserted from cell modifiers,
with the untouched turn asserted not dimmed), and the full key mapping through
the real journal. The production loop covers a pin (journal control and
`pinned` in an open inspector), an exclusion (inspector `user_excluded` and a
following request whose serialized items lack the turn), a reset back to
`default` with both controls kept, `context_control_covered` verbatim with
byte-identical journal bytes after a committed compaction, a `busy` refusal on
the active turn with nothing written, and pinned-to-excluded in one control.

Each mutation was applied alone, the named test failed, and it was reverted
before the complete gate:

| Mutation | Test | Failure |
| --- | --- | --- |
| `p` on a pinned turn sends `pinned` again | `tree_keys_toggle_each_turns_effective_mode`, `loop_reset_returns_the_turn_to_default` | wrong command asserted; the core refuses the second pin as `context_control_noop` |
| Hide excluded turns from the transcript | `excluded_turn_is_tagged_and_dimmed_but_still_readable` | the tag and the turn's text were absent |
| Let the `App` write the journal directly | `tui_boundary` | `navigation.rs: pure model touches storage with DataRoot/JournalStore/StoreError` |
| Do not refresh the inspector after a control | `loop_pin_journals_the_control_and_the_inspector_shows_pinned` | no `pinned`/controls row in the panel |

The last mutation is why the ratchet grew a file-scoped storage rule: a
control is the first client key that could plausibly "just append" an entry, and
the previous scan would not have noticed a direct store write inside the pure
model. `tui_boundary` now bars `DataRoot`, `JournalStore`, `StoreError` and
`Timestamp` from `mod.rs`, `render.rs`, `transcript.rs`, `navigation.rs` and
`inspector.rs` only; the shell files keep their read-only catalog scan and
journal inspection, and that exception has its own assertion.

No engine, provider, persistence, dependency or toolchain change was made. The
only files touched are `crates/cli/src/tui/{mod,navigation,transcript,inspector}.rs`,
`crates/cli/tests/{tui_controls,tui_boundary}.rs`, and this ADR plus
`docs/development.md`.

## Part 6: UX foundations

M7-05 keeps the model/shell boundary and width-independent projection. This
part supersedes the earlier bare prompt, fixed dialog size, character wrapping,
perpetual render timer and completed-turn status rows. Markdown parsing,
highlighting, tool content and approval-preview content remain M7-06's scope.
The user's review/addendum and subsequent style refinement choose **minimal
chrome**: typography, role colours and spacing; no emoji or transcript box.
The focused prompt has a single accent top rule with its mode and a `>` marker,
not a rounded box. Streaming dims that rule. A dim header shows workspace,
short session identity and sibling count on forked histories. Header, footer,
then prompt rule disappear at small heights; the minimum stays 20×5.

### Theme, rows and layout

`theme.rs` owns all colours and modifier roles. The shell computes `Theme`
once per app and injects it; neither the model constructor nor drawing reads
the environment. Any nonempty `NO_COLOR` strips every foreground/background
colour; bold, dim, underline and reversed still communicate meaning. Only ANSI
colours are used. The full-width status row uses terminal-default colours plus
`REVERSED`, with workspace/provider/model on the left and phase/context estimate
and percentage on the right. Narrow layouts drop right-hand details first.
Usage is labelled `N in / N out` on the assistant label, not an ambiguous pair
on the status bar. User and assistant share a two-cell gutter (`▌`, or `|`
in ASCII mode), role-coloured bold labels, and the same body indentation.
Assistant metadata is dim and right-aligned by the width-dependent renderer.
Normally completed outcomes are omitted; cancelled/interrupted/failed outcomes
remain compact, stable-code-bearing lines with no blank gap before them.

The exposed view does not carry journal record timestamps. Durations therefore
come only from observed, injected ticks and are retained in memory per turn;
unobserved/reopened durations are unknown and omitted, never guessed or read
from storage. Session title is likewise not in this view, so the header uses
the allowed short ID. No schema or core accessor is added.

`unicode-segmentation` is the sole added dependency, already present transitively:
wrapping uses grapheme clusters and Ratatui's display-cell measurements, keeps
span/line styles, breaks at whitespace and hard-breaks overlong tokens. Leading
indent/list markers establish hanging indentation. The projection's existing
code-frame markers are a no-reflow hint: those rows hard-wrap with `↪`, never
word-reflow. Gutter spans repeat on physical continuation rows. The prompt uses
a separate hard-wrapped grapheme layout shared with `Frame::set_cursor_position`;
there is no cursor glyph in its text. Empty prompts show the dim placeholder
`Ask anything · F1 help`. Large bracketed pastes are atomic, fully editable,
and label the mode rule with `[pasted N lines]` above 20 lines.

### Injected time and follow mode

`UiAction::Tick(Duration)` is a TUI-local **input**, not an engine command or an
outgoing shell effect. The shell supplies monotonic elapsed time at about 10 Hz
only during active turns. The pure model never consults a clock. Active phases
show a spinner, elapsed seconds and streaming displayable-byte count;
`TERM=dumb` selects an ASCII spinner/gutter. Dirty rendering is coalesced to
16 ms; there is no perpetual idle timer. Pending command/navigation futures can
still service render responsiveness counters while work is outstanding.

At the bottom the viewport follows. While scrolled up, growth is added to the
bottom offset so the exposed physical rows remain fixed, and a dim new-lines
counter plus `End to follow` appears. Overflow has a percentage marker. Reset
returns to following. End follows only with an empty draft; Ctrl-End follows
without discarding a nonempty draft.

### Key table, editor and dialogs

`keys.rs` is both the input allowlist and the generated grouped help reference;
handlers receive only documented chords (ordinary printable text is a documented
family). An independent contract test covers the required table rows and control
chords, so deleting a binding cannot silently remove both its help and behavior.
State-specific hint footers retain the most useful keys. Every added key or
changed existing meaning is listed here:

| Key | Added meaning |
| --- | --- |
| F1 | Open help over the current screen; close help when open. |
| `?` with empty prompt | Open help; otherwise ordinary prompt text. |
| Up on first logical line | Recall previous prompt. |
| Down on last logical line | Recall next prompt, then restore the saved draft. |
| Ctrl-Left / Ctrl-Right | Move by whitespace-delimited word. |
| Ctrl-W | Delete the previous word. |
| Ctrl-U | Delete to the start of the logical line. |
| End on empty prompt / Ctrl-End | Follow output (Ctrl-End preserves a draft). |
| Esc idle, without a dialog | Dismiss the error banner; active-turn Esc still cancels. |
| Up/Down and PgUp/PgDn in help/dialogs | Scroll content; selection dialogs keep Up/Down selection and expose it. |

History is seeded from current-branch user messages plus prompts sent in this
process; it is memory-only and reset on session replacement. Navigation,
compaction, approval and uncertain-operation semantics remain unchanged. In an
approval, `a`, `d`, `r` and Esc still emit precisely the original bound answer;
paste and scrolling never answer. Help closes back to the underlying dialog.
A shared content-sized modal frame clamps to area minus two cells, scrolls
wrapped content, carries title/key hints and shows `more ↓`/`more ↑`. The
inspector's existing rendered-preview commitment guard still uses the exposed
content rectangle. Errors wrap to three rows plus an ellipsis and dismiss on
idle Esc or send. The empty session has a workspace/model/four-key welcome.

### Offline evidence

Pure TestBackend coverage adds word/style/grapheme/code wrapping, modal overflow,
NO_COLOR across transcript/chrome/dialogs, fake-clock activity, history including
branch seeding, word edits/large paste, generated help and paging, follow anchors,
error/welcome behavior, and the exact Hello conversation's gutters/metadata,
full-width status, placeholder/real cursor at 80×24 plus 20×5 and 200×60. Loop
coverage checks active-only ticks and idle quiescence. Existing approval,
inspector commitment, cancellation, navigation, PTY and boundary tests retain
their behavioral meaning. Handoff-only render captures are not shipped.

Each mutation was applied alone, failed the named test (exit 101), and reverted:

| Mutation | Test | Observed failure |
| --- | --- | --- |
| Character rather than word wrapping | `word_wrap_keeps_words_and_hanging_indent` | `gamma delt` / `a` instead of intact words |
| `Color::Gray` in status drawing | `no_color_covers_transcript_prompt_errors_status_and_all_dialog_frames` | Gray cell instead of Reset |
| Remove Ctrl-W binding | `help_table_covers_every_handled_key` | undocumented key: Ctrl-W |
| Consult system time in model | `activity_spinner_elapsed_and_stream_size_use_only_injected_clock` | real epoch seconds instead of fake 12s |

## Part 7: content rendering (m7-06)

This part supersedes Part 6's statement that Markdown parsing, highlighting,
tool content and approval-preview content were a later step. It keeps the pure
model, the width-independent projection and the M5 boundary: the client formats
what the view already carries and still builds no request, authorization or
storage call. No core or runtime type changed; every fact the new content needs
was already in `RuntimeView` (call name and arguments from the assistant
response, and the call's result status, code, exit code and metadata).

### Rows, marks and the ASCII fallback

Two width-independent projection markers let the renderer do width-dependent
work without the projection knowing the pane: a code row carries `CODE_ROW` and
hard-wraps with `↪`, and a rule row carries `RULE_ROW` and becomes exactly the
pane width. This replaces Part 6's text sniffing of `┌─ code` / `└─` frames, so
a quoted or nested row can no longer be mistaken for code.

Every decorative glyph is canonical Unicode in the projection, and `Theme::apply`
translates it in ASCII mode: `▌`/`│`→`|`, `•`/`◦`/`▪`→`-`/`*`/`+`, `─`→`-`,
`…`→`...`, `↪`→`>`, `✓`→`ok`, `✗`→`x`, `⊘`→`-`, `↑`/`↓`→`^`/`v`, `·`→`-`.
Chrome the projection does not own (header, hints, status, prompt rule, dialog
titles and footers, welcome text, help) goes through `Theme::ascii_ready`, and
the prompt rule uses an ASCII border set, so `TERM=dumb` cell contents are pure
ASCII. The theme gained five code roles (keyword/string/number/function/type)
plus `Plain`; all colours are the named 16 ANSI variants.

### Markdown

`markdown.rs` walks `pulldown-cmark` with `ENABLE_TABLES`,
`ENABLE_STRIKETHROUGH`, `ENABLE_TASKLISTS`, `ENABLE_FOOTNOTES` and `ENABLE_GFM`.
Headings are H1 bold+accent+underlined, H2 bold, H3+ bold+dim, each preceded by
one blank row except at the message start; paragraphs are separated by exactly
one blank row and trailing blanks are trimmed. Emphasis is italic, strong bold,
strikethrough crossed out, inline code a distinct dim style with no backticks,
and links are underlined with a dim ` (url)` suffix unless text equals URL.
Autolinks therefore stay underlined and URLs are never lost. Lists nest to any
depth with hanging indents, bullets `•`/`◦`/`▪` cycling by depth, ordered lists
keeping their start number, and `[x]`/`[ ]` task marks. Block quotes nest a dim
`│ ` gutter per level; rules are the full pane width; images render
`[image: alt]`; raw HTML stays literal; footnote references become `[^n]` and
their definitions are collected and appended once at the end.

**Tables.** The projection computes natural column widths from the header and
rows and honours the delimiter row's alignment, with a bold header and single
`─`/`┼` rule separators. A table that fits the pane keeps that shape. A table
wider than the pane **hard-wraps** through the ordinary row wrapper; the brief
offered hard-wrapping or a `header: value` fallback and this part chooses
hard-wrapping because it keeps one projection for every width and never drops a
cell. The alternative was rejected: a fallback would have to know the pane width
inside the projection, which the width-independent contract forbids.

### Code and highlighting

A fenced block is a dim language label (`code` when untagged) plus a dim `│ `
gutter over rows that never reflow. Highlighting uses `syntect` 5.3 with
`default-features = false` and features `default-syntaxes`, `parsing`,
`dump-load` and `regex-fancy`: bundled grammars, the pure-Rust `fancy-regex`
engine, no C regex, and no download. The syntect theme is built at runtime from
scope selectors whose foregrounds are **probe** colours; the highlighter maps a
syntect style back to a semantic `Role` and applies `Theme::style`, so the only
colours handed to Ratatui are the theme's ANSI variants. `NO_COLOR` and untagged
or unknown languages render plain. A block over 2,000 lines or 256 KiB renders
plain with one dim note. `syntect` is the only new dependency; `cargo tree`
shows it in `crates/cli` only, and `Cargo.lock` grows by the crate plus
`fancy-regex`, `bit-set`, `bit-vec`, `bincode`, `flate2`/`miniz_oxide`,
`regex-syntax`, `same-file`, `walkdir` and their small supports.

The release binary measured 18,745,256 bytes before this part and 19,705,032
bytes after (`cargo build --release -p harness`, Rust 1.99.1, the repository
profile), a 959,776-byte (+5.1%) increase that is almost entirely the bundled,
compressed syntax dump.

### Tool blocks

Each call is one compact header: status mark, name, summary, then the exit code
or failure code. Marks are `✓` succeeded, `✗` failed/invalid/timed out/
infrastructure, `…` running (no result yet), `?` uncertain, and `⊘` denied or
cancelled. Summaries are the command in code style for `bash`, the path and byte
range for `read`, the path and byte count for `write`, and the path plus a
red/green `-`/`+` diff for `edit` bounded to 12 rows with `… +N lines`; unknown
tools show their compact arguments. A failed header always keeps its code, and
the exit code wins for `bash`. Result bodies collapse to the first three lines
plus a dim `… +N lines` row and expand with Ctrl-E, one global switch that
rebuilds the projection exactly like Ctrl-R. Recorded approvals are one dim
`approval: allowed/denied/requested` row under the call.

Reasoning is dim italic behind `│ `, collapsed to one line until Ctrl-R. Model
errors are one error-styled row with the stable code and the bounded provider
message (the message is now shown, where Part 1 showed the code only).
Provider state stays one dim row naming schema, byte count and owner, rather
than disappearing behind the verbosity switch: the brief allowed either, and a
single dim row can never look like a missing result.

Part 6a is folded in: the approval body no longer repeats the key hints (the
dialog frame and the footer keep them), non-normal turn outcomes are drawn
inside the turn's `▌ ` gutter in warning (cancelled) or error style, and tool
rows sit at the body indent instead of the old double indent.

### Approval dialog

The dialog keeps decision 9's keys and `ApprovalId`/`plan_digest` binding. Its
body is human-readable: `bash` shows the command in code style, the resolved
working directory, the timeout and `network: denied`; `edit` shows the path and
the full diff, scrollable through the existing modal scroll; `write` shows the
path, byte count, new-file/overwrite state and the first ten lines; `read` shows
the path and byte range; any other tool pretty-prints its JSON arguments. `v`
toggles raw JSON arguments and answers nothing. The plan digest stays visible and
dim, and modal bodies now also go through `Theme::apply`, so `NO_COLOR` and
ASCII mode cover them.

### Projection cache

`Renderer` caches a completed entry's rows by entry identity plus the switches
and facts that can change them: the two toggles, the observed turn duration, and
each call's approval decision and result status/code/exit code. Cached rows are
cloned into the rebuilt transcript; only the live `PartialResponse` is parsed
again, so a streaming delta never re-parses earlier Markdown. The cache is pruned
to the current ancestry. `App::transcript_parses()` is the test hook counting
entries rendered from scratch.

### Offline evidence

`crates/cli/tests/tui.rs` adds eight pure-`App` render tests with real storage
and `ScriptedProvider`: a kitchen-sink GFM fixture compared to checked-in golden
text at 80 and 120 columns plus cell-modifier assertions for every inline style
and ANSI highlight; a `NO_COLOR` buffer with no colour and an ASCII buffer with
no non-ASCII cell; a user message with leading spaces and a 1,000-character line
wrapped but never truncated, and a fenced user block; tool headers and a
collapsed read body expanded with Ctrl-E plus an `edit` diff in both transcript
and dialog; the write approval preview and the `v` raw toggle (asserting the key
hints appear only in the frame and footer); an unclosed fence while streaming,
rendered as code and stable across frames; a cache test (repeated frames add no
parses, a new entry adds one, a toggle re-renders once); and the 2,100-line cap
note. Unit tests cover every `ToolStatus` mark and failure-code header, the
bounded diff, the three approval previews plus raw mode, the ANSI-only palette
and the ASCII glyph table.

Rendering-only state changed: `tui/transcript.rs` lost its inline Markdown
walker and gains `Renderer`; `tui/{markdown,highlight,tools}.rs` are new pure
modules, and `tui_boundary` now scans them as pure model files. Four existing
`tui.rs` tests changed expected *text* only, keeping their meaning: the
streamed Markdown answer (a styled heading instead of `# Result`, a dim `rust`
label instead of `┌─ code rust`), the approval and denied-tool tests
(`✓ write note.txt` / `⊘ write note.txt` instead of `tool result: succeeded` /
`denied`), and the failed turn (which now also shows the provider message).

Each mutation was applied alone, failed the named test, and was reverted:

| Mutation | Test | Observed failure |
| --- | --- | --- |
| Drop `ENABLE_TABLES` | `kitchen_sink_markdown_renders_the_same_projection_at_80_and_120_columns` | the table rows rendered as literal `|` paragraphs, so both goldens differed |
| Truncate user lines to 400 characters | `user_text_keeps_indentation_and_wraps_long_lines_without_truncating` | only 400 of the 1,000 `x` characters reached the buffer |
| Re-render completed entries on every projection | `completed_entries_render_once_and_only_the_partial_is_reparsed` | the parse counter grew on identical frames |
| Emit an RGB colour from the highlighter | `no_color_has_no_colour_and_ascii_mode_has_no_wide_glyphs`, `rust_keywords_strings_and_comments_map_to_ansi_roles_only` | a `Color::Rgb` cell failed the ANSI-only assertion |
