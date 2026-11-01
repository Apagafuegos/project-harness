# Context-compaction experiments: live run on DeepSeek Flash

Status: experiment and report for Milestone 6, step 06 (m6-06). The live matrix
is **partially blocked**: 4 of 12 cells produced measurements and 8 failed
mechanically at compaction because the selected provider rejects the summarizer
request. The product default stays `rolling_summary` until the user picks one
(settled decision 4); the open ruling is in [Ruling needed](#ruling-needed).

Date: 2026-11-01 (UTC). Branch `m6-experiments`.

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

## Provenance

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
SHA-256 over the task directory's sorted `path, length, sha256` manifest):

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
  one more sign that the model-free parts are deterministic.

## Protocol

One bounded matrix, all four tasks × all three strategies × one repetition =
12 cells (settled decision 2 and the m6-06 brief). No Pro model, no model sweep,
no additional repetitions, no automatic retry, no tuning loop. One repetition is
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

## Commands and the scripted estimate

Run from the accepted checkout at `cd14762` (source SHA above), on branch
`m6-experiments`.

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

## Results

### Offline matrix — mechanics only, not quality evidence

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

### Live matrix — 4 measured cells, 8 mechanical failures

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
not `false`. Failed-cell wall clock (0.74–1.25 s) is dominated by the history
replay (0.13–0.23 s) plus the rejected compaction request (the same request took
358 ms in the diagnostic replay below); the record's `compaction_ms` is 0 because
the pipeline only stamps that duration once the preview succeeds.

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

## Ruling needed

The model-based strategies cannot compact on DeepSeek Flash. `provider_failure` in a compaction slot is the harness's collapsed code for *any*
failed provider stream; it is not by itself a diagnosis. What follows is the
evidence collected without running a second matrix.

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
tool result. Fixing this means changing how core composes the summary request
(and/or a documented profile property), which is outside this step's scope: m6-06
must not edit core, providers, CLI, strategies or defaults. No second paid matrix
was run, and none will be run without a ruling.

Requested ruling: either (a) authorize an implementation change that makes the
summarizer request end with a user turn and, once accepted, a fresh 12-cell run,
or (b) record the two model-based strategies as unmeasured for M6 and decide the
default from the single measured arm.

## Recommendation (bounded)

No winner can be named from this matrix. Eight cells are missing and the four
that ran measure a single strategy: the two model-based strategies have no
projection retention, no file recall, no completion and no token estimate at all,
so comparing them with masking would invent numbers. The offline matrix' full
scores are fixture-driven and must not be used as a tie-breaker.

What the data does support, stated as narrowly as possible:

- `observation_masking` is today the only strategy the live DeepSeek path can
  serve end to end: it compacts without a model call, completed 4/4 tasks, kept
  every requirement and file path in the projection, and kept every file path in
  the live probe.
- It is not a free win: it kept only 6/12 requirements in the live probe (one task
  kept none), and it slightly **increased** the estimated projection on this suite
  (2321 → 2459, +6%), because these histories are prose-heavy rather than
  tool-output-heavy. That is the trade-off the user has to weigh: reliability of
  the pipeline and of what the model can restate, versus no context reduction
  here.
- The measured data is one repetition of four small tasks with lexical matchers,
  so it bounds the decision rather than settling it.

The default stays `rolling_summary` until the user answers, per decision 4.

## Limits

- **Small N.** Four tasks × three strategies × one repetition; one task can move a
  total. Nothing here is a statistical claim.
- **Lexical matchers.** Retention and recall are normalized literal substrings
  over the scored text. A summary can state a requirement in other words and score
  0, or quote it and score 1 without preserving its meaning. Punctuation is
  significant; there is no regex or semantic matcher. The `multi-file-change`
  probe score of 0/3 is the clearest instance: the answer may well restate the
  requirements in words the matchers do not know, and the probe text is not
  stored in the record, so it cannot be audited without another paid run.
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
- **One compaction, one probe.** The matrix measures the first compaction of each
  cell, not a rolling sequence.
- **Offline proves mechanics, not quality.** The offline summary, probe and
  continuation texts are fixtures. Its full scores and its token drops (for
  example `rolling_summary` 2317 → 1661) reflect fixture lengths and must not be
  read as live behaviour.

## Evidence

Every file below is committed with this report; `eval/results/` is git-ignored by
design, so these are force-added explicitly and nothing else from it is.

Offline matrix (mechanics, label: offline):

- `eval/results/m6-06-offline/aggregate.json`
- `eval/results/m6-06-offline/aggregate.md`
- `eval/results/m6-06-offline/records/bugfix-with-test.observation_masking.0.json`
- `eval/results/m6-06-offline/records/bugfix-with-test.rolling_summary.0.json`
- `eval/results/m6-06-offline/records/bugfix-with-test.structured_state.0.json`
- `eval/results/m6-06-offline/records/feature-style-rule.observation_masking.0.json`
- `eval/results/m6-06-offline/records/feature-style-rule.rolling_summary.0.json`
- `eval/results/m6-06-offline/records/feature-style-rule.structured_state.0.json`
- `eval/results/m6-06-offline/records/multi-file-change.observation_masking.0.json`
- `eval/results/m6-06-offline/records/multi-file-change.rolling_summary.0.json`
- `eval/results/m6-06-offline/records/multi-file-change.structured_state.0.json`
- `eval/results/m6-06-offline/records/refactor-constraint.observation_masking.0.json`
- `eval/results/m6-06-offline/records/refactor-constraint.rolling_summary.0.json`
- `eval/results/m6-06-offline/records/refactor-constraint.structured_state.0.json`

Live matrix (include the eight failed records):

- `eval/results/m6-06-live/aggregate.json`
- `eval/results/m6-06-live/aggregate.md`
- `eval/results/m6-06-live/records/bugfix-with-test.observation_masking.0.json`
- `eval/results/m6-06-live/records/bugfix-with-test.rolling_summary.0.json`
- `eval/results/m6-06-live/records/bugfix-with-test.structured_state.0.json`
- `eval/results/m6-06-live/records/feature-style-rule.observation_masking.0.json`
- `eval/results/m6-06-live/records/feature-style-rule.rolling_summary.0.json`
- `eval/results/m6-06-live/records/feature-style-rule.structured_state.0.json`
- `eval/results/m6-06-live/records/multi-file-change.observation_masking.0.json`
- `eval/results/m6-06-live/records/multi-file-change.rolling_summary.0.json`
- `eval/results/m6-06-live/records/multi-file-change.structured_state.0.json`
- `eval/results/m6-06-live/records/refactor-constraint.observation_masking.0.json`
- `eval/results/m6-06-live/records/refactor-constraint.rolling_summary.0.json`
- `eval/results/m6-06-live/records/refactor-constraint.structured_state.0.json`

The dry run wrote no records (it exits before a run is written) and the scratch
trees it and the live run kept are deliberately not committed.

### Verification performed

The aggregate files are generated by the harness; they were independently
reconciled against the raw records with the following offline command (Python 3 is
used only as the auditor here; the project pipeline is Rust and unchanged):

```sh
python3 - <<'PY'
import json, glob, collections
keys = ["requirements_retained_projection","requirements_retained_probe",
        "requirements_total","files_recalled_projection","files_recalled_probe",
        "files_total","completions","checkable","failures",
        "tokens_before","tokens_after"]
for root in ["eval/results/m6-06-offline","eval/results/m6-06-live"]:
    recs = [json.load(open(p)) for p in sorted(glob.glob(root+"/records/*.json"))]
    agg = json.load(open(root+"/aggregate.json"))
    per = collections.defaultdict(collections.Counter)
    pertask = collections.defaultdict(lambda: collections.defaultdict(collections.Counter))
    for r in recs:
        c = per[r["strategy"]]; c["runs"] += 1
        m = r.get("metrics")
        if m:
            for k in keys[:6]:
                c[k] += m[k]; pertask[r["task"]][r["strategy"]][k] += m[k]
            if m.get("completion") is not None:
                c["checkable"] += 1; c["completions"] += int(m["completion"])
        t = r.get("tokens")
        if t:
            c["tokens_before"] += t["before"]; c["tokens_after"] += t["after"]
        ok = all(r["errors"].get(k) is None for k in
                 ["replay","control","compaction","probe","continuation","check"]) and m is not None
        c["failures"] += 0 if ok else 1
    bad = [(e["strategy"], k, e[k], per[e["strategy"]][k])
           for e in agg["strategies"] for k in keys if e[k] != per[e["strategy"]][k]]
    bad += [(t["task"], e["strategy"], k, e[k], pertask[t["task"]][e["strategy"]][k])
            for t in agg["tasks"] for e in t["strategies"] for k in keys[:8]
            if e[k] != pertask[t["task"]][e["strategy"]][k]]
    combos = collections.Counter((r["task"], r["strategy"], r["repetition"]) for r in recs)
    print(root, "records:", len(recs), "aggregate.runs:", agg["runs"],
          "mismatches:", bad or "none",
          "duplicate/missing cells:", [k for k, v in combos.items() if v != 1] or "none")
PY
```

Result: both runs have 12 records and `aggregate.runs = 12`; every per-strategy
and per-task total in `aggregate.json` matches the recomputed value
(`mismatches: none`); every `(task, strategy, repetition)` combination appears
exactly once. The eight live failures are the eight `provider_failure` compaction
records counted in the table above and are present in the committed record set.

## Deviations

1. **Descriptor hygiene in the agent tool host.** The first offline invocation
   never reached a cell: the product's sandbox descriptor audit refused to start
   because the harness process had inherited a non-close-on-exec descriptor
   (`fd=49 → /dev/ptmx`) from the tool host. That refusal is correct behaviour and
   was not patched. Both matrices were launched through a wrapper that closes all
   inherited descriptors before `exec`, so the harness saw only standard input,
   output and error. Nothing in the repository was changed for this, and the
   wrapper affects no measurement except that the sandbox can start.
2. **Two diagnostic provider requests outside the matrix.** After the paid matrix
   had already failed, two requests were made to identify the cause: one replay of
   the captured summarizer body (rejected, HTTP 400) and one identical body with a
   single appended `user` turn (accepted). They are not matrix cells, they are not
   retries of any cell, no result record was produced from them, and they are
   excluded from every count and aggregate above. One of them generated a summary
   that was dropped unread and is not used as data. The brief's one-matrix limit
   was respected: no second matrix was run.
3. **The dry estimate was produced twice.** Once in the standalone dry run
   (exit 2, no records) and once by the paid command's own built-in dry phase,
   which always runs before `--yes`. Both printed the same estimate.
4. **`--out` was passed explicitly** for every run to keep the three results
   directories apart; the default would otherwise be timestamped.

## Handoff

- Branch/head: pushed to `origin/m6-experiments`; the report commit is the branch
  head, the measured source commit is `cd14762` (see Provenance).
- Measured: 4 tasks × 3 strategies × 1 repetition = 12 cells; 4 completed,
  8 mechanical errors (all `compaction = provider_failure`), 0 cells completed for
  `rolling_summary` and `structured_state`.
- Default: `rolling_summary` is unchanged and the choice is open for the user
  (decision 4).
