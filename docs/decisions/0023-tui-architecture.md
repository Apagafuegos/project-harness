# 0023: TUI architecture — the pure model and the `Action` boundary

Status: Part 1 implemented for Milestone 5 step 02. The terminal lifecycle and
the event loop follow in step 03, which extends this record.

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
| `ratatui` | 0.30, default features | The selected TUI. Defaults select the crossterm backend, widgets, layout cache, macros and underline colour. `TestBackend`, `Frame` and `Terminal` come from `ratatui-core`. |
| `crossterm` | 0.29, `default-features = false`, `events` | Key-event types only. Ratatui's default backend already depends on the same 0.29 line, so there is one crossterm in the graph; the direct declaration makes the key types explicit instead of relying on a re-export. No terminal I/O is used here. |
| `pulldown-cmark` | 0.13, `default-features = false` | Minimal CommonMark parsing (decision 4). The default `html` and `getopts` features are unneeded; nothing is escaped to HTML and nothing is highlighted. |

Added to `[workspace.dependencies]` for version consistency with every other
dependency, but referenced only by `crates/cli`. `cargo tree` shows no
`ratatui`, `crossterm` or `pulldown-cmark` in `harness-core` or
`harness-providers`. `Cargo.lock` gains 108 package entries (232 → 340); they
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
snapshot crate is used. Twelve tests cover a streamed Markdown answer, the
approval modal and all four answers, a denied tool, cancellation, a failed turn
with `rate_limit`, a committed compaction summary, a model switch, a mid-stream
reset, narrow and wide layouts, every decision-9 key, the uncertain-resolution
modal and the opaque-state sentinel.

The uncertain-resolution fixture attaches an `awaiting` entry to a view read
from the completed real session: a resolution requires a process death between
the execution intent and its result, which an in-process test cannot produce
without an engine change. The snapshot, call identity and turn are real; only
the waiting state is attached. The modal logic depends only on that view.

Part 2 (m5-03) will add the terminal guard, raw mode and alternate screen,
restoration on panic and signals, the multiplexed event loop, resize,
the read-only auto-approve toggle and the real quit-cancels-durably path. The
projection, key map and `Action` boundary stay the extension point; a later
catalog-aware shell can refresh model limits after a switch without changing
the model.
