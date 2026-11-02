# Context-compaction experiments: two bounded live matrices on DeepSeek Flash

Status: experiment and report for Milestone 6. Two matrices exist and are kept
separately:

- **First run (m6-06, 2026-11-01, source `cd14762`)** — historical evidence,
  unchanged: 4 of 12 cells measured, 8 failed mechanically because the
  summarizer request ended with an assistant turn and the provider rejected it.
- **Second run (m6-06b, 2026-11-02, source `02bd561`)** — one additional matrix
  after the accepted request fix: **12 of 12 cells measured, 0 mechanical
  errors**. The fix is validated live; no arm can be named the winner on these
  scores (see [Recommendation](#recommendation-bounded)).

The product default stays `rolling_summary` until the user explicitly picks one
(settled decision 4). The user's choice is the only thing still open.

Dates: 2026-11-01 (UTC) for the first run, 2026-11-02 (UTC) for the second.
Branches: `m6-experiments` and `m6-experiments-rerun`.

## The question

Milestone 6 compares three compaction strategies and the user chooses the product
default from measured evidence (settled decisions 3 and 4):

- `rolling_summary` — the current default: a model-generated summary replaces the
  covered turns.
- `observation_masking` — deterministic, model-free: keeps the conversation's
  shape and drops tool-result bodies.
- `structured_state` — an LLM summary anchored by deterministic extracts.

The measured question is narrow and deliberately mechanical: after **one** forced
compaction of the same scripted history, what does the post-compaction projection
still state, what can the model still say about it, does the task still finish,
and how large is the projection before and after? It is not a quality judgement
and it is not a statistical claim (see [Limits](#limits)).

## Protocol (shared by both runs)

One bounded matrix per run, all four tasks × all three strategies × one
repetition = 12 cells (settled decision 2). No Pro model, no model sweep, no
additional repetitions, no automatic retry, no tuning loop. One repetition is
exploratory by design.

Every cell replays the **same** scripted `history.jsonl` through the real runtime
and the real tool host (so the pre-compaction conversation and workspace are
identical across arms), applies the task's controls, runs exactly **one**
compaction through `PreviewCompaction` + `CommitCompaction`, scores the
post-compaction projection, asks one probe turn, sends `continue.md` through the
normal bounded tool loop, closes the session, and runs the task's hidden
`check.sh` in Bubblewrap. Only the summary, probe and continuation are answered
by the live provider; the history replay is scripted in both offline and live
mode.

## First run (m6-06) at `cd14762` — historical evidence

This section is the record of the first matrix, preserved as measured. Its data
is neither relabelled nor overwritten by the second run; its ruling was resolved
by the accepted fix and the second attempt (see
[How the first run's failure was resolved](#how-the-first-runs-failure-was-resolved)).

### Provenance (first run)

| Item | Value |
| --- | --- |
| Source commit measured (`harness_commit` in every record) | `cd14762332ad459617bd518ed9f55d08a0cab0ec` |
| Record schema | 2 (`crates/eval/src/record.rs`) |
| Session model | `deepseek/deepseek-flash` |
| Summary model (strategies that need one) | `deepseek/deepseek-flash` |
| Strategy versions | `1` for all three (as written in the committed compaction entry) |
| Token estimator | `heuristic bytes4-flat-media-v1` |
| Provider profile | `deepseek` (DeepSeek's Anthropic-compatible Messages endpoint, ADR 0024) |
| Runtime/strategy bounds | unchanged product defaults: `max_inferences_per_turn = 8`, compaction deadline 300 s, per-stage deadline 180 s (eval stage timeout) |
| Repetitions | 1 per cell |

Task suite (the checked-in four tasks, one per shape; fingerprint is the record's
SHA-256 over the task directory's sorted `path, length, sha256` manifest). The
suite and all four fingerprints are identical in both runs:

| Task | Shape | Task fingerprint |
| --- | --- | --- |
| `bugfix-with-test` | `bug_fix` | `bba58395cf9b5554ed8bfb6640c80f9be8503302953de8cd1340258950ab04ad` |
| `feature-style-rule` | `feature` | `a24fe083c6e215c5b0e7fe0e7e8a46c6f13d5004767f2da2d681f485db7f0424` |
| `multi-file-change` | `multi_file` | `5c9d7c792a317ab08433d49a9a1c4b8d5849e5781e37bcda871f59e899c47f21` |
| `refactor-constraint` | `refactor` | `82d578a2ebba9044d0206fea04d7a53e9856463a917f91f0c3e87ba102093617` |

Two configuration identities are recorded separately (decision 15) and are not
interchangeable:

- `session_configuration` — the exact digest of the session-configuration
  artifact the committed compaction entry binds. It embeds the ephemeral scratch
  workspace path, so it differs per cell and is never aggregated.
- `compaction_policy` — `context::compaction::policy_fingerprint`, which does not
  depend on that path. The aggregate groups by it. The offline and live runs of
  `observation_masking` produced the **same** fingerprint,
  `9a22ff70dd57a55d61600793bd2848f0a8ac1fd825c44ee3b94c296cbbd1be86`, which is
  one more sign that the model-free parts are deterministic. The first run's two
  model-based strategies are at policy version `1` (`93233b8a…` offline,
  `a7156547…` offline structured; live records for those arms never committed a
  compaction and therefore carry no policy fingerprint at all).

### Commands and the scripted estimate (first run)

Run from the accepted checkout at `cd14762`, on branch `m6-experiments`.

```sh
# 1. Offline mechanics matrix (no credentials, no network).
cargo run --locked -p harness-eval --bin harness-eval -- --offline \
    --out eval/results/m6-06-offline --commit "$(git rev-parse HEAD)"

# 2. Live dry run: scripted, prints the estimate, spends nothing, exits 2.
./scripts/eval.sh --live --provider deepseek --model deepseek-flash \
    --summary-model deepseek-flash --repetitions 1 \
    --out eval/results/m6-06-dry

# 3. The one paid matrix (same cells, explicit --yes, 45-minute shell bound).
timeout 2700 ./scripts/eval.sh --live --provider deepseek --model deepseek-flash \
    --summary-model deepseek-flash --repetitions 1 --yes \
    --out eval/results/m6-06-live
```

`scripts/eval.sh` adds `--commit "$(git rev-parse HEAD)"` itself, so every record
carries the source SHA above rather than a placeholder. `--out` was explicit for
every run so the three runs never share a directory.

Scripted estimate printed before `--yes` (verbatim):

```text
estimate: 12 cell(s), 56 model request(s), about 37423 prompt tokens (heuristic; excludes tool-loop retries and output tokens)
```

The estimate comes from a scripted dry run of the same cells: one summary request
per model-based cell, one probe request per cell, and the scripted continuation's
own inferences. It is order-of-magnitude by construction — the live tool loop is
unknown and output tokens are excluded — and it is **not** a bill and **not** the
actual token usage. Neither number is recorded by the harness: records carry only
heuristic estimates, no provider usage.

### Results (first run)

#### Offline matrix — mechanics only, not quality evidence

12/12 cells completed, every hidden check ran and passed, and all pre-compaction
boundaries were reached. All three arms score full retention and completion
because the summary, probe and continuation texts are fixtures; this only proves
the pipeline works and that all four hidden-check targets are available.

| strategy | runs | req. retention (projection) | req. retention (probe) | file recall (projection) | file recall (probe) | completion | tokens before | tokens after | failures |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `rolling_summary` | 4 | 12/12 (1.000) | 12/12 (1.000) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2317 | 1661 | 0 |
| `observation_masking` | 4 | 12/12 (1.000) | 12/12 (1.000) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2321 | 2459 | 0 |
| `structured_state` | 4 | 12/12 (1.000) | 12/12 (1.000) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2318 | 2338 | 0 |

Wall clock: 10.8 s for the 12 cells after the workspace build (the build is
cached from the 2 m 43 s first invocation, which failed at the sandbox probe
before running any cell; see [Deviations](#deviations).)

#### Live matrix — 4 measured cells, 8 mechanical failures

| strategy | runs | req. retention (projection) | req. retention (probe) | file recall (projection) | file recall (probe) | completion | tokens before | tokens after | failures |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `rolling_summary` | 4 | n/a | n/a | n/a | n/a | n/a | 0 | 0 | **4** |
| `observation_masking` | 4 | 12/12 (1.000) | 6/12 (0.500) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2321 | 2459 | 0 |
| `structured_state` | 4 | n/a | n/a | n/a | n/a | n/a | 0 | 0 | **4** |

Counts: **4 completed cells, 8 cells with a mechanical error, 0 completed cells
for the two model-based strategies.** `n/a` is a real absence, not a zero: a cell
that fails before the compaction is committed has no metrics and no token
estimates, and the aggregate prints `n/a` because its denominator is 0.

Per-task detail (every live cell; `tokens` is the record's own
`before → after` whole-projection estimate):

| task | shape | strategy | req. proj | req. probe | files proj | files probe | completion | tokens | total ms |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `bugfix-with-test` | `bug_fix` | `rolling_summary` | — | — | — | — | — | — | 788 |
| `bugfix-with-test` | `bug_fix` | `observation_masking` | 3/3 | 2/3 | 2/2 | 2/2 | pass | 651 → 637 | 8742 |
| `bugfix-with-test` | `bug_fix` | `structured_state` | — | — | — | — | — | — | 973 |
| `feature-style-rule` | `feature` | `rolling_summary` | — | — | — | — | — | — | 738 |
| `feature-style-rule` | `feature` | `observation_masking` | 3/3 | 2/3 | 1/1 | 1/1 | pass | 494 → 555 | 9901 |
| `feature-style-rule` | `feature` | `structured_state` | — | — | — | — | — | — | 900 |
| `multi-file-change` | `multi_file` | `rolling_summary` | — | — | — | — | — | — | 822 |
| `multi-file-change` | `multi_file` | `observation_masking` | 3/3 | 0/3 | 2/2 | 2/2 | pass | 639 → 664 | 7897 |
| `multi-file-change` | `multi_file` | `structured_state` | — | — | — | — | — | — | 772 |
| `refactor-constraint` | `refactor` | `rolling_summary` | — | — | — | — | — | — | 897 |
| `refactor-constraint` | `refactor` | `observation_masking` | 3/3 | 2/3 | 1/1 | 1/1 | pass | 537 → 603 | 10971 |
| `refactor-constraint` | `refactor` | `structured_state` | — | — | — | — | — | — | 1250 |

Every failed cell failed in the **same** stage with the **same** code:
`errors.compaction = "provider_failure"`; `replay`, `control`, `probe`,
`continuation` and `check` are all clear, so no probe, no continuation and no
hidden check ran. Those cells therefore have no completion value at all — absence,
not `false` — and their records carry no `compaction_policy`, no token estimate
and no metrics, because no compaction entry was ever committed. Failed-cell wall
clock (0.74–1.25 s) is dominated by the history replay (0.13–0.23 s) plus the
rejected compaction request; the record's `compaction_ms` is 0 because the
pipeline only stamps that duration once the preview succeeds.

Durations of the measured cells: replay 135–189 ms, compaction 11–12 ms (masking
makes no model call), probe 3.5–4.1 s, continuation 3.6–6.9 s, hidden check
59–159 ms. The whole live command — dry estimate plus the paid matrix — took
55.4 s of wall clock, well inside the 45-minute bound.

What the measured arm shows:

- The projection keeps all 12 requirements and all 6 modified-file paths after
  masking, and the probe keeps all 6 file paths.
- The probe restates fewer requirements than the projection: 6/12, and
  `multi-file-change` matches **none** of its three requirements. The task set is
  small, but this is the only live model signal in the matrix and it points
  the opposite way from the projection.
- Masking does not shrink the estimated projection on this suite:
  2321 → 2459 tokens (+6%). The covered spans here are mostly prose, not large
  tool outputs, so removing observation bodies has little to remove.
- Task completion after compaction is 4/4. Completion is the hidden check on the
  workspace the continuation left behind; it says the task finished, not that the
  strategy preserved anything.

The measured arm's non-model numbers are identical offline and live
(12/12 projection, 6/6 files, 2321 → 2459, same `compaction_policy`
fingerprint); only the probe differs (12/12 offline because it is a fixture, 6/12
live). That is the expected shape: the strategy is deterministic, the model is
not.

### How the first run's failure was resolved

The ruling below was requested at the end of m6-06. It was answered by the user
on 2026-11-01/02: **option (a)** — implement the request fix and, once accepted,
run one more 12-cell matrix (settled decision 17). That fix is
`m6-side-summary-request` @ `02bd561` (decision 16, ADR 0026 and ADR 0019's
`m6-side` amendment), and the second matrix below is its live validation. The
diagnosis is kept verbatim because it is the reason the fix exists.

The model-based strategies could not compact on DeepSeek Flash. `provider_failure`
in a compaction slot is the harness's collapsed code for *any* failed provider
stream; it is not by itself a diagnosis. What follows is the evidence collected
without running a second matrix.

1. **The credential, endpoint and model are fine.** The four model-free cells sent
   live probe and continuation requests to `deepseek/deepseek-flash` in the same
   run and succeeded (their probe scores differ from the offline fixtures, and the
   hidden checks passed). So the DeepSeek instance in this environment authenticates
   and answers.
2. **The failing request is the summarizer request, and the provider rejects it.**
   The exact outbound body was captured offline by running one cell through the
   real adapter with a loopback transport (no network, no cost). It is
   `model=deepseek-flash`, `max_tokens=2048`, `stream=true`, a 936-character
   `system` instruction, and 10 alternating `user`/`assistant` messages with three
   correctly paired `tool_use`/`tool_result` pairs, **no** `tools`, **no**
   `thinking` object — and it **ends with an `assistant` message** (the last
   covered turn's reply), because the summary source is the covered span with no
   trailing instruction turn.
3. **Replaying that body once through the product transport** to
   `https://api.deepseek.com/anthropic/v1/messages` (credential read from the
   configured environment by the adapter, never printed) returned the adapter's
   sanitized classification `InvalidRequest`, `provider_code = http_400`, in
   358 ms.
4. **One variable changed:** the same body with a single appended `user` text
   block — and nothing else — was **accepted** (HTTP 200, 289 ms). The adapter
   therefore is not at fault for the transport, the credential, the tool pairing
   or the token bound: the request shape DeepSeek refuses is the one that ends
   with an assistant turn (Anthropic-style prefill). DeepSeek's Anthropic-API
   guide documents tool-result replay and pairing but never documents assistant
   prefill.

The summarizer request is the only live request the harness builds that ends with
an assistant message; probe and continuation both end with the user prompt or a
tool result. The accepted fix makes both model-based strategies end that request
with one fixed, request-only user instruction (`ContextItem::Request`), which is
a prompt change and bumps their strategy versions from `1` to `2`; masking is
untouched and stays at `1` (ADR 0026, ADR 0019 `m6-side`). The first matrix's
`provider_failure` records stay in the evidence set below exactly as measured.

### Ruling requested at the end of the first run (historical)

Either (a) authorize an implementation change that makes the summarizer request
end with a user turn and, once accepted, a fresh 12-cell run, or (b) record the
two model-based strategies as unmeasured for M6 and decide the default from the
single measured arm. **The user chose (a)** on 2026-11-02 (settled decision 17);
the second run is the result.

## Second run (m6-06b) at `02bd561` — after the accepted request fix

### What changed between the runs

Only the summarizer request changed, and with it the two model-based strategy
versions:

| Item | First run | Second run |
| --- | --- | --- |
| Source commit | `cd14762` | `02bd561` |
| `rolling_summary` version | `1` | `2` |
| `structured_state` version | `1` | `2` |
| `observation_masking` version | `1` | `1` (unchanged) |
| Summary request ending | covered source's last turn (could be an `assistant` turn) | one fixed request-only `user` instruction after the source (decision 16) |
| Task suite, shapes, fingerprints | — | identical (table above) |
| Estimator, provider profile, bounds, repetitions | — | identical |

The two prompt versions are **not pooled** anywhere in this report: the first
run's model-based cells produced no measurements at all, and the second run's
model-based numbers were produced by version `2` under a different policy
fingerprint.

### Provenance (second run)

| Item | Value |
| --- | --- |
| Date | 2026-11-02 (UTC) |
| Branch | `m6-experiments-rerun` |
| Source commit measured (`harness_commit` in every record) | `02bd56187ec57404bed951db383144ec7c2ae459` |
| Record schema | 2 |
| Session model | `deepseek/deepseek-flash` |
| Summary model (strategies that need one) | `deepseek/deepseek-flash` |
| Strategy versions | `rolling_summary` 2, `observation_masking` 1, `structured_state` 2 |
| Token estimator | `heuristic bytes4-flat-media-v1` |
| Provider profile | `deepseek` (ADR 0024) |
| Runtime/strategy bounds | unchanged product defaults: `max_inferences_per_turn = 8`, compaction deadline 300 s, per-stage deadline 180 s (eval stage timeout) |
| Repetitions | 1 per cell |
| Attempted / measured / mechanical errors | 12 / 12 / 0 |
| Paid-command bound | `timeout 2700` (45 min); actual wall clock 3 m 13.6 s |

Compaction-policy fingerprints (`compaction_policy`, decision 15) for the second
run. Offline and live differ only because the summary model differs
(`mock/summary` vs `deepseek/deepseek-flash`), and masking's is the same one the
first run produced:

| strategy | version | offline policy fingerprint | live policy fingerprint |
| --- | --- | --- | --- |
| `rolling_summary` | 2 | `4e85090101c466f9…` | `8ceb6c94f1e73dd4…` |
| `observation_masking` | 1 | `9a22ff70dd57a55d…` (unchanged since the first run) | `9a22ff70dd57a55d…` |
| `structured_state` | 2 | `9d34ee3aadb29569…` | `07a95ceaed371afb…` |

Each of the 12 live cells has its own `session_configuration` digest (12 distinct
values, because the digest embeds the per-cell scratch path); those are never
aggregated. All 12 records carry `harness_commit =
02bd56187ec57404bed951db383144ec7c2ae459`, verified before launch and in the
records after the run.

### Commands and the estimate (second run)

Every command ran from the accepted checkout at `02bd561` through
`scripts/with-clean-fds.sh`, the descriptor-hygiene wrapper used by the first run
(see [Deviations](#deviations)).

```sh
# 1. Offline mechanics matrix into a fresh directory (no credentials, no network).
./scripts/with-clean-fds.sh ./scripts/eval.sh --offline \
    --out eval/results/m6-06b-offline

# 2. Dry estimate, no paid request: scripted run, prints the estimate, exits 2.
./scripts/with-clean-fds.sh ./scripts/eval.sh --live --provider deepseek \
    --model deepseek-flash --summary-model deepseek-flash --repetitions 1 \
    --out eval/results/m6-06b-dry

# 3. The single paid matrix (same cells, explicit --yes, 45-minute bound).
timeout 2700 ./scripts/with-clean-fds.sh ./scripts/eval.sh --live \
    --provider deepseek --model deepseek-flash --summary-model deepseek-flash \
    --repetitions 1 --yes --out eval/results/m6-06b-live
```

`scripts/eval.sh` supplied `--commit "$(git rev-parse HEAD)"` to all three, so the
records carry `02bd561` rather than a placeholder; `--out` was explicit so the
four run directories (`m6-06-*`, `m6-06b-*`) never share a directory. The dry
command exited 2, wrote no records and created no directory, exactly as designed.

Scripted estimate printed before the paid run (verbatim, and identical inside the
paid command's own built-in dry phase):

```text
estimate: 12 cell(s), 56 model request(s), about 37423 prompt tokens (heuristic; excludes tool-loop retries and output tokens)
```

As in the first run this is a coarse count from a scripted dry run of the same
cells — one summary request per model-based cell, one probe per cell, plus the
scripted continuation's own inferences. It is not a bill, not measured usage and
not the harness's own token metric; the records carry only heuristic estimates.

The paid command was invoked exactly once. There was no retry, no second attempt
for a failed cell, and no diagnostic provider request of any kind during or after
the matrix.

### Results — offline matrix (mechanics only, not quality evidence)

12/12 cells completed, every stage ran, every hidden check passed, 0 mechanical
errors. All three arms score full retention and completion because the summary,
probe and continuation texts are fixtures.

| strategy | version | runs | req. retention (projection) | req. retention (probe) | file recall (projection) | file recall (probe) | completion | tokens before | tokens after | failures |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `rolling_summary` | 2 | 4 | 12/12 (1.000) | 12/12 (1.000) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2317 | 1661 | 0 |
| `observation_masking` | 1 | 4 | 12/12 (1.000) | 12/12 (1.000) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2321 | 2459 | 0 |
| `structured_state` | 2 | 4 | 12/12 (1.000) | 12/12 (1.000) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2318 | 2338 | 0 |

The offline aggregate reproduces the first run's offline aggregate exactly except
for the recorded commit and the two bumped strategy versions (`diff` over the two
`aggregate.json` files shows only those lines; same for `aggregate.md`). Wall
clock 2 m 48 s, of which about 2 m 35 s was the cold workspace build; the 12
cells took 740–1420 ms each.

### Results — live matrix (12/12 measured, 0 mechanical errors)

| strategy | version | runs | req. retention (projection) | req. retention (probe) | file recall (projection) | file recall (probe) | completion | tokens before | tokens after | failures |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `rolling_summary` | 2 | 4 | 8/12 (0.667) | 5/12 (0.417) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2317 | 3003 | 0 |
| `observation_masking` | 1 | 4 | 12/12 (1.000) | 6/12 (0.500) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2321 | 2459 | 0 |
| `structured_state` | 2 | 4 | 12/12 (1.000) | 5/12 (0.417) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2318 | 3825 | 0 |

Counts: **12 attempted cells, 12 completed, 0 with a mechanical error, 0
retried.** Every record has `errors: {}`, every stage ran, and the hidden check
ran in all 12 cells (55–181 ms) and returned `completion: true` in all 12. There
are no missing denominators in this run, so nothing is `n/a` here; the first
run's `n/a` cells stay absent as reported.

Per-task detail, every live cell in the second run (`tokens` is the record's own
`before → after` whole-projection estimate; `total` is the record's whole-cell
duration):

| task | shape | strategy | req. proj | req. probe | files proj | files probe | completion | tokens | total ms |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `bugfix-with-test` | `bug_fix` | `rolling_summary` | 3/3 | 2/3 | 2/2 | 2/2 | pass | 650 → 722 | 14291 |
| `bugfix-with-test` | `bug_fix` | `observation_masking` | 3/3 | 2/3 | 2/2 | 2/2 | pass | 651 → 637 | 8701 |
| `bugfix-with-test` | `bug_fix` | `structured_state` | 3/3 | 1/3 | 2/2 | 2/2 | pass | 650 → 1011 | 12133 |
| `feature-style-rule` | `feature` | `rolling_summary` | 2/3 | 2/3 | 1/1 | 1/1 | pass | 493 → 681 | 15794 |
| `feature-style-rule` | `feature` | `observation_masking` | 3/3 | 2/3 | 1/1 | 1/1 | pass | 494 → 555 | 7982 |
| `feature-style-rule` | `feature` | `structured_state` | 3/3 | 2/3 | 1/1 | 1/1 | pass | 493 → 941 | 17375 |
| `multi-file-change` | `multi_file` | `rolling_summary` | 0/3 | 0/3 | 2/2 | 2/2 | pass | 638 → 753 | 15245 |
| `multi-file-change` | `multi_file` | `observation_masking` | 3/3 | 0/3 | 2/2 | 2/2 | pass | 639 → 664 | 18477 |
| `multi-file-change` | `multi_file` | `structured_state` | 3/3 | 0/3 | 2/2 | 2/2 | pass | 638 → 948 | 13280 |
| `refactor-constraint` | `refactor` | `rolling_summary` | 3/3 | 1/3 | 1/1 | 1/1 | pass | 536 → 847 | 13193 |
| `refactor-constraint` | `refactor` | `observation_masking` | 3/3 | 2/3 | 1/1 | 1/1 | pass | 537 → 603 | 22603 |
| `refactor-constraint` | `refactor` | `structured_state` | 3/3 | 2/3 | 1/1 | 1/1 | pass | 537 → 925 | 23102 |

Per-task caveats, stated for every cell rather than only the failing ones:

- `bugfix-with-test` (3 requirements, `slug.sh` + `test.sh`): every arm kept all
  three requirement phrasings in the projection, but neither model arm restated
  the "existing two tests must keep passing" wording in the probe
  (`structured_state` 1/3 is its lowest probe cell).
- `feature-style-rule` (3 requirements, `greet.sh`): `rolling_summary` is the
  **only** cell in the second run whose projection lost a requirement (2/3). The
  record keeps counts, not the projection text, so *which* requirement was worded
  differently cannot be audited without another paid run.
- `multi-file-change` (3 requirements, `lib/util.sh` + `main.sh`): the harshest
  cell. `rolling_summary`'s projection matched **none** of the three requirement
  phrasings, and **all three arms** matched none of them in the probe (0/3 each),
  which reproduces the first run's masking probe exactly. The matchers are literal
  strings such as `do not rename add`, so a summary or a probe answer that says
  "left `add` alone" scores 0 without being wrong.
- `refactor-constraint` (3 requirements, `report.sh`): all three projections kept
  all three requirements; probes matched 1–2 of 3, the lowest being
  `rolling_summary` at 1/3.

Durations (per-strategy ranges over the four live cells; `compaction` is the
model-based arm's summary request plus assembly, and is the model-free arm's
rendering only):

| strategy | replay | compaction | probe | continuation | hidden check | whole cell |
| --- | --- | --- | --- | --- | --- | --- |
| `rolling_summary` | 125–273 ms | 3 273–6 053 ms | 3 115–4 759 ms | 4 589–6 423 ms | 55–168 ms | 13 193–15 794 ms |
| `observation_masking` | 122–244 ms | 11–12 ms | 2 620–6 393 ms | 4 527–15 426 ms | 69–145 ms | 7 982–22 603 ms |
| `structured_state` | 115–290 ms | 3 726–5 816 ms | 2 448–3 847 ms | 4 639–12 754 ms | 78–181 ms | 12 133–23 102 ms |

The whole paid command — the built-in scripted dry phase, the estimate, and all
12 live cells — took 3 m 13.6 s of wall clock, inside the 45-minute bound. The
records show no retry and no missing stage (every cell has a full
`durations_ms` set and `errors: {}`); they do not count requests, so the 56-request
estimate is not confirmed by any recorded usage figure.

### Comparison within the second matrix

Every number below is from this matrix only; the first run contributes no
model-based measurement and its masking arm is compared separately below.

- **Mechanical completion is saturated and therefore not a discriminator.**
  12/12 cells ran every stage and 4/4 hidden checks passed in each arm. Mechanical
  success says the pipeline works; it does not say an arm preserved anything.
- **File recall is saturated too.** 6/6 projection and 6/6 probe in every arm, on
  all four tasks.
- **Projection retention separates the arms by four requirements.**
  `observation_masking` and `structured_state` each kept 12/12;
  `rolling_summary` kept 8/12 (one in `feature-style-rule`, three in
  `multi-file-change`). That is the only projection difference in the matrix.
- **The probe does not separate them meaningfully.** 6/12 (`masking`), 5/12
  (`rolling_summary`), 5/12 (`structured_state`) out of 12 possible requirement
  matches. One requirement of difference is well inside what the lexical matcher,
  one repetition and four small tasks can resolve, and `multi-file-change`
  contributes 0/3 to every arm.
- **No arm reduced the estimated projection on this suite; all three grew it.**
  `rolling_summary` 2317 → 3003 (+686, +29.6%), `observation_masking`
  2321 → 2459 (+138, +6.0%), `structured_state` 2318 → 3825 (+1507, +65.0%).
  These are short, prose-heavy histories, so a summary plus the two retained
  turns costs more than the span it replaces, and masking has almost no tool
  bodies to drop. Cost does not favour any arm here either.
- **`structured_state` keeps every requirement in the projection but grows the
  projection most**, and its probe score is not better than masking's
  (5/12 vs 6/12). Its deterministic extract block is the reason the projection
  keeps all three requirements of `multi-file-change` where `rolling_summary`
  keeps none.
- **Masking reproduced its first-run behaviour exactly**: the same
  `compaction_policy` fingerprint, the same per-task token estimates
  (651 → 637, 494 → 555, 639 → 664, 537 → 603), the same projection counts, the
  same 6/12 probe total and the same per-task probe counts (2, 2, 0, 2). Its
  non-model numbers are, as designed, deterministic; the probe agreeing again is
  a consistency observation, not a controlled repetition.

Mechanical success (stages + hidden check) and task success (the check's verdict)
are reported separately throughout: both are 12/12 here, and neither is evidence
about what the model could still say.

## Limits

Every limit of the first run applies to the second, and the second run adds two
of its own (the last two items).

- **Small N.** Four tasks × three strategies × one repetition per run; one task
  can move a total. Nothing here is a statistical claim. The second run has the
  same shape, so it doubles no denominator: 12 cells is still 12 cells.
- **Fixed, prose-heavy tasks.** The four checked-in tasks are short dialogues with
  little tool output. Their histories do not exercise the case masking is designed
  for (large stale observations), which is why its estimated projection grows
  rather than shrinks here.
- **Lexical matchers.** Retention and recall are normalized literal substrings
  over the scored text. A summary can state a requirement in other words and score
  0, or quote it and score 1 without preserving its meaning. Punctuation is
  significant; there is no regex or semantic matcher. The `multi-file-change`
  probe score of 0/3 in **every** arm is the clearest instance: the answers may
  well restate the requirements in words the matchers do not know.
- **Heuristic token estimates.** `bytes4-flat-media-v1` divides bytes by four; it
  is not a tokenizer. `before`/`after` are whole-projection estimates from the
  report, and the dry estimate is not billing and not measured usage. The harness
  records no provider usage at all.
- **Probe effects.** One extra turn with a fixed prompt, scored with the same
  matchers. It measures what the model can still say, not correctness, and a
  second attempt is not measured.
- **Lossy memory.** A summary is lossy by definition. Completion after compaction
  can succeed because the retained turns and the workspace carry the task even
  when the projection lost a requirement — the two are reported separately and
  must not be conflated.
- **No LLM judge.** Scoring is deterministic on purpose (settled decision 11).
- **No workspace rollback.** The harness never reverts a workspace; completion is
  the hidden check on the workspace the continuation left. A half-fix reports
  `completion: false`, and a failed cell leaves its scratch tree behind.
- **One compaction, one probe.** Each cell measures the first compaction, not a
  rolling sequence.
- **Offline proves mechanics, not quality.** The offline summary, probe and
  continuation texts are fixtures. Its full scores and its token drops (for
  example `rolling_summary` 2317 → 1661) reflect fixture lengths and must not be
  read as live behaviour.
- **The second run's texts are not retained.** The paid command exited 0, so the
  harness removed its scratch trees; no live journal, projection text or probe
  answer survives, and records carry counts only. "Which requirement was worded
  differently" cannot be answered from the committed evidence, and answering it
  would need another paid run, which is not authorized.
- **No per-cell retry, by design.** A cell that had failed would have been kept as
  a failure. None failed, but the matrix therefore also says nothing about
  variance across attempts.

## Evidence

Every file below is committed with this report; `eval/results/` is git-ignored by
design, so these are force-added explicitly and nothing else from it is.

First run — offline matrix (mechanics, label: offline):

- [eval/results/m6-06-offline/aggregate.json](../eval/results/m6-06-offline/aggregate.json)
- [eval/results/m6-06-offline/aggregate.md](../eval/results/m6-06-offline/aggregate.md)
- [eval/results/m6-06-offline/records/bugfix-with-test.observation_masking.0.json](../eval/results/m6-06-offline/records/bugfix-with-test.observation_masking.0.json)
- [eval/results/m6-06-offline/records/bugfix-with-test.rolling_summary.0.json](../eval/results/m6-06-offline/records/bugfix-with-test.rolling_summary.0.json)
- [eval/results/m6-06-offline/records/bugfix-with-test.structured_state.0.json](../eval/results/m6-06-offline/records/bugfix-with-test.structured_state.0.json)
- [eval/results/m6-06-offline/records/feature-style-rule.observation_masking.0.json](../eval/results/m6-06-offline/records/feature-style-rule.observation_masking.0.json)
- [eval/results/m6-06-offline/records/feature-style-rule.rolling_summary.0.json](../eval/results/m6-06-offline/records/feature-style-rule.rolling_summary.0.json)
- [eval/results/m6-06-offline/records/feature-style-rule.structured_state.0.json](../eval/results/m6-06-offline/records/feature-style-rule.structured_state.0.json)
- [eval/results/m6-06-offline/records/multi-file-change.observation_masking.0.json](../eval/results/m6-06-offline/records/multi-file-change.observation_masking.0.json)
- [eval/results/m6-06-offline/records/multi-file-change.rolling_summary.0.json](../eval/results/m6-06-offline/records/multi-file-change.rolling_summary.0.json)
- [eval/results/m6-06-offline/records/multi-file-change.structured_state.0.json](../eval/results/m6-06-offline/records/multi-file-change.structured_state.0.json)
- [eval/results/m6-06-offline/records/refactor-constraint.observation_masking.0.json](../eval/results/m6-06-offline/records/refactor-constraint.observation_masking.0.json)
- [eval/results/m6-06-offline/records/refactor-constraint.rolling_summary.0.json](../eval/results/m6-06-offline/records/refactor-constraint.rolling_summary.0.json)
- [eval/results/m6-06-offline/records/refactor-constraint.structured_state.0.json](../eval/results/m6-06-offline/records/refactor-constraint.structured_state.0.json)

First run — live matrix (include the eight failed records):

- [eval/results/m6-06-live/aggregate.json](../eval/results/m6-06-live/aggregate.json)
- [eval/results/m6-06-live/aggregate.md](../eval/results/m6-06-live/aggregate.md)
- [eval/results/m6-06-live/records/bugfix-with-test.observation_masking.0.json](../eval/results/m6-06-live/records/bugfix-with-test.observation_masking.0.json)
- [eval/results/m6-06-live/records/bugfix-with-test.rolling_summary.0.json](../eval/results/m6-06-live/records/bugfix-with-test.rolling_summary.0.json)
- [eval/results/m6-06-live/records/bugfix-with-test.structured_state.0.json](../eval/results/m6-06-live/records/bugfix-with-test.structured_state.0.json)
- [eval/results/m6-06-live/records/feature-style-rule.observation_masking.0.json](../eval/results/m6-06-live/records/feature-style-rule.observation_masking.0.json)
- [eval/results/m6-06-live/records/feature-style-rule.rolling_summary.0.json](../eval/results/m6-06-live/records/feature-style-rule.rolling_summary.0.json)
- [eval/results/m6-06-live/records/feature-style-rule.structured_state.0.json](../eval/results/m6-06-live/records/feature-style-rule.structured_state.0.json)
- [eval/results/m6-06-live/records/multi-file-change.observation_masking.0.json](../eval/results/m6-06-live/records/multi-file-change.observation_masking.0.json)
- [eval/results/m6-06-live/records/multi-file-change.rolling_summary.0.json](../eval/results/m6-06-live/records/multi-file-change.rolling_summary.0.json)
- [eval/results/m6-06-live/records/multi-file-change.structured_state.0.json](../eval/results/m6-06-live/records/multi-file-change.structured_state.0.json)
- [eval/results/m6-06-live/records/refactor-constraint.observation_masking.0.json](../eval/results/m6-06-live/records/refactor-constraint.observation_masking.0.json)
- [eval/results/m6-06-live/records/refactor-constraint.rolling_summary.0.json](../eval/results/m6-06-live/records/refactor-constraint.rolling_summary.0.json)
- [eval/results/m6-06-live/records/refactor-constraint.structured_state.0.json](../eval/results/m6-06-live/records/refactor-constraint.structured_state.0.json)

Second run — offline matrix (mechanics, label: offline):

- [eval/results/m6-06b-offline/aggregate.json](../eval/results/m6-06b-offline/aggregate.json)
- [eval/results/m6-06b-offline/aggregate.md](../eval/results/m6-06b-offline/aggregate.md)
- [eval/results/m6-06b-offline/records/bugfix-with-test.observation_masking.0.json](../eval/results/m6-06b-offline/records/bugfix-with-test.observation_masking.0.json)
- [eval/results/m6-06b-offline/records/bugfix-with-test.rolling_summary.0.json](../eval/results/m6-06b-offline/records/bugfix-with-test.rolling_summary.0.json)
- [eval/results/m6-06b-offline/records/bugfix-with-test.structured_state.0.json](../eval/results/m6-06b-offline/records/bugfix-with-test.structured_state.0.json)
- [eval/results/m6-06b-offline/records/feature-style-rule.observation_masking.0.json](../eval/results/m6-06b-offline/records/feature-style-rule.observation_masking.0.json)
- [eval/results/m6-06b-offline/records/feature-style-rule.rolling_summary.0.json](../eval/results/m6-06b-offline/records/feature-style-rule.rolling_summary.0.json)
- [eval/results/m6-06b-offline/records/feature-style-rule.structured_state.0.json](../eval/results/m6-06b-offline/records/feature-style-rule.structured_state.0.json)
- [eval/results/m6-06b-offline/records/multi-file-change.observation_masking.0.json](../eval/results/m6-06b-offline/records/multi-file-change.observation_masking.0.json)
- [eval/results/m6-06b-offline/records/multi-file-change.rolling_summary.0.json](../eval/results/m6-06b-offline/records/multi-file-change.rolling_summary.0.json)
- [eval/results/m6-06b-offline/records/multi-file-change.structured_state.0.json](../eval/results/m6-06b-offline/records/multi-file-change.structured_state.0.json)
- [eval/results/m6-06b-offline/records/refactor-constraint.observation_masking.0.json](../eval/results/m6-06b-offline/records/refactor-constraint.observation_masking.0.json)
- [eval/results/m6-06b-offline/records/refactor-constraint.rolling_summary.0.json](../eval/results/m6-06b-offline/records/refactor-constraint.rolling_summary.0.json)
- [eval/results/m6-06b-offline/records/refactor-constraint.structured_state.0.json](../eval/results/m6-06b-offline/records/refactor-constraint.structured_state.0.json)

Second run — live matrix (all 12 measured cells):

- [eval/results/m6-06b-live/aggregate.json](../eval/results/m6-06b-live/aggregate.json)
- [eval/results/m6-06b-live/aggregate.md](../eval/results/m6-06b-live/aggregate.md)
- [eval/results/m6-06b-live/records/bugfix-with-test.observation_masking.0.json](../eval/results/m6-06b-live/records/bugfix-with-test.observation_masking.0.json)
- [eval/results/m6-06b-live/records/bugfix-with-test.rolling_summary.0.json](../eval/results/m6-06b-live/records/bugfix-with-test.rolling_summary.0.json)
- [eval/results/m6-06b-live/records/bugfix-with-test.structured_state.0.json](../eval/results/m6-06b-live/records/bugfix-with-test.structured_state.0.json)
- [eval/results/m6-06b-live/records/feature-style-rule.observation_masking.0.json](../eval/results/m6-06b-live/records/feature-style-rule.observation_masking.0.json)
- [eval/results/m6-06b-live/records/feature-style-rule.rolling_summary.0.json](../eval/results/m6-06b-live/records/feature-style-rule.rolling_summary.0.json)
- [eval/results/m6-06b-live/records/feature-style-rule.structured_state.0.json](../eval/results/m6-06b-live/records/feature-style-rule.structured_state.0.json)
- [eval/results/m6-06b-live/records/multi-file-change.observation_masking.0.json](../eval/results/m6-06b-live/records/multi-file-change.observation_masking.0.json)
- [eval/results/m6-06b-live/records/multi-file-change.rolling_summary.0.json](../eval/results/m6-06b-live/records/multi-file-change.rolling_summary.0.json)
- [eval/results/m6-06b-live/records/multi-file-change.structured_state.0.json](../eval/results/m6-06b-live/records/multi-file-change.structured_state.0.json)
- [eval/results/m6-06b-live/records/refactor-constraint.observation_masking.0.json](../eval/results/m6-06b-live/records/refactor-constraint.observation_masking.0.json)
- [eval/results/m6-06b-live/records/refactor-constraint.rolling_summary.0.json](../eval/results/m6-06b-live/records/refactor-constraint.rolling_summary.0.json)
- [eval/results/m6-06b-live/records/refactor-constraint.structured_state.0.json](../eval/results/m6-06b-live/records/refactor-constraint.structured_state.0.json)

The dry runs wrote no records (they exit before a run is written) and the scratch
trees they and the live runs kept are deliberately not committed. The second
run's scratch trees were removed by the harness itself after its clean exit, as
noted under [Limits](#limits).

## Verification performed

The aggregate files are generated by the harness; they were independently
reconciled against the raw records with the same asserting audit the first run
used, with only the two roots and the expected source SHA adapted. Python 3 is
used only as the auditor here; the project pipeline is Rust and unchanged.

```sh
python3 - <<'PY'
import json, glob, collections, itertools
keys = ["requirements_retained_projection","requirements_retained_probe",
        "requirements_total","files_recalled_projection","files_recalled_probe",
        "files_total","completions","checkable","failures",
        "tokens_before","tokens_after"]
for root in ["eval/results/m6-06b-offline","eval/results/m6-06b-live"]:
    recs = [json.load(open(p)) for p in sorted(glob.glob(root+"/records/*.json"))]
    agg = json.load(open(root+"/aggregate.json"))
    per = collections.defaultdict(collections.Counter)
    pertask = collections.defaultdict(lambda: collections.defaultdict(collections.Counter))
    for r in recs:
        c = per[r["strategy"]]; c["runs"] += 1
        pertask[r["task"]][r["strategy"]]["runs"] += 1
        m = r.get("metrics")
        if m:
            for k in keys[:6]:
                c[k] += m[k]; pertask[r["task"]][r["strategy"]][k] += m[k]
            if m.get("completion") is not None:
                c["checkable"] += 1; c["completions"] += int(m["completion"])
                pertask[r["task"]][r["strategy"]]["checkable"] += 1
                pertask[r["task"]][r["strategy"]]["completions"] += int(m["completion"])
        t = r.get("tokens")
        if t:
            c["tokens_before"] += t["before"]; c["tokens_after"] += t["after"]
            pertask[r["task"]][r["strategy"]]["tokens_before"] += t["before"]
            pertask[r["task"]][r["strategy"]]["tokens_after"] += t["after"]
        ok = all(r["errors"].get(k) is None for k in
                 ["replay","control","compaction","probe","continuation","check"]) and m is not None
        c["failures"] += 0 if ok else 1
        pertask[r["task"]][r["strategy"]]["failures"] += 0 if ok else 1
    bad = [(e["strategy"], k, e[k], per[e["strategy"]][k])
           for e in agg["strategies"] for k in ["runs"] + keys if e[k] != per[e["strategy"]][k]]
    bad += [(t["task"], e["strategy"], k, e[k], pertask[t["task"]][e["strategy"]][k])
            for t in agg["tasks"] for e in t["strategies"] for k in ["runs"] + keys
            if e[k] != pertask[t["task"]][e["strategy"]][k]]
    combos = collections.Counter((r["task"], r["strategy"], r["repetition"]) for r in recs)
    expected = set(itertools.product(
        ["bugfix-with-test", "feature-style-rule", "multi-file-change", "refactor-constraint"],
        ["rolling_summary", "observation_masking", "structured_state"], [0]))
    assert set(combos) == expected and all(v == 1 for v in combos.values())
    assert len(recs) == agg["runs"] == 12 and not bad, bad
    assert all(r["harness_commit"] == agg["harness_commit"] ==
               "02bd56187ec57404bed951db383144ec7c2ae459" for r in recs)
    print(root, "records:", len(recs), "aggregate.runs:", agg["runs"],
          "mismatches:", bad or "none",
          "duplicate/missing cells:", [k for k, v in combos.items() if v != 1] or "none")
PY
```

Result:

```text
eval/results/m6-06b-offline records: 12 aggregate.runs: 12 mismatches: none duplicate/missing cells: none
eval/results/m6-06b-live records: 12 aggregate.runs: 12 mismatches: none duplicate/missing cells: none
```

Both runs therefore have 12 records, `aggregate.runs = 12`, every per-strategy and
per-task total in `aggregate.json` matching the recomputed value, every
`(task, strategy, repetition)` combination appearing exactly once, and every
record carrying `02bd561`. Additional checks made on the same evidence:

- Strategy versions in the live records are `2`, `1`, `2` for
  `rolling_summary`, `observation_masking`, `structured_state`; masking's
  `compaction_policy` equals the first run's `9a22ff70…`, and the two model-based
  arms carry new fingerprints because their version changed.
- Every live record has `errors: {}` and a full `durations_ms` set, so all six
  stages ran in all 12 cells.
- The only `diff` between the first and second offline aggregates is the commit
  line and the two model-based `strategy_version` values — the mechanics numbers
  are reproduced exactly.
- The committed result files contain no credential, no environment value and no
  scratch path (grep for `sk-`, `api_key`, `bearer`, `DEEPSEEK_API` and `/tmp/`
  returns nothing); records carry counts, digests and durations only.
- The allowlist was inspected before staging: exactly `aggregate.json`,
  `aggregate.md` and the 12 `records/*.json` in each `m6-06b` directory, and
  nothing else from `eval/results/`.

## Deviations

1. **Descriptor hygiene in the agent tool host.** The first run's first offline
   invocation never reached a cell: the product's sandbox descriptor audit refused
   to start because the harness process had inherited a non-close-on-exec
   descriptor (`fd=49 → /dev/ptmx`) from the tool host. That refusal is correct
   behaviour and was not patched. All runs in both steps were launched through
   `scripts/with-clean-fds.sh`, which closes inherited descriptors before `exec`,
   so the harness saw only standard input, output and error. Nothing in the
   repository was changed for this, and the wrapper affects no measurement except
   that the sandbox can start.
2. **Two diagnostic provider requests outside the first matrix.** After the first
   paid matrix had already failed, two requests were made to identify the cause:
   one replay of the captured summarizer body (rejected, HTTP 400) and one
   identical body with a single appended `user` turn (accepted). They are not
   matrix cells, they are not retries of any cell, no result record was produced
   from them, and they are excluded from every count and aggregate above. One of
   them generated a summary that was dropped unread and is not used as data. The
   brief's one-matrix limit was respected: no second matrix was run in that step.
   **The second run made no diagnostic request of any kind.**
3. **The dry estimate was produced twice in the first run.** Once in the
   standalone dry run (exit 2, no records) and once by the paid command's own
   built-in dry phase, which always runs before `--yes`. Both printed the same
   estimate; the same is true of the second run, whose standalone dry command
   printed the identical line and created no directory.
4. **`--out` was passed explicitly** for every run to keep the results directories
   apart; the default would otherwise be timestamped.
5. **Second run: the clean exit removed the scratch trees.** Because the paid
   command exited 0, the harness deleted both its built-in dry phase's scratch
   root and the live run's, so no live journal, projection rendering or probe
   answer was retained. That is the harness's designed behaviour for a clean run
   (a failed run keeps its tree and says where it is); it is why the committed
   evidence is counts only. Nothing was re-run to recover the texts.
6. **Second run: the offline matrix ran on a cold build.** Its 2 m 48 s wall clock
   includes about 2 m 35 s of workspace compilation; the 12 cells themselves took
   740–1420 ms each.

## Recommendation (bounded)

No winner can be named from the second matrix, and none is named here. Every
comparison that would decide the default is either saturated, incomplete by
construction (one repetition, four tasks) or measured by a lexical matcher:

- Mechanical completion (12/12) and file recall (6/6 everywhere) are saturated, so
  they cannot separate the arms.
- Projection retention separates them by four requirements out of twelve
  (12/12, 12/12, 8/12) and the probe by one (6/12, 5/12, 5/12) — inside the noise
  of four tasks, one repetition and literal substring matching, with one task
  (`multi-file-change`) scoring 0/3 in every arm.
- No arm reduced the estimated projection on this suite; all three grew it
  (`rolling_summary` +29.6%, `observation_masking` +6.0%, `structured_state`
  +65.0%), so cost does not favour any of them either.

What the evidence does support, stated as narrowly as possible:

- The accepted request fix works live: both model-based strategies compacted,
  probed and completed all four tasks with no mechanical error, so the first run's
  eight failures are a fixed defect and not a property of the strategies.
- `observation_masking` is the only arm whose compaction is deterministic and
  model-free, and it reproduced its first-run numbers exactly (same policy
  fingerprint, tokens, projection counts and probe counts). It also has the second
  highest probe total (6/12).
- `structured_state` kept every requirement in the projection (12/12) and grew the
  projection the most (+65%); its probe total (5/12) did not exceed masking's.
- `rolling_summary`, today's default, is the only arm that lost requirement
  phrasings in the projection (8/12; one task lost all three) and tied the lowest
  probe total (5/12). That is a signal against it, not a verdict: a summary is
  meant to be lossy, the matchers are literal, and the tasks are four short
  dialogues.
- A defensible next step, if the user wants a stronger basis for the choice, is a
  larger suite and more repetitions — not another paid run of this one, which is
  not authorized.

**The default therefore stays `rolling_summary` and the choice remains the
user's** (settled decision 4). Nothing in this step changed the default, the
implementation, the README, the roadmap or any acceptance record.

## Handoff

- Branch/head: `m6-experiments-rerun`, pushed to `origin`. The measured source
  commit of the second matrix is `02bd56187ec57404bed951db383144ec7c2ae459`; the
  first matrix's measured source stays `cd14762332ad459617bd518ed9f55d08a0cab0ec`.
- Commands: offline matrix, dry estimate (exit 2, no paid request) and exactly one
  paid matrix, all through `scripts/with-clean-fds.sh` and `timeout 2700`; the
  estimate was `12 cell(s), 56 model request(s), about 37423 prompt tokens` and
  the paid matrix took 3 m 13.6 s.
- Counts: second matrix 12 attempted, 12 measured, 0 mechanical errors, 0
  retries, 0 diagnostic requests; first matrix preserved at 4 measured /
  8 `provider_failure` / 0 retried.
- Evidence: the three `m6-06b` result directories are linked above; the first
  run's files are unchanged.
- Limits: one repetition of four small, prose-heavy tasks, lexical matchers,
  heuristic token estimates, one probe turn, no LLM judge, no workspace rollback,
  and no retained projection/probe texts for the second run.
- Recommendation: no winner; the bounded reading is that the fix is validated,
  masking is the deterministic arm that reproduced, and `rolling_summary` shows
  the weakest projection retention — with the caveats above.
- **Open question for the user:** which strategy should be the product default for
  M6? It stays `rolling_summary` until you choose explicitly.
