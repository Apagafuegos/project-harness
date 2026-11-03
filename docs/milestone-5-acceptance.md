# Milestone 5 acceptance

Milestone 5, “Ratatui client”, is complete within its agreed scope as of
**2026-10-31**. This maps every sentence-level criterion in
[roadmap §5](roadmap.md#5-ratatui-client--complete) and the behavioral settled
decisions 1–4, 9–11 and 14 to evidence that runs in the gate. The original live
TUI run hit the subscription's Codex quota limit; the M7-01 evidence below
closes that limitation on 2026-11-03. Nothing else in scope is uncovered.

Status: **met** means covered within the agreed milestone scope; **partial**
means a recorded limitation with its owning decision or later milestone named;
**gap** means an uncovered in-scope requirement. There are no gap rows.

Test names are Rust functions unless a target or script is named. The terminal
client's offline evidence lives in five integration targets:

| Target | What it drives | Why it is separate |
| --- | --- | --- |
| `crates/cli/tests/tui.rs` | Pure `App`: recorded `SubscriptionItem`s from a real `SessionHandle`, reduced and rendered into `TestBackend` | No terminal, no Tokio in the model |
| `crates/cli/tests/tui_runtime.rs` | The injectable loop: scripted events/signals, real storage, `ScriptedProvider` | Event/reply ordering, cancellation, quit |
| `crates/cli/tests/tui_navigation.rs` | Session handover, branch tree, model switching through the production loop | Durable shutdown/open and writer-lock retry |
| `crates/cli/tests/tui_inspector.rs` | Context inspector and preview-first compaction through the production loop | Read-only inspection, nonblocking replies |
| `crates/cli/tests/tui_pty.rs` | The real binary under a PTY (feature `test-provider`, debug only) | Termios/escape-sequence restoration |
| `crates/cli/tests/tui_recovery.rs` | A real child process killed after a durable write intent | Uncertain state cannot be faked in-process |

No ordinary test needs a network, a credential, or a terminal. The scope, the
engine boundary and the terminal lifecycle are recorded in
[ADR 0021](decisions/0021-shared-wiring-session-catalog.md),
[ADR 0022](decisions/0022-recorded-model-instructions.md) and
[ADR 0023](decisions/0023-tui-architecture.md).

## Acceptance matrix

| Criterion | Evidence running in the gate | Status | Note |
| --- | --- | --- | --- |
| Rendering over the same headless engine API | `streamed_answer_renders_markdown_and_status`, `denied_tool_is_rendered_with_its_outcome`, `failed_turn_shows_its_code`, `compaction_summary_is_a_distinct_block`, `model_switch_renders_its_portability`, `narrow_and_wide_areas_render_without_panicking`, `diagnostics_and_faults_render_with_their_codes` | met | One pure `App` draws transcript, prompt, banner, status bar and one modal. Rows come from `snapshot.ancestry(head)` plus the live partial; nothing is re-derived from provider text. 20×5 and 80×24 both render. |
| Navigation (sessions, branches, models) | `picker_keys_and_render_status_refusals`, `opening_another_and_back_releases_old_writer_durably_and_new_session`, `busy_active_turn_handover_is_refused_without_shutdown`, `branch_selection_reprojects_and_edit_resubmit_preserves_original_sibling`, `select_head_during_active_turn_shows_core_busy_reason`, `switch_reject_first_report_then_explicit_omit_has_no_foreign_state`, `required_incompatibility_is_not_omittable`, `dialogs_render_and_map_keys_without_the_loop` | met | Ctrl-O catalog, Ctrl-T branch tree, Ctrl-L models. Handover is idle-only, durably shuts the old handle down, retries `session_locked` once, and restores the previous session when an open fails. Other workspaces are refused, never authorized. |
| Streaming | `streamed_answer_paste_resize_and_ctrl_d`, `streamed_answer_renders_markdown_and_status` | met | `TextDelta`/`ToolArgumentsDelta`/`UsageUpdated` reduce into one `ClientView`; the loop draws on a 16 ms tick and resizes immediately. |
| Cancellation | `esc_cancel_and_double_ctrl_c_quit_are_durable`, `cancelled_turn_streams_then_finishes_cancelled`, `loop_generation_shows_progress_then_quit_cancels_without_waiting` | met | Esc and the first Ctrl-C submit `Cancel`; quit waits for the durable `turn_finished`, not for `Cancelling`. A stalled summary generation is cancelled before draining. |
| Approvals | `approval_modal_captures_a_d_r_and_esc`, `approval_allow_deny_and_dismiss`, `denied_tool_is_rendered_with_its_outcome` | met | The modal binds the view's `approval_id`/`plan_digest`. `a`/`r` allow, `d` denies, Esc denies; every other key is ignored. No grant is remembered without `r`. |
| Context inspector | `pure_panel_budget_order_exclusions_configuration_and_preview_label`, `loop_panel_report_refreshes_after_committed_turn`, `loop_model_override_uses_merged_catalog_and_never_switches`, `preview_first_requires_a_render_and_explicit_enter`, `loop_compaction_preview_then_commit_summary_in_transcript_and_report`, `loop_stale_preview_names_the_changed_field_and_offers_retry`, `loop_not_eligible_names_reason_and_makes_zero_provider_calls`, `loop_budget_failure_preserves_reason_and_suggests_eligible_compaction`, `slow_inspection_never_blocks_keys_resize_or_render_ticks`, `pure_unknown_overflow_flags_and_old_report_rejection`, `inspect_preview_reads_live_proposal_and_stale_rejection_without_regeneration`, `loop_escape_during_generation_shows_progress_and_preserves_journal`, `loop_preview_cancel_leaves_journal_bytes_identical` | met | Ctrl-K is read-only: `SessionHandle::inspect_context` and the preview accessor only. Budgets are labelled estimates, opaque bytes are not tokens, and previews write no journal bytes. An override preview never switches the model. |
| Same headless engine API, no client fork | `tui_never_builds_requests_runs_tools_or_approves_without_a_key`, `lag_reset_does_not_duplicate_rows`, `faulted_reset_still_renders_banner_and_accepts_quit`, `actual_crash_uncertain_resolution_then_next_turn` | met | `harness-core` gained no runtime, provider or storage API in M5 except the read-only catalog and `ModelCatalog::iter()` (decision 14). Actions carry `harness_core::runtime::Command` verbatim plus four shell-only navigation/inspection actions. |
| **The TUI implements no provider request building, no tool authorization and no execution** | `tui_never_builds_requests_runs_tools_or_approves_without_a_key`, `boundary_guard_rejects_provider_execution_and_timer_approval_mutations` | met | Source guard over every file under `crates/cli/src/tui/`, recursively. Forbidden: `harness_providers`, `ModelRequest`, `PreparedContext`, `FsWorkspace`, `Sandbox`, `ToolHost`, `Executor`, `AuthorizedOperation`, `ExecutionOutcome`, `ToolSet`, `PolicySnapshot`, `Registry` and code inclusion. `SubmitApproval` is allowed only in `mod.rs::approval_key`; the key reducer may be called only from the terminal event arm and the Ctrl-C signal arm. The second test proves the guard rejects each mutation, including an import alias and a timer that synthesizes a key. |
| Terminal state restored on shutdown and errors (plus panics and signals) | `terminal_restored_on_quit_panic_and_sigterm`, `terminal_restored_after_returned_loop_error`, `non_tty_exits_two_without_terminal_sequences`, `faulted_reset_still_renders_banner_and_accepts_quit` | met | `TerminalGuard` refuses a non-TTY before mutating anything, and restores raw mode, paste, alternate screen and cursor idempotently. The PTY probe compares complete termios before/after and requires the restoration sequence for quit, panic, SIGTERM and a returned `Err`; the panic assertion also requires restoration **before** the diagnostic. A returned error exits 1 with the journal released. |
| Subscriber lag handled through coherent snapshots | `lag_reset_does_not_duplicate_rows`, `lag_reset_mid_stream_replaces_transcript_inspector_and_branch_tree`, `mid_stream_reset_rebuilds_without_duplicates` | met | `Reset` replaces the whole view and recomputes transcript, inspector request identity and branch tree. The coherence test provokes a real broadcast overflow (capacity 1), then compares transcript, reader, inspector and branch-tree renders with a fresh client of the same authoritative view, and requires the two renders to be byte-identical per panel; a late report for the old head cannot repopulate the panel. A second reset after the late report must stay identical too, so an invalidated report cannot come back. |
| Decision **1**: plain `harness` opens the TUI; no `tui` alias | `plain_harness_is_the_tui_with_seven_flags_and_no_config_file`, `terminal_restored_on_quit_panic_and_sigterm`, `non_tty_exits_two_without_terminal_sequences` | met | The PTY tests spawn the binary with no subcommand. `harness tui` exits 2. The workspace is the canonical cwd. M6 added the shared `--compaction-strategy` flag, so the M5 name and count were updated to seven; the M5 behavior is otherwise unchanged. |
| Decisions **2** and **11**: recorded base + root `AGENTS.md` instructions | `new_session_records_base_and_root_agents_and_sends_instructions_first`, `missing_agents_gets_base_only_and_unavailable_sandbox_never_mentions_shell`, `oversized_agents_is_skipped_with_64_kib_warning_not_truncated`, `invalid_utf8_agents_is_skipped_with_warning`, `escaping_agents_symlink_is_skipped_with_warning`, `agents_exactly_64_kib_is_not_truncated_or_skipped`, `edited_agents_continuation_keeps_recorded_instructions_and_opens_fine`, `catalog_json_and_context_include_new_session_instructions_without_modification` | met | Both clients assemble through `wiring::open_session`; the TUI shows creation warnings in its banner. Instructions are creation configuration at `system` then `developer` level, immutable for a continuation (ADR 0022). |
| Decision **3**: flags only, no config file | `plain_harness_is_the_tui_with_seven_flags_and_no_config_file`, `ask_help_and_usage_errors` | met | Seven top-level options after M6, shared with `ask`. `--config` is rejected with exit 2. |
| Decision **4**: minimal Markdown | `streamed_answer_renders_markdown_and_status` | met | `pulldown-cmark` headings, emphasis, lists, quotes, inline code and fenced blocks; raw HTML is inert text. No syntax highlighting (decision). |
| Decision **9**: key map, dismiss denies, durable quit | `every_decision_9_key_binding`, `approval_modal_captures_a_d_r_and_esc`, `approval_allow_deny_and_dismiss`, `esc_cancel_and_double_ctrl_c_quit_are_durable`, `streamed_answer_paste_resize_and_ctrl_d`, `dialogs_render_and_map_keys_without_the_loop` | met | Enter sends, Shift/Alt+Enter newline, Esc cancels, Ctrl-C cancels then quits within the busy period, Ctrl-D quits on an empty prompt, quit mid-turn cancels durably first, and the modal captures input until answered. |
| Decision **10**: `ask` unchanged and scriptable | `exact_stdout_streamed_answer_and_reasoning_only_on_stderr`, `continue_session_preserves_history_without_reprinting_old_answer`, `stdin_dash_is_prompt_not_approval_input`, `lag_resets_preserve_exact_stdout_without_duplicates`, `budget_failure_is_1_without_opening_provider`, `ask_help_and_usage_errors` | met | Only assistant text reaches stdout; approvals use `/dev/tty`; exit codes and flags are unchanged by M5. The TUI and `ask` share `wiring`, not state. |
| Decision **14**: catalog accessor is the only core addition | `iter_lists_every_model_in_instance_then_name_order`, `empty_catalog_and_strays_are_ignored`, `live_writer_is_busy_and_listing_never_takes_exclusive_lock_or_changes_bytes`, `torn_corrupt_unsupported_and_unsafe_sessions_stay_listed_without_changes`, `sessions_help_empty_missing_and_relative_root` | met | `ModelCatalog::iter()` is read-only and ordered by instance then model name. `DataRoot::sessions()` is the read-only catalog; listing takes a shared lock, writes nothing and repairs nothing. |
| Product goal: a real coding session end to end in the TUI | `streamed_answer_paste_resize_and_ctrl_d`, `approval_allow_deny_and_dismiss`, `opening_another_and_back_releases_old_writer_durably_and_new_session`, `branch_selection_reprojects_and_edit_resubmit_preserves_original_sibling`, `actual_crash_uncertain_resolution_then_next_turn`; opt-in live `tui` below | met | The offline evidence runs a real streamed turn, a real approved write through the tool host, durable handover, branch re-projection and crash resolution. On 2026-11-03, commit `8724ff6`, both live PTY smokes completed a pong answer and restored the terminal (Codex `gpt-6.1-sol`, DeepSeek `deepseek-flash`, exit 0). The live smoke proves answer/quit/restoration, not a live coding/tool workflow; the latter remains offline evidence. |

## Running the evidence

```sh
cargo fetch --locked                 # provisioning, may use network
CARGO_NET_OFFLINE=true ./scripts/check.sh
# Stronger external-network enforcement, used for the acceptance run and CI:
./scripts/with-offline-network.sh ./scripts/with-clean-fds.sh \
  env CARGO_NET_OFFLINE=true ./scripts/check.sh
```

The complete Rust **1.99.1** acceptance gate passed with a **3,600,000 ms**
timeout. `check.sh` builds every target, then runs the ordinary workspace tests,
the `test-provider` PTY target, the conformance-feature core tests, the required
S02 security suite and both examples. `cargo fmt --all` was run before the gate.

Against base **9a3b21b**, this step adds **five Rust tests** and one small
product fix:

- `crates/cli/tests/tui_boundary.rs` (new target, 2 tests): the boundary guard
  and its self-test (mutations listed below).
- `crates/cli/tests/tui_inspector.rs`: `lag_reset_mid_stream_replaces_transcript_inspector_and_branch_tree`
  (real broadcast overflow) plus a `Fixture::with_capacity` helper.
- `crates/cli/tests/tui_pty.rs`: `plain_harness_is_the_tui_with_seven_flags_and_no_config_file`
  (not feature-gated) and `terminal_restored_after_returned_loop_error`
  (feature-gated PTY).
- `crates/cli/src/tui_cli.rs`: a debug-only `HARNESS_TUI_TEST_ERROR` seam,
  compiled only with `test-provider` **and** `debug_assertions`, exactly like the
  existing `HARNESS_TUI_TEST_PANIC`/`HARNESS_TUI_TEST_PROVIDER` seams. It changes
  no release behavior.

Workspace test functions grow **599 → 604**; ignored opt-in tests stay **42**.
The new `tui_boundary` target runs everywhere; the returned-error test runs in
the feature-enabled PTY invocation that `check.sh` already performs.

### Product defect found and fixed

The coherence test found that `Reset` replaced the transcript but left the last
inspector report rendered against the new snapshot until the loop's next
refresh pass, and a late reply for the old head could still be accepted in that
window. Fix commit **3f5e1e8** clears the displayed report and its request
identity when a reset is reduced, which is ~10 lines in `tui/inspector.rs` and
one call in `tui/mod.rs`; it changes no public API and no engine behavior. The
test fails on the parent commit and passes after it.

## Boundary: what is allowed where

| Location | May do | Must not do |
| --- | --- | --- |
| `crates/cli/src/tui/` (model + loop) | Reduce `SubscriptionItem`s and `KeyEvent`s; emit typed `Command`s; request read-only inspection; render into a `Frame`; own the terminal and signals | Build a provider request, name a provider adapter, touch `tools::*` executors/workspaces/sandboxes/policies, decide an approval outside the modal handler, include code, or read artifacts/credentials/journal bytes |
| `crates/cli/src/tui_cli.rs` (shell) | Resolve flags, call `wiring` for provider/session assembly, own the process exit code | Submit commands itself, approve, or render |
| `crates/cli/src/wiring.rs` (shared) | Build providers, catalogs, policy, instructions, tool host, create/open | Depend on the TUI or on client I/O |

The guard is a source scan, not a compiler-enforced boundary: a determined
author could obfuscate a call. It exists so drift is loud in review, while the
engine's typed API, the crate graph and the tool host remain the real
enforcement. Approval authority stays with core: the UI can only answer a
ticket core opened.

## Break-on-purpose verification

Each mutation was applied alone to the committed tree, the named evidence was
observed failing, and the tree was restored before the gate.

| Temporary violation | Named evidence that failed | Result |
| --- | --- | --- |
| Call a tool executor from `tui/runtime.rs` | `tui_never_builds_requests_runs_tools_or_approves_without_a_key` | Guard reported `forbidden engine identifier Executor` and `AuthorizedOperation`. |
| Auto-approve from the render tick (timer) | Same guard | Guard reported `approval outside the modal key handler`. |
| Patch the inspector report incrementally across `Reset` | `lag_reset_mid_stream_replaces_transcript_inspector_and_branch_tree` | `panel Some('k') retained pre-reset state`. |
| Skip restoration on a returned loop error (`mem::forget` the guard) | `terminal_restored_after_returned_loop_error` | `termios differs: before=… after=…`. |

## Live checks — 2026-10-31, opt-in only

Each command was invoked once; no retry follows a rate limit. The scripts print
only sanitized summaries: no credential, account, request, session or message
text.

### `scripts/live-check.sh ask` — exit 0

```text
live ask: auth=codex
exit: 0
stdout contains pong: pass
summary: pass
```

### `scripts/live-check.sh tui` — exit 1, partial

```text
rate_limit (not retried; terminal restored; exit=0)
summary: fail
```

The verifier reached the real binary under a PTY, the subscription reported
`rate_limit`, and the binary still restored the terminal and exited zero; only
the assertion that an answer arrived failed. This is a quota state, not a
product failure, and that run establishes no live TUI answer. It is superseded
for the live-answer criterion by the passing M7-01 evidence below. The smoke
remains outside `check.sh` and CI.

An earlier invocation of the same command (10:43 UTC) produced no output at all
because the surrounding agent run ended mid-command; it is not evidence and is
not counted as a result.

## Evidence log — M7-01, 2026-11-03

Source commit: `8724ff6` (`test: add provider selection to opt-in live TUI smoke`)
on this host. Each paid smoke ran once, with no retries. These are sanitized
outputs; the two verbose termios Debug lines per run are normalized to the
verified equality below. No raw screen capture, session content or credentials
are retained. The PTY verifier also requires `completed` and the answer in
addition to the echoed prompt before quitting. (Since M7-05 removed the
`turn: completed` row, the verifier instead requires an idle render after the
answer before quitting.)

### `scripts/live-check.sh tui --provider openai` — exit 0

```text
live tui: provider=openai
auth=codex
model=gpt-6.1-sol
termios before/after: identical
sequences: enter alternate, enable paste, disable paste, leave alternate, show cursor; exit=0; pong observed
summary: pass
```

### `scripts/live-check.sh tui --provider deepseek` — exit 0

```text
live tui: provider=deepseek
model=deepseek-flash
termios before/after: identical
sequences: enter alternate, enable paste, disable paste, leave alternate, show cursor; exit=0; pong observed
summary: pass
```

M7-01 negative-input checks (no paid calls, no persistent changes):

```text
live tui: provider=deepseek
skipped (no DEEPSEEK_API_KEY)
exit: 0
usage: tui_smoke BINARY [--provider openai|deepseek]
exit: 2
usage: scripts/live-check.sh tui [--provider openai|deepseek]
exit: 2
```

The latter two invocations passed `--provider unknown`. The missing-key run
removed the key from the child environment only; no source mutation remained
to revert. The offline PTY tests retain their existing scenarios and assertions.

## Partial rows

| Item | What is missing | Where it belongs |
| --- | --- | --- |
| Live TUI end-to-end (closed 2026-11-03) | None for the live-answer smoke: Codex `gpt-6.1-sol` and DeepSeek `deepseek-flash` passed on this host at `8724ff6` | **Met**; Evidence log above. Original rate-limit evidence is preserved; no product change. |
| Anthropic live compatibility | `ANTHROPIC_API_KEY` and one live run | Carried from M4; ADR 0018 |
| Cross-provider live switching | A live OpenAI → Anthropic → OpenAI session | Carried from M4; ADR 0020 amendment |
| Exact token counts | The inspector shows labelled heuristic estimates | Carried from M4; a tokenizer decision |
| Pins and context experiments | Pin exclusion and strategy experiments | **Closed by [Milestone 6](milestone-6-acceptance.md):** whole-turn controls, three strategies, the offline evaluation harness and two live matrices are implemented and mapped there. This row records M5's past scope; it is not rewritten. |

## Known limits recorded by this milestone

- **No config file** (decision 3): seven command-line flags only after M6.
- **Nested `AGENTS.md` files are not read**; only the workspace root, capped at
  64 KiB, at session creation (ADR 0022).
- **No syntax highlighting** in fenced code blocks (decision 4).
- **One workspace per process**: the session picker lists other workspaces but
  refuses to open them; there is no workspace switch.
- **No session deletion, retention or export** UI; `harness journal` remains the
  repair path, and the TUI never repairs a torn tail silently.
- **No provider-credential UI**: authentication comes from the flags and the
  existing read-only Codex login or `ANTHROPIC_API_KEY`.
- **Model budget** is shown from the merged catalog; a model outside it and
  different from the opened model shows an unknown budget.
- The TUI depends on a real terminal for both stdin and stdout; piped use is
  `harness ask`, which exits 2 rather than degrading.
- Carried M4 limits are unchanged: `ConvertSupported` is refused, no automatic
  retry, preserved-thinking transformations are requested but not inspected,
  and opaque artifacts remain sensitive bytes on disk.
