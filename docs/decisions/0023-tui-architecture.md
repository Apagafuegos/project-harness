# 0023: TUI architecture — the pure model and the `Action` boundary

Status: Parts 1 and 2 implemented for Milestone 5 steps 02 and 03.

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
