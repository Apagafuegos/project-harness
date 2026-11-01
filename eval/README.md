# Evaluation harness

`crates/eval` (`harness-eval`) is a **dev-only** crate that measures what each
compaction strategy preserves for continuing a coding task. It is not a
dependency of the `harness` binary, and `harness-core` holds none of its types.
The design, metric definitions and limits are in
[ADR 0027](../docs/decisions/0027-evaluation-harness.md).

Offline mode runs the whole pipeline with `ScriptedProvider` and is part of
`scripts/check.sh`, so it is in CI. It proves the **mechanics**, never model
quality: the post-history answers are fixed scripts.

## Running it

```sh
# Offline: deterministic, no credentials, needs Bubblewrap for the hidden check.
cargo run --locked -p harness-eval -- --offline

# A subset, with an explicit commit and results directory.
cargo run --locked -p harness-eval -- --offline \
    --tasks bugfix-with-test,feature-style-rule \
    --strategies rolling_summary,observation_masking \
    --repetitions 2 --out eval/results/local --commit "$(git rev-parse HEAD)"

# Opt-in live mode. Never part of CI; reads DEEPSEEK_API_KEY from the environment.
./scripts/eval.sh --live --provider deepseek --model deepseek-flash --yes
```

Exit status is 0 only when every cell ran every stage and produced a record. A
task that *completes false* is a metric, not a failure: only a mechanical error
(a refused compaction, a failed turn, a sandbox that could not run the check)
returns 1.

Results land in `<out>/records/<task>.<strategy>.<n>.json`, with
`<out>/aggregate.json` and `<out>/aggregate.md`. The default `<out>` is
`eval/results/<run-id>/`, which is git-ignored. Scratch workspaces live outside
it, under a fixed-width path in the system temp directory, so the results
directory never influences a token estimate; a failed run keeps its scratch tree
and prints where it is.

## Task format

A task is one directory under `eval/tasks/`:

| Path | Purpose |
| --- | --- |
| `task.json` | `{"name","shape","description"}`; `name` must equal the directory name |
| `workspace/` | the pre-task project tree, copied into a fresh scratch directory per run |
| `history.jsonl` | the pre-compaction conversation, replayed through the real runtime |
| `requirements.json` | the requirements stated in the history, with matchers |
| `modified_files.json` | `{"paths":[...]}` the history modified |
| `continue.md` | the continuation prompt sent after compaction |
| `check.sh` | the hidden completion check, run in Bubblewrap; never shown to a model |
| `controls.json` | optional `{"pin":[n],"exclude":[m]}`, 1-based turn ordinals |
| `scripted/summary.md` | offline summary text for the model-based strategies |
| `scripted/probe.md` | offline probe answer |
| `scripted/continue.jsonl` | offline continuation steps |

`shape` is one of `refactor`, `bug_fix`, `feature`, `multi_file`. The suite must
contain at least four tasks and cover all four shapes.

### `history.jsonl`

One JSON object per line. The history is **replayed as a script**, not restored
from a recorded journal: a journal is bound to entry identities, timestamps and a
record version, while a script replays through the real runtime and therefore
produces real tool calls and real tool results against the workspace.

```json
{"type":"user","text":"..."}
{"type":"tool_call","tool":"read","arguments":{"path":"report.sh"},"say":"optional narration"}
{"type":"assistant","text":"..."}
```

- A `user` step opens a turn. Everything until the next `user` step belongs to it.
- Every turn must end with an `assistant` step: a tool call may not be the last
  thing in a turn, because the model has to answer its result.
- `tool_call` runs the real built-in tool against the scratch workspace. The
  next step is the following inference, so a tool call and its result are always
  a real round trip.
- `assistant` is one completed response with one text block. `say` adds a text
  block before the call.

Validation requires 4–8 turns, at least 2 tool calls and at least 2 requirements.
`scripted/continue.jsonl` uses the same vocabulary without `user`, and must end
with an `assistant` step.

### `requirements.json`

```json
{"requirements":[
  {"id":"R1","text":"the printed output must stay byte-identical",
   "all_of":["byte-identical"]},
  {"id":"R3","text":"the existing two tests in test.sh must keep passing",
   "all_of":["existing two tests"],"any_of":["keep passing","still green"]}
]}
```

`id` is unique per task, `text` is the exact phrasing stated in the history, and
`all_of`/`any_of` are the matchers: **every** `all_of` term and, when `any_of` is
nonempty, **at least one** `any_of` term must occur in the scored text. At least
one term is required overall.

Matching is a literal substring test over normalized text: the text and the term
are lowercased and every run of whitespace collapses to one space, so a phrase a
summary wrapped across lines still matches. Nothing else is rewritten —
punctuation is significant, and a term that looks like a pattern is matched
literally. There is no regular-expression matcher in M6: substring matchers need
no dependency and are fully deterministic, and a regex engine would make it easy
to write a matcher that passes for the wrong reason.

`text` is also a self-check: `cargo test -p harness-eval` asserts that every
requirement's own phrasing satisfies its own matcher.

### `check.sh`

The hidden completion check. It runs with `sh check.sh` from the workspace root
inside the existing Bubblewrap sandbox (`--unshare-all`, network denied), and
exits 0 only when the task is complete. Requirements for a check:

- every task's check must contain the marker `# harness-eval hidden check:` so the
  suite can prove it knows where the check's text lives;
- it must fail on the pre-task workspace (the gate asserts this) and pass after a
  correct continuation;
- it must not need the network, and must ignore `.harness-eval-check.sh` (see
  below) — list the files it inspects rather than scanning everything;
- it must be quick: the whole offline gate has to stay within a few minutes.

The runner copies the check into the workspace as `.harness-eval-check.sh`
immediately before running it and removes it afterwards, so the check is never
in the workspace while the model runs. The eval gate asserts that no continuation
request contains the marker, the file name or the phrase "hidden check".

### `controls.json`

Optional. `{"pin":[1]}` pins the first turn, `{"exclude":[2]}` excludes the
second. Ordinals are 1-based turn positions in `history.jsonl`, and only turns a
compaction would otherwise absorb are eligible (the newest two retained turns are
not), so a control always measures something.

## Metrics

For one task × strategy × repetition:

| Metric | Definition |
| --- | --- |
| `requirements_retained_projection` | requirements whose matcher matches the post-compaction projection text |
| `requirements_retained_probe` | requirements whose matcher matches the probe answer |
| `requirements_total` | requirements the task declares |
| `files_recalled_projection` | declared modified paths that occur in the post-compaction projection text |
| `files_recalled_probe` | declared modified paths that occur in the probe answer |
| `files_total` | declared modified paths |
| `completion` | the hidden check exited 0 |

The **post-compaction projection text** is the product's own account of what the
model would see: the runner takes the live context report, walks its `included`
items in order, and renders each one from the journal — user text, assistant text
and its included tool calls with their arguments, tool-result text, and the
summary text of each included compaction entry. Instructions are skipped: they are
fixed scaffolding, identical in every arm, and never derived from the task's
history.

The **probe** is one extra turn after compaction, with a prompt fixed by the
harness (`pipeline::PROBE_PROMPT`), asking the model to restate the requirements
and the files changed so far. It is scored with the same matchers, so the two
numbers answer different questions: what the projection kept, and what the model
can still say after seeing it.

Offline, the probe answer is a script, so `requirements_retained_probe` measures
the harness plumbing, not the model. The gate proves the plumbing responds by
breaking a requirement out of a scripted summary and asserting retention falls.

## The pipeline

Per task × strategy × repetition:

1. Copy `workspace/` into a fresh scratch directory and create a fresh data root.
2. Create the session through `harness::wiring` — the same assembly the headless
   and terminal clients use, with the strategy recorded in its configuration and
   an independent summary model when the strategy needs one.
3. Replay `history.jsonl` through `SessionHandle`. Tool calls execute for real
   through the product's tool host and Bubblewrap.
4. Apply `controls.json` with `Command::SetContextControl`.
5. Run **one** compaction through the product commands: `PreviewCompaction`, then
   `CommitCompaction` of that exact preview. It is forced regardless of the budget
   trigger.
6. Score the post-compaction projection.
7. Ask the probe and score its answer.
8. Send `continue.md` and let the model work through the normal tool loop with a
   turn and inference cap.
9. Close the session, then run the hidden check in the sandbox against the
   scratch workspace.

### Auto-approval

The eval answers the product's approval prompts itself. The session's policy still
says `ask` for every mutation and every process — auto-approval is **not** a
policy value and does not exist in the product — the eval simply submits
`Decision::Allow` for each ticket it is shown, because it runs unattended inside a
scratch workspace it created. This lives only in `crates/eval/src/pipeline.rs` and
is scoped to that scratch workspace: the data root, the workspace and the sandbox
root are all per-cell, and nothing outside them is authorized.

## Records and aggregates

Each record carries the harness commit (`--commit`, normally
`git rev-parse HEAD`), the task fingerprint (a SHA-256 over the task directory's
sorted `path, length, sha256` manifest), the strategy name and version as written
into the committed compaction entry, the session and summary models, the
compaction policy fingerprint (which hashes the strategy name, its version and the
compaction configuration), the estimator method and version, token estimates
before and after, the metrics, per-stage durations, per-stage error codes and the
repetition index (`seed`).

Aggregates sum counts per strategy and per task. They contain no durations, no
identities and no floats, so **two runs of the same suite write byte-identical
`aggregate.json` and `aggregate.md`**. `cargo test -p harness-eval --test
offline_gate -- --ignored` asserts exactly that, and also that two runs' records
are equal apart from `durations_ms`.

## Adding a task

1. Create `eval/tasks/<name>/` with the files above.
2. Write the history so it makes at least two real tool calls and states at least
   two requirements in the first turn.
3. Write the matchers against the phrases you actually state, and check that each
   requirement's `text` satisfies its own matcher.
4. Write `check.sh` so it fails on `workspace/` as checked in, and passes after the
   continuation in `scripted/continue.jsonl`.
5. Run `cargo test -p harness-eval` (format) and
   `cargo test -p harness-eval --test offline_gate -- --ignored` (mechanics).
6. If the task should exercise pins or exclusions, add `controls.json`.
