# M8-01 dogfood baseline

Measured v0.1.0 at `73e687b` on 2026-11-04 with the driver in this commit. One
run per task per provider, no retries. Raw journals, artifacts, transcripts and
scratch workspaces were deleted after the metrics and the friction notes were
extracted; only the sanitized files under
[`docs/dogfood/baseline/`](baseline/) are committed. The m8-06 rerun with the
M8 tools and a user configuration is compared in
[m8-rerun.md](m8-rerun.md).

**The DeepSeek half is not measured.** The supplied key's secret file had been
deleted and `DEEPSEEK_API_KEY` was absent, so those five cells are `not_run` /
`key_unavailable` (exit-77 semantics) and the run exited 77. m8-06's comparison
must add them before any cross-provider statement is made.

The first attempt at 17:45 UTC was killed by the OpenAI account's Codex usage
limit: one inference returned `rate_limit` in 311 ms and, per decision 4, the
driver marked that provider's remaining cells `not_run` / `provider_quota`. The
cells below were taken after the announced reset at 21:15 UTC. That aborted
attempt measured nothing and is not part of the table.

## Method

### Fixture

`crates/eval/dogfood/fixture/` is `stock-notebook`, a dependency-free crate with
its own empty `[workspace]` table, so it is not a member of the root workspace,
`cargo metadata` does not list it and `check.sh` never builds it. It keeps
insertion-ordered movements in `Vec<Movement>`, aggregates them into a
`BTreeMap<String, i64>`, and renders/parses the `item: quantity` text format. It
has 16 unit tests: 15 pass and exactly one seeded failure
(`ledger::tests::balance_includes_cutoff`, observed 2, expected 5), caused by an
exclusive `<` comparison in `Ledger::balance_through`. Its `target/` is ignored
by its own `.gitignore`.

### Tasks

| Task | Kind | What the model is asked to do | Oracle |
| --- | --- | --- | --- |
| T1 | explain | Name the storage type and ordering, zero-balance retention, blank/duplicate line handling and the public formatter; change nothing | No file may differ from the pristine fixture, and the answer must state all six facts |
| T2 | fix | Make `balance_through` include the cutoff day; preserve every existing test | `cargo test --offline` passes, no test body was removed or ignored, only `src/*.rs` changed, and an injected assertion re-checks the boundary |
| T3 | feature | Add `Ledger::balance_for` and `summary_for`, with tests in **both** files; leave the seeded bug | Both files changed and both gained tests, every old test preserved, the seeded failure is skipped, and injected assertions check `balance_for`/`summary_for` |
| T4 | rename | Rename `display::render_balance` to `display::format_balance` everywhere, no alias | No `render_balance` remains in any `.rs`, the new name exists, old tests preserved modulo the rename, injected assertion checks the new API |
| T5 | investigate | Report the failing test and its observed/expected quantities without changing files | No file changed, and the answer names `balance_includes_cutoff`, `ledger`, 2 and 5 |

Oracles run on the host, in the driver, after the run: a byte diff of the
workspace against the pristine fixture, a `cargo test --offline` of the copy
(T2–T4 only), and answer greps (T1, T5). Host `Command` is intentional here —
this is opt-in dev tooling like `live-check.sh`, and the harness itself still
runs every model-requested command inside Bubblewrap. `cargo fmt`, `clippy
--examples -D warnings` and the offline self-test pass.

Each oracle is proven both ways, offline and without keys, by
`scripts/dogfood.sh --self-test`: every oracle fails on the pristine fixture,
passes on the committed reference patch/answer in
`crates/eval/dogfood/reference/`, T1 and T5 also fail if any file changed, and
the metric extractor is checked against a real journal built through the
runtime. The same command proves the spawned PTY really exposes `/dev/tty`.

### Models and driver

- OpenAI `gpt-6.1-sol` through the read-only Codex login (`--auth codex`), the
  `harness ask` default.
- DeepSeek `deepseek-flash` — not run; no key.

`crates/cli/examples/dogfood.rs` runs `harness ask --no-config --workspace <ws>
--data-root <data> --provider P "<prompt>"` under a PTY, created by the same
`portable-pty` pattern as the M7 PTY driver. `--no-config` keeps user TOML out
of the measurement. Every approval prompt is answered `y` and counted; a
10-minute deadline sends Ctrl-C (Ctrl-C itself is the durable cancel) and then
kills the process. Each cell gets a fresh fixture copy and a fresh private data
root under `target/dogfood-tmp/`, removed after the oracle ran. The driver was
**run without `--approve-read-only`**, as briefed, so reads ask like everything
else.

### What the numbers are

- **Inferences**: committed `assistant_response` entries in the turn.
- **Tool calls by tool**: `tool_call` blocks, counted by name.
- **Approvals**: `y` answers the driver sent; `approvals_by_tool` comes from the
  journal's `approval_requested` events, joined to the call.
- **Refusals**: non-succeeded results with status `denied`/`invalid_arguments`.
  Errors are other non-succeeded results (`nonzero_exit` here), which are the
  agent's own failed commands, not policy denials.
- **Tool execution time**: for each call, the durable timestamp of its
  `execution_intent` record to the timestamp of the commit that carries its
  `tool_result` entry — the same records the runtime uses, not a client clock.
- **Turn wall time**: `turn_started` to `turn_finished`.
- **Tool-time share**: summed tool execution time over that turn's wall time.
- **Read-only segment**: within one model response, a run of two or more
  consecutive calls to registry-declared read-only tools. v0.1 has exactly one:
  `read`. `bash` is a process effect and never counts, even when the command is
  `ls` or `cat`.
- **Tokens**: summed `usage.input_tokens`/`output_tokens` over the turn's
  inferences. Input tokens are re-sent context, not unique tokens.

## Results

OpenAI `gpt-6.1-sol`, one run per task:

| Task | Oracle | Exit | Result code | Inferences | Tool calls | Approvals | Refusals | Wall | Input/output tokens | Tool time / turn | Share |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| T1 | pass | 0 | completed | 3 | bash 2 | 2 (bash 2) | 0 | 13.2 s | 4,312 / 300 | 91 ms / 12,584 ms | 0.72% |
| T2 | pass | 0 | completed | 6 | bash 3, edit 1, read 1 | 5 (bash 3, edit 1, read 1) | 0 | 15.6 s | 11,199 / 291 | 183 ms / 15,252 ms | 1.20% |
| T3 | pass | 0 | completed | 7 | bash 2, edit 4 | 6 (bash 2, edit 4) | 0 | 32.2 s | 21,577 / 987 | 164 ms / 31,855 ms | 0.51% |
| T4 | pass | 0 | completed | 5 | bash 4 | 4 (bash 4) | 0 | 16.6 s | 8,242 / 351 | 208 ms / 16,139 ms | 1.29% |
| T5 | pass | 0 | completed | 4 | bash 3 | 3 (bash 3) | 0 | 12.5 s | 5,593 / 284 | 166 ms / 12,189 ms | 1.37% |
| **All** | **5/5** | 0 | completed | 25 | bash 14, edit 5, read 1 | 20 | 0 | 90.0 s | 50,923 / 2,213 | 813 ms / 88,019 ms | 0.92% |

DeepSeek `deepseek-flash`, one row per task; nothing was sent:

| Task | Oracle | Exit | Result code | Inferences | Tool calls | Approvals | Refusals | Wall | Tokens | Share |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| T1–T5 | not run | — | `key_unavailable` | 0 | — | 0 | 0 | — | — | — |

Per-cell evidence, including each tool call's status, exit code and duration, is
in `<provider>-<task>/metrics.json`; `summary.json` holds the same records plus
the decision-7 aggregate. Model text is excerpted in `excerpt.txt` (at most
about 40 lines, temp roots replaced with `<ws>`/`<data>`; lines matching the
credential heuristic were dropped).

## Friction log

| ID | Category | Severity | Runs | Observed behaviour (journal facts) | Suggested direction |
| --- | --- | --- | --- | --- | --- |
| F01 | missing capability (sandbox) | **high** — the agent cannot verify its own work at all | T2, T3, T4, T5 (4/5 runs) | 6 `bash` calls attempted `cargo`; 5 returned exit 127 with `/bin/bash: line 1: cargo: command not found`, 1 returned 129. T2 and T3 also bundled `git diff --check`/`--stat` into the same call, so the 127 masked whether the diff check ran. Every run's closing text says verification was blocked by the missing toolchain. | Decision 10 / m8-07: opt-in read-only toolchain mounts, `path_prepend` and a writable cache. Nothing was worked around in the fixture, tasks or prompts. |
| F02 | approval burden | **medium** — 20 prompts for 20 tool calls in 90 s of model work; a human is the bottleneck, but no run was denied | all (T1–T5) | 20 approval requests, 20 `y` answers, 0 denials; by tool: `bash` 14, `edit` 5, `read` 1. T3 alone asked 6 times. Reads ask because `--approve-read-only` was not passed (ask-only policy, M7 decision 14). | m8-05: an `ask` read-only auto-approve path and the remembered-grant UX, without weakening approval binding. |
| F03 | tool choice / missing capability | **medium** — inspection costs a process-approval and then hits the sandbox | T1, T4, T5 (and partly T2) | 14 of 20 calls were `bash`; the single `read` was in T2. T1 answered a pure read-only question with `ls -a` and `find src -type f; cat Cargo.toml; cat src/*.rs`; T5 began with `cargo test` and then read with `ls -la; find …; cat …`. No `list`, `glob` or `grep` exists in v0.1. | m8-02's `list`/`glob`/`grep` plus a base-instruction nudge; the measurement should be rerun unchanged in m8-06. |
| F04 | instruction / limit | **low** — wasted inferences, no wrong outcome | T1, T2 | T1 sent byte-identical `ls -a` twice in consecutive inferences; T2 retried `cargo test --offline` after the 127 (exit 129), which cannot succeed while the toolchain is absent. | m8-05: a hint that a missing executable is terminal for that command, and/or prompt guidance against repeating an identical read. |
| F05 | client output | **low** — evidence is still readable, but stdout/stderr interleave ambiguously | all (T1–T5) | Each `excerpt.txt` ends with the assistant's last line glued to the `usage: [...]` JSON, e.g. ``…`cargo: command not found`).usage: [{"input_tokens":…}]``, because assistant text on stdout carries no trailing newline before stderr writes its usage line. | m8-05: terminate assistant output before writing diagnostics, or keep usage off the shared PTY stream. |

Not friction, recorded for honesty: in T2, T4 and T5 the model explicitly told
the user that `cargo` was unavailable and that its quantities were inferred from
source, rather than claiming a test run. No run attempted a policy bypass, no
command was refused, and no protected path was touched.

## Decision 7 data

Decision 7's condition is: tool-execution time, in turns with two or more
consecutive read-only calls, is at least 10% of those turns' wall time.

| Provider | Qualifying turns | Read-only segments | Tool time in segments | Those turns' wall time | Share | Threshold |
| --- | --- | --- | --- | --- | --- | --- |
| OpenAI `gpt-6.1-sol` | 0 of 5 | 0 | 0 ms | 0 ms | 0% (no qualifying turn) | 10% |
| DeepSeek `deepseek-flash` | 0 of 5 (not run) | 0 | 0 ms | 0 ms | not measured | 10% |

The number plainly: **0%**, against a 10% threshold, because no measured turn
contained two or more consecutive read-only calls. The reason is visible in the
tool counts: the models inspected files with `bash` (14 of 20 calls), which is a
process effect, and the only `read` call (T2) was a single call in its inference.
Whether that is a property of the models, of v0.1's tool set, or of this
fixture's size is not determined by one run per cell; m8-02's `list`/`glob`/
`grep` and the m8-06 rerun are what would distinguish them.

## Approval burden

20 approval prompts for 5 tasks (mean 4, median 4, max 6 in T3), split `bash` 14
/ `edit` 5 / `read` 1, with zero denials. The baseline ran **without**
`--approve-read-only`, so every `read` asked; in this sample that is a single
prompt, because the models used `bash` instead. Approval was never the cause of
a failed task, and no prompt expired.

## What this does not show

- No TUI timings or UX measurements: `ask` has no TUI, and nothing here times
  rendering, scrolling or dialogs.
- One run per cell (ten cells requested, five measured). Model variance,
  sampling effects and provider-side changes are not controlled for.
- The DeepSeek half is missing, so no cross-provider comparison exists yet.
- The agent never executed the fixture's tests, so "passes" is the host oracle's
  judgement of the resulting text, not the agent's own verified result.
- `cargo test --offline` was run by the oracle on the host after the run; the
  oracle's exit code is not part of the agent's turn.
- The oracles are mechanical and small: they prove the named facts and the file
  invariants, not general code quality.
