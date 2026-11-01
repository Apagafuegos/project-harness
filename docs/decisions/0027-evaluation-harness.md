# 0027: The offline evaluation harness

Status: Implemented for Milestone 6, step 05 (m6-05), on branch `m6-eval`. The
live experiment run itself is m6-06. Milestone 6 settled decisions 11 and 12.

Date: 2026-11-01

## Decision

Milestone 6 compares three compaction strategies (settled decisions 3 and 4). The
comparison needs to be repeatable, offline in the gate, and honest about what it
does not measure, so the harness is a **separate dev-only crate**:

- `crates/eval`, package and library `harness-eval`, binary `harness-eval`,
  `publish = false` (inherited from the workspace).
- It is **not** a dependency of `harness`, and `harness-core` gains no
  evaluation type. The TUI boundary test stays green, and `cargo tree -p
  harness-core` is unchanged.
- It depends on `harness-core` (the runtime, the builder, the sandbox), and on
  `harness` for `harness::wiring`, so a session is created by the same assembly
  the headless and terminal clients use instead of a second copy of it.

### Dependencies

`clap` and `tokio` are already workspace dependencies of the shipped client.
`serde` and `serde_json` are already in the workspace and the lock file, and
decision 11 names `serde_json` for the JSON output. No third-party crate is added
for the evaluation itself:

- **No regular expressions.** Matchers are normalized substrings (`all_of` and
  `any_of`). Substring matching is deterministic, dependency-free and easy to
  audit; a regex engine would make it easy to write a matcher that passes for the
  wrong reason, and the task format does not need one.
- **No `tempfile` at runtime.** `tempfile` is a test-only dependency, per the
  carried-over M3–M5 rule. The binary makes its own scratch directory under the
  system temp directory with a fixed-width name (see *Determinism*).
- **No `git` process.** The harness commit is a `--commit` argument, normally
  `git rev-parse HEAD` from the calling shell. The crate never spawns a process
  outside the sandbox.

### The task format

A task is a directory under `eval/tasks/<name>/`. The full schema, validation
rules and the authoring checklist are in
[`eval/README.md`](../../eval/README.md); the shape is `task.json`,
`workspace/`, `history.jsonl`, `requirements.json`, `modified_files.json`,
`continue.md`, `check.sh`, optional `controls.json`, and `scripted/` for the
offline answers.

**Replayable script, not a recorded journal.** The pre-compaction history is a
script of user messages, assistant replies and tool proposals, replayed through
the real runtime. A recorded journal was rejected: it is bound to entry
identities, timestamps, artifact digests and a record version, so it would need
migrating whenever a record version changes, and it could not be re-pointed at a
fresh scratch workspace. A script replays the same conversation against a fresh
workspace with fresh identities, and every tool call in it is a **real** call
through the product's tool host, producing a **real** result. That is what makes
the journal, and therefore the compaction input, genuine.

The script vocabulary is deliberately small (`user`, `assistant`, `tool_call`,
plus optional `say` narration), and the loader enforces the shape that matters:
4–8 turns, at least two tool calls, at least two requirements, every turn ending
with an assistant reply and **no reply before that** (a reply ends its turn; a
stray one would silently push the remaining scripts into the next stage, which the
runner now also refuses as `unconsumed_*_scripts`), and a continuation that cannot
contain a user message.

**Ground truth is measured, not declared.** `modified_files.json` names the files
the history itself changed, and the pipeline diffs the scratch workspace against
the task's pristine copy at the pre-compaction boundary to produce
`pre_compaction_changes`. The gate asserts they are equal for every task, that the
set is non-empty, and that the hidden check still fails on that boundary snapshot.
A path existing in the workspace is never evidence that it changed; the pristine
check in `tests/task_format.rs` only verifies that a declared path is a regular
file inside the workspace.

### Metrics, exactly

For one task × strategy × repetition, `requirements_total` and `files_total` are
the task's own counts, and:

```text
requirements_retained_projection
    = |{r : r.matcher matches normalize(projection_text)}|
requirements_retained_probe
    = |{r : r.matcher matches normalize(probe_text)}|
files_recalled_projection
    = |{p in modified_files : normalize(projection_text) contains normalize(p)}|
files_recalled_probe
    = |{p in modified_files : normalize(probe_text) contains normalize(p)}|
completion
    = the hidden check exited 0 inside the bubblewrap sandbox
```

`normalize(s)` lowercases `s` and replaces every run of whitespace with a single
space, trimming the ends. Nothing else changes: punctuation stays significant and
a term is matched literally. `r.matcher` is `all_of ∧ (any_of ≠ ∅ → ∃ any_of)`,
with both lists normalized the same way; at least one term is required.

`projection_text` is rendered from the product's own account of the projection:
the live `ContextReport`'s `included` items, in order, each rendered from the
journal — user text, included assistant text and its included tool calls with
their arguments, tool-result text, and the `summary` text of each included
compaction entry. Instructions are skipped: they are fixed scaffolding, identical
in every arm, and never derived from the task's history. Rendering from the report
rather than from a second build means the scored text cannot disagree with what
the runtime decided to include.

`modified_files` is the set the history really changed, so file recall measures
whether the projection still names files the task had already touched.

`probe_text` is the probe turn's assistant text: one extra turn after compaction
with a prompt fixed by the harness (`pipeline::PROBE_PROMPT`) asking for the
requirements and the files changed so far, scored with the same matchers. The two
numbers answer different questions: what the projection kept, and what the model
can still say after seeing it.

### The pipeline

Per task × strategy × repetition: copy the workspace into a fresh scratch
directory; create the session through `harness::wiring` with the strategy recorded
in its configuration and an independent summary model where the strategy needs
one; replay the history; apply the task's controls through
`Command::SetContextControl`; run **one** compaction through
`PreviewCompaction` then `CommitCompaction`, forced regardless of the budget
trigger; score the projection; ask the probe; send `continue.md` through the
normal tool loop bounded by `max_inferences_per_turn` and a per-stage deadline;
close the session; run the hidden check.

**The fixed history is replayed in live mode too.** A `PhaseProvider` serves every
request from the task's scripts until the history has replayed, then hands the
summary, probe and continuation to the plan's provider — in offline mode that is
the same scripted provider, so the phase switch is a no-op. This is what makes the
comparison meaningful: the pre-compaction conversation and workspace are identical
in both plans and across strategies, and a live run does not spend requests before
the measured stage. The gate asserts identical `history_requests` (dialogue only,
because instructions embed the absolute workspace path) and identical
`pre_compaction_changes` across plans and strategies.

**Shutdown is unconditional.** `drive` awaits `SessionHandle::shutdown` on every
post-open exit — success, refusal, coded failure or timeout — before `run_task` is
allowed to publish the hidden check. Dropping the handle would only signal
asynchronous shutdown, which on a current-thread runtime can still be pending when
a synchronous check runs. Stage failures carry their own stage, so a probe timeout
is recorded as `probe: stage_timeout`, never as a replay failure; the check runs
only when `errors` is clear and metrics exist. Each stage deadline is injectable
(`RunOptions::stage_timeout`) so the gate can prove the failure paths release
everything with a 250 ms bound.

Nothing in the pipeline reimplements a product stage. A missing product hook was a
"stop and report" condition, and none was needed: the report exposes the ordered
included items with their block-level decisions, `Command::SetContextControl`
exists, and the stored `Compaction` entry carries the strategy, version, model and
configuration digest it was written with.

### Auto-approval, and its exact scope

The eval answers the product's approval prompts itself. The session policy still
says `ask` for every mutation and every process: auto-approval is **not** a policy
value, it does not exist in the product, and a search for approval decisions
outside `crates/eval` finds none. The eval submits `Decision::Allow` for each ticket it
is shown, because it runs unattended in a scratch workspace it created.

The scope is enforced by construction, not by convention. Each cell gets its own
data root, its own workspace copy, and a `SandboxConfig` whose `workspace_root`
is that copy; the data root and the workspace are under one scratch directory the
eval created. The session's policy is the wiring's fixed
`ask/ask/ask/deny-network` policy with no allowances, so a tool call can only touch
that workspace, and the shell only ever runs inside `bwrap`. A refusal that is not
an approval (`busy`, `resolution_required`, `faulted`) fails the stage instead of
being retried.

### Sandboxing

The hidden check runs through the product's `Sandbox`, the same backend the shell
tool uses: `--unshare-all`, network denied, the scratch workspace bound
read-write at `/workspace`, `--chdir /workspace`, the system paths mounted
read-only, a fresh `/proc`, `/dev` and `/tmp`. The plan is a normal `bash` plan
built by `Registry::builtin()` and `Tool::plan` under an `eval-check-v1` policy,
then executed with `Sandbox::run`. There is **no host fallback**: an unusable
backend fails `Sandbox::probe`, the run fails with `sandbox_unavailable`, and the
gate is ignored where `bwrap` is missing, exactly like the product's S02 targets.

The check script is not part of the workspace while the model runs. Immediately
before the check, the runner **publishes** it to `.harness-eval-check.sh`, runs
`sh .harness-eval-check.sh`, and removes it. Because the check is copied in only
after the session is closed, it cannot be sent to a model; the gate asserts that
no continuation request contains the check marker, the file name or the phrase
"hidden check", and that the file is gone afterwards.

Publication is the one host-side write into a model-controlled directory, so it is
exclusive and never follows a link: `create_new` plus `O_NOFOLLOW`. A pre-existing
file, symlink or hardlink at the carrier name is refused with
`check_carrier_exists` and its target is left untouched; a carrier that is not a
single-link regular file is refused as `check_carrier_unsafe`; after the run the
carrier must still be that file (`check_carrier_replaced`, `check_carrier_missing`)
and cleanup must succeed (`check_cleanup_failed`). Any of those failures makes the
outcome a non-passing check, never a silent success. The regression test links the
carrier to an external sentinel and asserts the sentinel keeps its bytes.

The gate itself (`crates/eval/tests/offline_gate.rs`, `#[ignore]`) also asserts
that the runner really is isolated: a command run through it reports
`pwd = /workspace`, cannot see a host-only file outside the workspace, and sees a
route table with no routes.

### Determinism

Offline mode is deterministic. Two runs of the same suite write **byte-identical**
`aggregate.json` and `aggregate.md`, and records that are equal apart from
`durations_ms`. The gate asserts both.

What made this need care:

- **No identities or clocks in aggregates.** Records carry the repetition index as
  the seed, the task fingerprint, the compaction-policy fingerprint and the
  estimator identity. Session, entry and call identities never reach a record;
  durations are per-record and never aggregated; ratios are printed as fixed-width
  decimals from integer counts.
- **Two configuration identities, one of them path-dependent (decision 15).** Each
  record keeps the exact session-configuration artifact digest the committed
  compaction entry binds — which embeds the absolute workspace path — *and* the
  compaction-policy fingerprint, which does not. The aggregate groups by the policy
  fingerprint and omits the raw digest, so the aggregate is the deterministic
  comparison while the record keeps exact provenance. Records from two runs are
  therefore equal apart from `durations_ms` and `session_configuration`; that is
  asserted, not assumed.
- **The scratch path must not influence an estimate.** A session's base
  instruction embeds the absolute workspace path, and the heuristic estimator
  counts its bytes. The eval therefore places every cell under a scratch root
  whose name has a **fixed width**. Without that, moving the results directory
  changed a token estimate by one — the sort of nondeterminism that is invisible
  until two runs are diffed.
- **Ordering.** Tasks are sorted by name, strategies follow the requested order,
  and every map is a `BTreeMap`, so nothing depends on hash iteration order.
- **Scripted answers.** The offline summary, probe answer and continuation are
  files in the task directory, so the model side of the pipeline is a constant.

Live mode is not deterministic and is not expected to be; it uses the same
pipeline with a real provider for the post-history stages.

### Records and provenance

One record per task × strategy × repetition, written to
`<out>/records/<task>.<strategy>.<n>.json`, with `<out>/aggregate.json` and
`<out>/aggregate.md`. The default `<out>` is `eval/results/<run-id>/`, which is
git-ignored.

A record (schema 2) carries: the harness commit, the task fingerprint (SHA-256 over
the task directory's sorted `path, length, sha256` manifest), the strategy name
**and version as written into the committed compaction entry**, the session and
summary models, **both configuration identities** —
`session_configuration` (the compaction entry's
`configuration_fingerprint`, i.e. the exact session-configuration artifact digest)
and `compaction_policy` (`context::compaction::policy_fingerprint`) — the
estimator method and version, token estimates before and after
(`CompactionDiff::before/after.estimated_total`), the metrics, per-stage
durations, per-stage error codes (one slot per stage) and the repetition index.

The commit is never `unknown`: `--commit` must be a 40-character id and is
validated, and when it is absent the binary resolves `HEAD` from the checkout
itself — `.git` as a directory or a linked worktree's `gitdir`, then the ref file
or `packed-refs` — without spawning a process. If neither works, the run exits 2
and asks for `--commit` instead of recording a placeholder. `scripts/eval.sh`
passes `git rev-parse HEAD` automatically.

The strategy is read back from the committed entry rather than from the input, so
a record cannot claim a strategy the journal does not hold; `observation_masking`
records no summary model because the entry has none. The gate asserts that all
three strategies appear with their own policy fingerprint, so leaving the strategy
out of a record fails a test.

### Offline mode in the gate

`harness-eval --offline` runs every cell with `ScriptedProvider`. It is in
`scripts/check.sh` and in the S02 CI job, through an ignored integration test
rather than the binary, so that the "required target fails without `bwrap`"
control-flow test in `scripts/test-security-gate.sh` covers it the same way it
covers `sandbox`, `tool_host` and `doctor`.

The gate asserts: the suite has four tasks covering four shapes; each task's
hidden check **fails** on the pre-task workspace and passes after the fixing
continuation; every cell completes; the two runs' aggregates are byte-identical; a
summary with a requirement removed lowers `requirements_retained_projection` and
specifically drops that requirement; a continuation that changes nothing gives
`completion: false` without being a harness error; no continuation request
contains the check; and the aggregate's sums match the records it was built from.

Runtime: about 35 seconds on a development host.

### Live mode

`harness-eval --live --provider deepseek --model deepseek-flash
[--summary-model …] [--repetitions N] [--tasks …] [--strategies …] --yes`, wrapped
by `scripts/eval.sh`. The credential comes from the environment
(`DEEPSEEK_API_KEY`); nothing is printed, logged or stored. Live mode:

- replays the same scripted **history** for every arm — the comparison needs an
  identical pre-compaction conversation — and uses the real provider for the
  summary, the probe and the continuation;
- first runs a **scripted dry run** of the same cells, prints a coarse estimate
  (cells, model requests, approximate prompt tokens) and exits 2 without `--yes`.
  The estimate is order-of-magnitude by construction: the estimator is heuristic,
  the continuation's live tool loop is unknown, and output tokens are excluded;
- is never in CI, and no live run is performed in this step. m6-06 runs it.

## Limits

These are stated here so a reader of the m6-06 report cannot mistake the numbers:

- **Small N.** Four tasks, three strategies, one repetition per cell offline.
  Nothing here is a statistical claim about which strategy is better; a single
  task can move a mean.
- **Lossy measurement.** Retention is a matcher over text. A summary can retain a
  requirement by quoting it, or drop it and still behave correctly; a requirement
  can be restated in words the matcher does not know. The metric measures
  presence, not fidelity or correctness.
- **No LLM judge.** Scoring is deterministic on purpose (settled decision 11).
  There is no model in the loop deciding whether an answer is "good".
- **Not workspace rollback.** The harness never reverts a workspace. Completion is
  the hidden check on the workspace as the continuation left it; the pre-task
  failure check uses a *copy*. A continuation that half-fixes a task simply
  reports `completion: false`.
- **Offline proves mechanics, not quality.** Offline summary, probe and
  continuation texts are fixtures in the task directory. The offline table is a
  smoke test of the pipeline, so the default suite reports every arm at full
  retention and completion; the gate's mutation tests are what show the metrics
  respond.
- **One compaction per cell.** The harness measures the first compaction, not a
  rolling sequence of them, and it forces it regardless of the budget trigger.
- **The probe is one turn.** A model that restates requirements better on a second
  attempt is not measured.

## Alternatives considered

- **Recorded journals as fixtures.** Rejected: bound to identities, timestamps and
  a record version, and impossible to replay against a fresh workspace.
- **Scoring from the full history.** Rejected: it would measure the task, not the
  compaction. The gate breaks this on purpose — scoring from the full history
  stops the dropped-requirement test from failing.
- **An LLM judge.** Rejected for M6 by settled decision 11.
- **A regex matcher.** Deferred: normalized substrings cover the recorded tasks and
  add no dependency; a regex matcher can be added later behind the same
  `Matcher` type without changing a task.
- **Running the hidden check on the host.** Rejected outright: the check executes
  arbitrary shell in a workspace a model just edited.

## Evidence

- Format and matchers: `crates/eval/src/task.rs`, `src/matcher.rs` unit tests and
  `crates/eval/tests/task_format.rs` (suite shape, requirement self-match,
  fingerprint, probe prompt, literal matchers).
- Mechanics: `crates/eval/tests/offline_gate.rs` (nineteen ignored integration
  tests, listed above, including the review regressions for m6-05).
- Gate wiring: `scripts/check.sh`, `scripts/test-security-gate.sh`,
  `.github/workflows/ci.yml`.
- `eval/tasks/` — `refactor-constraint`, `bugfix-with-test`,
  `feature-style-rule`, `multi-file-change`.

## Review round 2 (m6-05 @ 970b9b5)

The final review also exercises a successful injected-live path through all three
strategies. Offline post-history replies are queued only for the scripted plan;
the live plan consumes only its injected provider's responses. Carrier cleanup
compares device and inode with the exclusively created file and preserves any
replacement, reporting `check_carrier_replaced`. Its CLOEXEC handle stays open
through execution to prevent unlink/recreation from reusing that inode.

The first head was not accepted. Six findings, and what changed:

1. **P1, check publication escaped through a symlink.** `fs::write` into a
   model-controlled directory followed a model-created symlink and overwrote its
   external target. Now: exclusive `create_new` + `O_NOFOLLOW`, refuse any
   pre-existing object, verify the carrier is a single-link regular file, remove
   only the file this call created, and fail the check on publication or cleanup
   failure. Regression: `hidden_check_publication_refuses_links_and_existing_paths`.
2. **P1, live mode did not replay the fixed history.** `ModelPlan::Live` skipped
   the scripts, so every history prompt went to the real provider. Now a
   `PhaseProvider` serves the scripts for the fixed history in **both** plans and
   only the post-history stages reach the injected provider. Regressions:
   `live_plan_replays_the_fixed_history_before_the_first_live_request`,
   `live_and_scripted_plans_replay_identical_history_and_workspaces`.
3. **P2, file recall ground truth listed planned work.** No history modified a
   file; `modified_files.json` listed what the continuation would change. Now every
   history makes real, incomplete progress through write/edit in a coverable turn,
   `modified_files.json` names exactly the files the history changed, and the gate
   diffs the workspace at the pre-compaction boundary. Regression:
   `every_task_declares_exactly_the_files_its_history_changed`.
4. **P1, error paths skipped awaited shutdown.** Stage failures returned out of the
   pipeline without awaiting `shutdown`, were labelled `replay`, and could publish
   the hidden check while driver or executor work was live. Now: one awaited
   shutdown on every post-open exit, per-stage attribution (`control` is its own
   record slot), a check only when every preceding stage succeeded, and injectable
   deadlines. Regressions: `a_failed_compaction_skips_the_probe_and_the_hidden_check`,
   `a_probe_failure_is_attributed_and_skips_the_hidden_check`,
   `a_stalled_probe_times_out_and_the_continuation_never_runs`.
5. **P2, incomplete provenance.** The wrapper did not pass `--commit`, so records
   said `unknown`, and `configuration_fingerprint` was only the compaction policy.
   Now the commit is resolved and validated (never `unknown`), and the record keeps
   the exact session-configuration digest separately from the compaction-policy
   fingerprint, per decision 15, with the aggregate carrying only the deterministic
   policy identity. Regressions:
   `cli_records_a_real_commit_and_refuses_a_malformed_one`,
   `a_non_compaction_configuration_change_changes_source_provenance`, and the
   record-equality half of the determinism test.
6. **P2, completion accepted constraint violations.** A forbidden edit to the
   protected file, an arbitrary extra file, and a removed final newline all passed.
   Now every check enforces the allowed file set, compares protected files against
   trusted bytes, and compares output bytes with a sentinel so the final newline
   counts. Regressions: `completion_rejects_a_change_to_the_forbidden_math_file`,
   `completion_rejects_an_extra_file_in_the_no_new_files_task`,
   `byte_identical_output_includes_the_final_newline`.

Two reviewer CLI corrections are kept and now covered: the runtime uses
`enable_all` (a time-only runtime has no I/O driver for the live adapters) and an
absent `--summary-model` inherits the selected session model rather than the
provider default — `the_cli_runtime_has_an_io_driver_for_the_live_adapters` and
`an_absent_summary_model_inherits_the_selected_session_model`.

The reviewer's temporary `tests/review_findings.rs` target was consolidated into
`tests/offline_gate.rs`, which `scripts/check.sh` and the CI S02 job run; no
ignored target is left outside the gate. A loader rule added here —
`an_assistant_reply_before_the_last_step_is_rejected` — exists because the first
version of finding 3's fixture had a mid-turn reply, which silently pushed two
scripts into the summary request.

### Break-on-purpose results

Each mutation was applied individually, the named check failed, and it was
reverted:

| Mutation | Failing check and observation |
| --- | --- |
| Score retention from the full history instead of the post-compaction projection | `a_summary_that_drops_a_requirement_lowers_projection_retention`: retention stayed at 3/3 after the requirement was removed from the summary |
| Run the hidden check on the host instead of in the sandbox | `the_hidden_check_runs_inside_the_sandbox_and_never_on_the_host`: `pwd` reported the host workspace path instead of `/workspace`, and the host-only sentinel was visible |
| Put the check's text into the model's request | `offline_pipeline_is_deterministic_complete_and_does_not_leak_the_check`: the continuation request contained the hidden-check marker |
| Leave the strategy out of the result record | `offline_pipeline_is_deterministic_complete_and_does_not_leak_the_check`: the record had no strategy, so the per-strategy aggregate and the policy-fingerprint assertion disagreed |
