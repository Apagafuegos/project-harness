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
with an assistant reply, a continuation that cannot contain a user message.

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

Nothing in the pipeline reimplements a product stage. A missing product hook was a
"stop and report" condition, and none was needed: the report exposes the ordered
included items with their block-level decisions, `Command::SetContextControl`
exists, and the stored `Compaction` entry carries the strategy, version and model
it was written with.

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
before the check, the runner writes it to `.harness-eval-check.sh`, runs
`sh .harness-eval-check.sh`, and removes it. Because the check is copied in only
after the session is closed, it cannot be sent to a model; the gate asserts that
no continuation request contains the check marker, the file name or the phrase
"hidden check", and that the file is gone afterwards.

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
  the seed, the task fingerprint, the policy fingerprint and the estimator
  identity. Session, entry and call identities never reach a record; durations are
  per-record and never aggregated; ratios are printed as fixed-width decimals from
  integer counts.
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

A record carries: the harness commit (`--commit`, normally
`git rev-parse HEAD`), the task fingerprint (SHA-256 over the task directory's
sorted `path, length, sha256` manifest), the strategy name **and version as
written into the committed compaction entry**, the session and summary models, the
compaction **policy fingerprint** (`context::compaction::policy_fingerprint`,
which hashes the strategy name, its version and the compaction configuration), the
estimator method and version, token estimates before and after
(`CompactionDiff::before/after.estimated_total`), the metrics, per-stage
durations, per-stage error codes and the repetition index.

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
- Mechanics: `crates/eval/tests/offline_gate.rs` (five ignored integration tests,
  listed above).
- Gate wiring: `scripts/check.sh`, `scripts/test-security-gate.sh`,
  `.github/workflows/ci.yml`.
- `eval/tasks/` — `refactor-constraint`, `bugfix-with-test`,
  `feature-style-rule`, `multi-file-change`.

### Break-on-purpose results

Each mutation was applied individually, the named check failed, and it was
reverted:

| Mutation | Failing check and observation |
| --- | --- |
| Score retention from the full history instead of the post-compaction projection | `a_summary_that_drops_a_requirement_lowers_projection_retention`: retention stayed at 3/3 after the requirement was removed from the summary |
| Run the hidden check on the host instead of in the sandbox | `the_hidden_check_runs_inside_the_sandbox_and_never_on_the_host`: `pwd` reported the host workspace path instead of `/workspace`, and the host-only sentinel was visible |
| Put the check's text into the model's request | `offline_pipeline_is_deterministic_complete_and_does_not_leak_the_check`: the continuation request contained the hidden-check marker |
| Leave the strategy out of the result record | `offline_pipeline_is_deterministic_complete_and_does_not_leak_the_check`: the record had no strategy, so the per-strategy aggregate and the policy-fingerprint assertion disagreed |
