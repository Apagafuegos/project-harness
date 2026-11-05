# M8-06 dogfood rerun — v0.2.0 as a user would configure it

Rerun of the [m8-01 baseline](m8-baseline.md), twice: once against the v0.2.0
product code (`d270512d02243ddfeb01d46b8127a5d4b8008a31`) and the same fixture,
tasks and oracles, with the M8 tools available and the harness configured the
way a daily user would configure it — `approve_read_only = true` and an opt-in,
read-only `[sandbox]` toolchain mount. Ran on 2026-11-05 between 15:33 and 15:36
UTC, one run per task per provider, no retries. Raw journals, artifacts,
transcripts and scratch workspaces were deleted after the metrics and the
friction notes were extracted; only the sanitized files under
[`docs/dogfood/rerun/`](rerun/) are committed.

**The DeepSeek half was not measured in this run; see the
[addendum](#addendum-deepseek-half-added-2026-11-05-after-v020-was-published),
added after publication.** No DeepSeek key was available for this step, so those five cells are `not_run` / `key_unavailable` (exit-77
semantics), exactly as in the baseline. Any cross-provider statement is
therefore still impossible, and the DeepSeek half of M8 acceptance is partial.

**Quota.** The OpenAI cells used the user's existing Codex login at the
`harness ask` default model, `gpt-6.1-sol`. The account's usage limit reset at
15:18 UTC on the day of the rerun; the run started at 15:33:53 UTC, after that
reset. No cell reported `rate_limit`, `insufficient_quota` or any provider code,
so nothing was waited for, repeated or retried. The one run per cell rule was
respected throughout.

## Method

### Unchanged from the baseline

- The fixture `crates/eval/dogfood/fixture/` (`stock-notebook`, dependency-free,
  its own empty `[workspace]`, one seeded failing test) and its 16 unit tests.
- The five tasks and prompts (`crates/eval/dogfood/tasks/T1…T5/prompt.md`), and
  their mechanical oracles in `tasks/oracle.rs`: a byte diff of the workspace
  against the pristine fixture, a host `cargo test --offline` of the copy, and
  answer greps.
- One fresh fixture copy and one fresh private data root per cell under
  `target/dogfood-tmp/`, a 10-minute wall-clock deadline per cell, every
  approval prompt answered `y`, and one run per cell with no retries.
- The provider and model: OpenAI `gpt-6.1-sol` through the read-only Codex
  login (`--auth codex`). DeepSeek `deepseek-flash` would need
  `DEEPSEEK_API_KEY`; it was absent.
- The metric definitions below, including the decision-7 read-only segment rule.

### What changed (decision 15)

1. **The driver gained an opt-in `--config FILE`**
   (`crates/cli/examples/dogfood.rs`, `scripts/dogfood.sh`). With it, the driver
   forwards `harness --config FILE ask …` instead of
   `harness --no-config ask …`; without it, the baseline method still
   reproduces byte for byte. `scripts/dogfood.sh --self-test` now proves the
   forwarding offline: a stand-in binary prints its argv inside the same PTY
   path a live cell uses, and the self-test asserts `--config <path>` is present
   and `--no-config` absent, then the reverse for the default.
2. **Metrics gained the approval split** (record schema 2): human prompts divided
   into read-only (`read`, `list`, `glob`, `grep`) and other, plus the number of
   read-only requests the client auto-approved. Requests come from the journal's
   `approval_requested` events joined to the call — `approve_read_only` never
   changes the recorded policy, so the request is journaled either way — and the
   auto-approval count comes from the client's own
   `approval: read-only auto-approved` line in the run transcript. The split is
   also checked for consistency: `read-only prompts + other prompts` must equal
   the prompts the driver actually answered, and the number of auto-approvals
   must not exceed the read-only requests. All five measured cells are
   consistent; `summary.json` carries `split_consistent: true`.
3. **The measured configuration** is a private user TOML, written under this
   checkout's `target/`, never committed and deleted after the run. Its shape,
   with this host's paths replaced by `<toolchain>`:

   ```toml
   approve_read_only = true

   [sandbox]
   toolchain_paths = ["<toolchain>"]
   path_prepend = ["<toolchain>/bin"]

   [sandbox.env]
   RUSTUP_TOOLCHAIN = "1.99.1"
   ```

   `<toolchain>` is this host's rustup `1.99.1-aarch64-unknown-linux-gnu`
   directory, resolved the way ADR 0033's host example does (the toolchain
   directory itself, not the `~/.cargo/bin` rustup proxy shims). The fixture has
   no dependencies, so no `cargo_registry` is configured. The journal of every
   measured cell records this configuration; the evidence keeps only its shape
   (`toolchain_mounts: 1`, `path_prepend_entries: 1`, `cargo_registry: false`,
   `env_keys: ["RUSTUP_TOOLCHAIN"]`), never a path.
4. **New product behaviour was available to the model:** the M8 search tools
   (`list`, `glob`, `grep`, ADR 0031), durable undo of trusted edits (ADR 0032),
   the TUI `approve_read_only` (and the same auto-approval in `ask`), the
   exit-127 hint, the remembered-grant message and the base-instruction sentence
   against repeating an identical call (m8-05), and the toolchain sandbox itself
   (ADR 0033). Nothing else about the harness changed.

### Three sequencing deviations, disclosed

- **`CARGO_NET_OFFLINE` is not in the `[sandbox.env]` block.** Decision 15's
  sample configuration and the host recipes in ADR 0033, `docs/tools.md` and
  `docs/development.md` all set it, but the shipped validation refuses every
  `CARGO_*` name because the harness owns the sandbox cargo environment
  (`CARGO_HOME` is a private per-command directory). Loading that config fails
  with exit 2:
  `sandbox: sandbox.env.CARGO_NET_OFFLINE: refused environment key or invalid string`.
  The env block therefore carries `RUSTUP_TOOLCHAIN` only. This changes nothing
  about network access — the sandbox still denies it — and the model passed
  `--offline` to cargo itself in every measured command. Recorded as a
  documentation defect against shipped behaviour in the handoff; the example
  documents, not the validation or any runtime rule, were corrected in this
  step.
- **The measured binary's `--version` reported `0.1.0`.** The run used the
  product code at `d270512`, built before the workspace version bump to `0.2.0`
  in this same step. `v0.2.0` is that code plus the version metadata: no product
  source, journal format or runtime path differs between the measured commit and
  the acceptance commit. The evidence records `harness_commit: d270512d0224…`
  in every cell. Re-running after the bump would have been a second run per
  cell, which the brief forbids.
- **The driver exited 1 (`infrastructure_failure`), not 77.** Every cell was
  written and closed before that exit; the failure came from the driver's final
  `remove_dir` of `target/dogfood-tmp/`, which a leftover doctor-probe directory
  from the configuration check made non-empty. No measurement was retried or
  affected, and the leftover directory was removed afterwards. Recorded as F09.

### What the numbers are

The definitions are the baseline's: inferences are committed
`assistant_response` entries; tool calls are `tool_call` blocks by name;
approvals are the `y` answers the driver sent; refusals are non-succeeded
results with status `denied`/`refused`/`invalid_arguments`; errors are other
non-succeeded results (here `nonzero_exit`, the agent's own failed commands);
tool execution time runs from a call's `execution_intent` record to the commit
carrying its `tool_result`; turn wall time runs from `turn_started` to
`turn_finished`; and a read-only segment is a run of two or more consecutive
calls to registry-declared read-only tools (`read`, `list`, `glob`, `grep`)
**within one model response**. Tokens are summed provider usage; input tokens
are re-sent context, not unique tokens.

Two new fields: `approval_prompts` (what a human actually answered) and
`approval_classes.read_only` / `.other` (`requests`, `auto_approved`, `prompts`).
Baseline approvals in the tables below are therefore the baseline's prompt
counts, which equal its requests because nothing was auto-approved.

**One run per cell is anecdotal, not statistical.** Five tasks on one model,
once each, cannot separate model variance, provider-side changes, prompt
sensitivity and tool-set effects. The comparison below is a record of what
happened twice, not an estimate of a distribution.

## Results

OpenAI `gpt-6.1-sol`, one run per task. "Prompts" is what a human answered;
"auto" is registry read-only calls answered without a prompt.

| Task | Oracle base → rerun | Inferences base → rerun | Prompts base → rerun | Read-only prompts / auto (rerun) | Refusals base → rerun | Wall base → rerun | Tokens in/out base → rerun | Tool time / turn base → rerun |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| T1 | pass → pass | 3 → 6 | 2 → 0 | 0 / 5 | 0 → 0 | 13.2 s → 23.7 s | 4,312 / 300 → 15,396 / 344 | 91 / 12,584 ms (0.72%) → 55 / 23,320 ms (0.23%) |
| T2 | pass → pass | 6 → 5 | 5 → 2 | 0 / 2 | 0 → 0 | 15.6 s → 21.7 s | 11,199 / 291 → 11,456 / 273 | 183 / 15,252 ms (1.20%) → 1,268 / 21,230 ms (5.97%) |
| T3 | pass → pass | 7 → 8 | 6 → 5 | 0 / 2 | 0 → 0 | 32.2 s → 63.9 s | 21,577 / 987 → 27,743 / 1,051 | 164 / 31,855 ms (0.51%) → 888 / 63,572 ms (1.40%) |
| T4 | pass → pass | 5 → 5 | 4 → 1 | 0 / 3 | 0 → 0 | 16.6 s → 20.0 s | 8,242 / 351 → 14,192 / 370 | 208 / 16,139 ms (1.29%) → 1,028 / 19,767 ms (5.20%) |
| T5 | **pass → fail** | 4 → 3 | 3 → 1 | 0 / 1 | 0 → 0 | 12.5 s → 19.4 s | 5,593 / 284 → 6,479 / 207 | 166 / 12,189 ms (1.37%) → 761 / 19,054 ms (3.99%) |
| **All** | **5/5 → 4/5** | 25 → 27 | 20 → 9 | 0 / 13 | 0 → 0 | 90.0 s → 148.6 s | 50,923 / 2,213 → 75,266 / 2,245 | 813 / 88,019 ms (0.92%) → 3,999.6 / 146,942 ms (2.72%) |

Tool calls by tool, base → rerun:

| Task | `bash` | `edit` | `read` | `list` | `glob` | `grep` |
| --- | --- | --- | --- | --- | --- | --- |
| T1 | 2 → 0 | 0 → 0 | 0 → 3 | 0 → 2 | 0 → 0 | 0 → 0 |
| T2 | 3 → 1 | 1 → 1 | 1 → 0 | 0 → 1 | 0 → 0 | 0 → 1 |
| T3 | 2 → 1 | 4 → 4 | 0 → 2 | 0 → 0 | 0 → 0 | 0 → 0 |
| T4 | 4 → 1 | 0 → 0 | 0 → 0 | 0 → 1 | 0 → 0 | 0 → 2 |
| T5 | 3 → 1 | 0 → 0 | 0 → 1 | 0 → 0 | 0 → 0 | 0 → 0 |
| **All** | 14 → 4 | 5 → 5 | 1 → 6 | 0 → 4 | 0 → 0 | 0 → 3 |

Approvals, base → rerun, split as decision 15 requires:

| Task | Prompts (base) | Prompts (rerun) | read-only prompts | other prompts | auto-approved read-only |
| --- | --- | --- | --- | --- | --- |
| T1 | 2 (`bash` 2) | 0 | 0 | 0 | 5 (`list` 2, `read` 3) |
| T2 | 5 (`bash` 3, `edit` 1, `read` 1) | 2 (`bash` 1, `edit` 1) | 0 | 2 | 2 (`list` 1, `grep` 1) |
| T3 | 6 (`bash` 2, `edit` 4) | 5 (`bash` 1, `edit` 4) | 0 | 5 | 2 (`read` 2) |
| T4 | 4 (`bash` 4) | 1 (`bash` 1) | 0 | 1 | 3 (`list` 1, `grep` 2) |
| T5 | 3 (`bash` 3) | 1 (`bash` 1) | 0 | 1 | 1 (`read` 1) |
| **All** | 20 | **9** | **0** | 9 (`bash` 4, `edit` 5) | **13** |

Errors (the agent's own failed commands, not policy): 3 `nonzero_exit` results in
the rerun, all `cargo test` runs with exit 101 — T3 after adding its feature
(the seeded failure remained), T4 after the rename (same), T5 investigating
(same). The baseline had none, because the toolchain was unreachable. No command
was refused or denied in either run, and no run touched a protected path.

OpenAI cells that passed an oracle passed it in both directions: T1 and T5
require an unchanged tree, T2–T4 preserve every existing test and pass a real
`cargo test --offline` of the copy plus an independently injected assertion.

DeepSeek `deepseek-flash`, one row per task; nothing was sent:

| Task | Oracle | Result code | Inferences | Prompts | Auto | Wall | Tokens |
| --- | --- | --- | --- | --- | --- | --- | --- |
| T1–T5 | not run | `key_unavailable` | 0 | 0 | 0 | — | — |

Per-cell evidence, including every call's status, exit code and duration, is in
`<provider>-<task>/metrics.json`; `summary.json` holds the same records plus the
decision-7 aggregate and the per-provider approval totals. Model text is
excerpted in `excerpt.txt` (at most about 40 lines, temp roots replaced with
`<ws>`/`<data>` and the configured toolchain root with `<toolchain>`; lines
matching the credential heuristic were dropped).

### The one oracle failure, in full

T5 asked which test fails and with what quantities, and forbids changing files.
The answer named `ledger::tests::balance_includes_cutoff`, `ledger`, the observed
`2` and the expected `5`, and said the numbers came from execution — the
`answer_facts` check passed. The oracle still failed because the tree gained one
file, `Cargo.lock`: the model verified with `cargo test --offline`, and cargo
writes a lock file for a package that has none, exactly as it would for a user.
The fixture ships no `Cargo.lock` (only `/target/` is ignored), and the T1/T5
"no file changed" invariant does not exempt it, although the T2–T4 `safe` check
already treats a new `Cargo.lock` as ordinary build output. No source file
changed, and `target/` never enters the tree comparison.

So this is a real regression in the *measurement*, not a wrong answer: the
baseline could only pass T5 because `cargo` was unreachable. It is recorded as
new friction F06 and was not "fixed" here, because changing an oracle after
measuring with it would make the two cells incomparable.

## Friction status F01–F05

| ID | Status | Journal fact from the rerun |
| --- | --- | --- |
| F01 — no cargo in the sandbox | **Resolved** | Four `bash` calls ran `cargo` inside the sandbox; **none** returned 127 or "command not found". T2's returned **exit 0** with "16 passed, 0 failed" in its result text; T3's, T4's and T5's returned exit 101 because the seeded failure is still there (T3/T4 were asked to leave it, T5 was investigating it). T2 and T5 both verified with tests: T2 reported a full passing run, T5 reported the failing test's name and both quantities from `cargo test` output. Every run's closing text cites a real test run instead of saying verification was blocked. |
| F02 — approval burden | **Improved** | 9 human prompts for 22 tool calls (baseline: 20 for 20). Read-only prompts **0**; 13 read-only requests were auto-approved (`list` 4, `read` 6, `grep` 3). `bash` 4 and `edit` 5 still asked, as designed. T1 needed no prompts at all (baseline: 2); T4 one (baseline: 4). |
| F03 — tool choice / missing capability | **Resolved** | 13 of 22 calls were in-process read calls (`read` 6, `list` 4, `grep` 3); baseline had 1 `read` of 20 and no `list`/`glob`/`grep`. `bash` fell from 14 to 4. T1 answered a pure read-only question with `list`/`read` only and zero `bash` (baseline: `ls -a` and a `find … cat …` call); T4 located the rename sites with `list` + `grep` before one `bash` mutation. |
| F04 — instruction / limit (wasted inferences) | **Improved** | No byte-identical repeated call and no retry after a missing executable — the two facts the baseline logged. Inference count rose 25 → 27 and input tokens 50,923 → 75,266, but the extra inferences did work (more, smaller read calls), and the 127 retry is impossible now. |
| F05 — client output (stdout/stderr glue) | **Resolved** | Every rerun excerpt ends with its final answer line followed by a separate `turn: completed` line; no excerpt glues the `usage: [...]` JSON onto assistant text (baseline: all five). The usage line is still written after the assistant text ends, as designed. |

Not friction, recorded for honesty: in T3 and T4 the model reported the seeded
failure accurately instead of claiming a green run; in T4 it renamed via a
`python3` heredoc inside `bash` rather than with `edit` (see F07); no run
attempted a policy bypass, and no mount or protected path was touched.

## Decision 7, recomputed

Decision 7's condition: tool-execution time, in turns with two or more
consecutive read-only calls, is at least 10% of those turns' wall time.

| Provider | Qualifying turns | Read-only segments | Tool time in segments | Those turns' wall time | Share | Threshold |
| --- | --- | --- | --- | --- | --- | --- |
| OpenAI `gpt-6.1-sol` | 0 of 5 | 0 | 0 ms | 0 ms | 0% (no qualifying turn) | 10% |
| DeepSeek `deepseek-flash` | 0 of 5 (not run) | 0 | 0 ms | 0 ms | not measured | 10% |

**Still 0% against the 10% threshold**, for a different reason than the baseline.
The baseline had almost no read-only calls (one `read`, inside its own
inference); the rerun has 13, but the models issued them **one per model
response** (T1: `list`, `list`, `read`, `read`, `read` across five inferences;
T2: `grep` and `list` separated by `edit` and `bash`), so no response contained
two consecutive read-only calls. Total tool time is 3,999.6 ms of 146,942 ms
(2.72%), up from 0.92% because `cargo test` now really runs — still far below
10% even if every turn qualified.

Consequence: **m8-04 (parallel read-only tools) remains deferred** and is not a
post-v0.2.0 candidate on this evidence. Decision 11's revisit condition ("only
if the m8-06 rerun shows the share at or above 10%") is not met. Whether the
one-call-per-inference pattern is a model property, the base instruction's "one
call at a time" wording, or the fixture's size is not decided by one run per
cell; parallel execution would not have saved measurable wall time in these five
turns anyway.

## New friction

| ID | Category | Severity | Runs | Observed behaviour (journal/transcript facts) | Suggested direction |
| --- | --- | --- | --- | --- | --- |
| F06 | measurement / oracle assumption | **medium** — one task's oracle now fails on a correct answer | T5 | `cargo test --offline` created `Cargo.lock` in a fixture that ships none; T5's oracle requires an unchanged tree, so `pass: false` while `answer_facts: true`. T2–T4 already allow a new `Cargo.lock` in their `safe` check. | Ship a `Cargo.lock` in the fixture, or exempt build outputs from the T1/T5 invariant the way the T2–T4 check already does. Not changed after measuring. |
| F07 | undo coverage / tool choice | **medium** — a correct result that undo cannot reverse | T4 | The rename was one `bash` call running a `python3` heredoc, not `edit` calls. ADR 0032 covers trusted `write`/`edit` effects only, so this turn's file changes are listed as not reversible by `Ctrl-Z`/`sessions undo`. It also collapsed two edits into one broad approval instead of per-edit previews. | Decide after v0.2.0 whether to nudge mutations to trusted tools (instruction or result feedback) or widen undo coverage; neither is in this step. |
| F08 | cost / limit | low — no wrong outcome | T1 (and the total) | Answering a read-only question cost 6 inferences and 15,396 input tokens (baseline: 3 and 4,312), because each small `list`/`read` result is re-sent as context. Tool time fell to 55 ms, so the price is context, not process time. | Watch it in the next measurement; the existing result-size bounds and context reports already expose it. No change now. |
| F09 | dev tooling (driver) | low — no measurement affected | the run as a whole | The driver's final `fs::remove_dir` of `target/dogfood-tmp/` fails if any unrelated file or directory is left there, turning a fully recorded run into `infrastructure_failure` (exit 1) instead of the intended 77 for missing keys. It happened here because a doctor probe had created directories under the same scratch root. | Use a per-run scratch subdirectory, or treat the final cleanup as best-effort with an explicit warning. |

## v0.2.1 measurement fixes (2026-11-05, after this rerun)

Nothing in this report was re-measured, and the tables above are unchanged on
purpose: they record what the oracles decided at v0.2.0.

- **F06 is fixed in the oracle, not in the cells.** From v0.2.1 the T1/T5
  "no file changed" invariant exempts a *created* `Cargo.lock`, exactly as the
  T2–T4 `safe` check already did, and the dogfood `--self-test` covers it. Read
  against that corrected oracle, **T5's recorded facts pass** for both providers:
  each T5 cell records `answer_facts: true` and exactly one changed file,
  `Cargo.lock`, with no source file changed (see this report's DeepSeek addendum
  for the provider that ran later). The cells stay comparable because the
  correction relaxes an invariant that was inconsistent with the other three
  tasks, rather than the answer being judged differently.
- **F09 is fixed in the driver.** A leftover scratch file or directory no longer
  turns a finished run into `infrastructure_failure`: the driver removes the
  whole per-run scratch tree and, if it survives, reports it as a warning
  without failing any recorded cell.

## What this does not show

- No TUI timings or UX measurements: `ask` has no TUI and nothing here times
  rendering, dialogs, Ctrl-Z, or the read-only auto-approval banner.
- One run per cell on one model (five measured cells, five unrun). Model
  variance, sampling, provider-side changes and prompt sensitivity are not
  controlled for, and no cross-provider comparison exists.
- The oracle is a mechanical judge of a small result, not a quality measure; T5's
  failure is an invariant mismatch, not a wrong answer.
- The measured binary reported `0.1.0`; the workspace version became `0.2.0`
  afterwards with no product code change (see the deviations above).
- The rerun says nothing about turns longer than the 10-minute deadline (none
  hit it) or about toolchains other than rustup's `1.99.1` on this host.


## Addendum: DeepSeek half (added 2026-11-05, after v0.2.0 was published)

The user supplied a new key after publication, so the DeepSeek half was run with
the rerun's configuration, on commit `b93f61d43977f1035c4c1161a9ae1713b6fc2bc2`
(the published v0.2.0 product code plus a docs-only record). One run per task, no
retries. Its cells replace the `not_run` placeholders under
[`docs/dogfood/rerun/`](rerun/), and its aggregate is
[`summary-deepseek.json`](rerun/summary-deepseek.json). The tables above are left
as written.

| Task | Oracle | Inferences | Tool calls | Prompts (RO/other) | Auto-RO | Refusals | Wall | Tokens in/out |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| T1 | pass | 4 | glob 1, grep 1, list 1, read 4 | 0 (0/0) | 7 | 0 | 14.1 s | 5,615 / 1,945 |
| T2 | pass | 7 | bash 2, edit 1, glob 1, list 1, read 5 | 3 (0/3) | 7 | 0 | 11.5 s | 8,082 / 891 |
| T3 | pass | 9 | bash 3, edit 4, glob 1, list 1, read 6 | 7 (0/7) | 8 | 0 | 21.4 s | 11,772 / 2,732 |
| T4 | pass | 8 | bash 4, grep 1, list 1, read 3 | 4 (0/4) | 5 | 0 | 15.1 s | 6,082 / 1,484 |
| T5 | **fail** (answer correct; `Cargo.lock` created) | 3 | bash 1, list 1, read 1 | 1 (0/1) | 2 | 0 | 6.8 s | 3,914 / 682 |

- **Totals:** 4/5 oracles, 31 inferences, 44 tool calls
  (10 `bash`), 15 prompts, 29 read-only auto-approvals, 68.9 s wall, 35,465 / 7,734
  tokens. OpenAI's rerun totals, for reference: 4/5, 27, 22 (4 `bash`), 9, 13,
  148.6 s, 75,266 / 2,245.
- **T5** fails for the same reason as OpenAI's: `cargo` created a `Cargo.lock`.
- **Cargo in the sandbox:** 5 attempts, none `command not found`. Their recorded
  exit status is 0, but that doesn't mean the tests passed: the model piped cargo
  through `tail`, so the status is `tail`'s. The answers report 16 passed in T2 after
  the fix, and in T3, T4 and T5 the fixture's seeded test still failing.
- **Decision 7:** 0.40% against the 10% threshold, across
  4 eligible turns and 9 read-only segments. Unlike
  OpenAI's rerun, this model did make consecutive read-only calls, so the share is a
  real measurement rather than an empty one. It is still far below the threshold.

**Limits.** There is no DeepSeek baseline: v0.1 was never measured on this provider,
so nothing here shows improvement over time. Two providers on one run each is not a
model comparison, and token counts aren't comparable across them.
