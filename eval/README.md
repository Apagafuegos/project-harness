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
- A reply ends its turn. The **last** step of a turn must be `assistant`, and no
  earlier step in that turn may be: a `tool_call` is followed by another
  inference, which is how multi-call turns work. The loader rejects the other
  order, because a stray reply would silently push the remaining scripts into the
  next stage.
- `tool_call` runs the real built-in tool against the scratch workspace. The
  next step is the following inference, so a tool call and its result are always
  a real round trip.
- `assistant` is one completed response with one text block. `say` adds a text
  block before the call.
- A history command that is expected to **fail** must not let stdout and stderr
  interleave: the replay must be byte-comparable across plans and strategies. For
  example capture them separately —
  `out=$(sh main.sh 2>/dev/null); status=$?; printf 'stdout=<%s> status=%s\n' "$out" "$status"`.

Validation requires 4–8 turns, at least 2 tool calls and at least 2 requirements.
`scripted/continue.jsonl` uses the same vocabulary without `user`, and must end
with an `assistant` step.

**The history must make real, incomplete progress.** At least one coverable turn
(not one of the two retained turns, and not a controlled turn) writes or edits a
file for real, so the task is half-done at the pre-compaction boundary:
`modified_files.json` names exactly the files whose bytes the history changed, and
the task's hidden check still fails there. The continuation finishes the work. The
S02 gate asserts all of this by diffing the scratch workspace against the task's
pristine copy at the boundary:
`every_task_declares_exactly_the_files_its_history_changed`. A path that merely
exists is never accepted as evidence that it changed.

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
- it must fail on the pre-task workspace and on the workspace at the
  pre-compaction boundary, and pass after a correct continuation;
- it must not need the network;
- it must be quick: the whole offline gate has to stay within a few minutes;
- it must enforce the task's **allowed file set** (`find . -type f`, excluding
  only `.harness-eval-check.sh`), so an arbitrary extra file fails;
- a protected file must be compared against its **trusted original bytes**
  (including the final newline), not against a hash of whatever is there;
- output identity must be compared **byte for byte, including the final newline**.
  Command substitution strips trailing newlines, so capture with a sentinel:
  `out=$(sh main.sh; printf '\037'); out=${out%"$sentinel"}`;
- it must not depend on files it creates: the check runs in the workspace, so end
  by leaving it as the solution left it.

The runner publishes the check into the workspace as `.harness-eval-check.sh`
immediately before running it and removes it afterwards, so the check is never in
the workspace while the model runs. The eval gate asserts that no continuation
request contains the marker, the file name or the phrase "hidden check".

Publication is **exclusive and never follows a link**: the carrier name is
model-controlled, so a pre-existing file, symlink or hardlink there is refused
(`check_carrier_exists`) without touching its target, and the check does not run.
Only a file the runner created is removed, and a publication or cleanup failure
is never reported as a successful check.

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
   through the product's tool host and Bubblewrap. The **same scripted history is
   replayed in live mode too** — a comparison needs one fixed pre-compaction
   conversation — and only the summary, probe and continuation are answered by the
   live provider. The runner records the workspace at the pre-compaction boundary
   and the exact history requests, so the gate can show the replay is identical
   across plans and strategies and that exactly one compaction is committed.
4. Apply `controls.json` with `Command::SetContextControl`.
5. Run **one** compaction through the product commands: `PreviewCompaction`, then
   `CommitCompaction` of that exact preview. It is forced regardless of the budget
   trigger.
6. Score the post-compaction projection.
7. Ask the probe and score its answer (the harness's only model-facing question).
8. Send `continue.md` and let the model work through the normal tool loop with a
   turn and inference cap.
9. Close the session — **awaiting** `shutdown` on every exit path, success or
   failure — then run the hidden check in the sandbox against the scratch
   workspace.

Every stage failure is attributed to its own slot (`replay`, `control`,
`compaction`, `probe`, `continuation`), a timed-out stage reports
`stage_timeout`, and the hidden check runs only when every preceding stage
succeeded. A check that runs and fails is the `completion: false` metric, not an
error.

### Auto-approval

The eval answers the product's approval prompts itself. The session's policy still
says `ask` for every mutation and every process — auto-approval is **not** a
policy value and does not exist in the product — the eval simply submits
`Decision::Allow` for each ticket it is shown, because it runs unattended inside a
scratch workspace it created. This lives only in `crates/eval/src/pipeline.rs` and
is scoped to that scratch workspace: the data root, the workspace and the sandbox
root are all per-cell, and nothing outside them is authorized.

## Records and aggregates

Schema 2. Each record carries the harness commit, the task fingerprint (a SHA-256
over the task directory's sorted `path, length, sha256` manifest), the strategy
name and version as written into the committed compaction entry, the session and
summary models, the estimator method and version, token estimates before and
after, the metrics, per-stage durations, per-stage error codes and the repetition
index (`seed`).

Provenance has **two separate configuration identities** (M6 decision 15):

| Field | What it is |
| --- | --- |
| `session_configuration` | the exact digest of the session configuration artifact the committed compaction entry binds: instructions, execution policy and compaction policy |
| `compaction_policy` | the compaction-policy fingerprint: strategy name, its version and the compaction configuration |

They are not interchangeable. Changing a workspace instruction (for example
`AGENTS.md`) changes `session_configuration` and leaves `compaction_policy` alone;
changing the strategy or its configuration changes both.

The commit is never `unknown`. `--commit` must be a 40-character commit id, and
when it is absent the binary reads `HEAD` from the checkout itself (a `.git`
directory or a linked worktree's `gitdir`); if that fails it exits 2 and asks for
`--commit`. `scripts/eval.sh` passes `git rev-parse HEAD` automatically.

Aggregates sum counts per strategy and per task, and group by `compaction_policy`.
They contain no durations, no identities, no floats and **not** the raw
`session_configuration` digest, which embeds the ephemeral scratch path. So **two
runs of the same suite write byte-identical `aggregate.json` and
`aggregate.md`** — that aggregate, not the raw record, is the deterministic
comparison. `cargo test -p harness-eval --test offline_gate -- --ignored` asserts
it, and also that two records are equal apart from `durations_ms` and
`session_configuration`.

## Adding a task

1. Create `eval/tasks/<name>/` with the files above.
2. Write the history so it makes at least two real tool calls, states at least two
   requirements in the first turn, and **changes files for real in a coverable
   turn**, leaving the task half-done.
3. List exactly the changed paths in `modified_files.json`; the gate compares them
   with the workspace at the pre-compaction boundary.
4. Write the matchers against the phrases you actually state, and check that each
   requirement's `text` satisfies its own matcher.
5. Write `check.sh` so it fails on `workspace/` as checked in *and* on the
   half-done workspace, and passes after the continuation in
   `scripted/continue.jsonl`. Enforce the allowed file set, compare protected
   files with trusted bytes and compare output bytes including the final newline.
6. Run `cargo test -p harness-eval` (format) and
   `cargo test -p harness-eval --test offline_gate -- --ignored` (mechanics).
7. If the task should exercise pins or exclusions, add `controls.json`.
