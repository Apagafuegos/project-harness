# Milestone 9 acceptance — in progress, not accepted

The owner authorized the recommended options in the [M9 plan](milestone-9-plan.md)
and requested implementation through verification. The base is `6467b9f`,
containing the merged v0.2.1 fixes and review follow-up. This document is an
acceptance checklist, **not a claim that the milestone is complete**.

## Required evidence

| Requirement | Acceptance evidence | Current state |
| --- | --- | --- |
| m9-00: v0.2.1 baseline, T1–T5 unchanged, separate T6 | Sanitized original provider cells and report pinned to the exact base/config/model/oracle; one run per cell, no silent retries | Pending: Codex login and DeepSeek key unavailable in this workspace; v0.2.1 not yet published |
| T6 mechanical oracle | Pristine/reference and negative oracle self-tests; per-call undoability counts from actual journal evidence | In progress |
| A1/m9-01: honest pipeline status | Exact legacy/new argv tests, bound digest/goldens, both client previews; failing pipeline through real Bubblewrap | Local shell-suffix tests pass (2); shared ask preview tests pass (2); real S02 pending |
| B1/m9-02a: reviewed edit design | ADR with limits, repeated-path semantics, complete preflight, partial commit, recovery, persistence and versioning | ADR 0034 in progress |
| B1/m9-02b: reversible batched edits | Behavioral tool/policy/descriptor/pre-image/partial-failure/undo/replay/client tests; T6 undo proof | In progress |
| B2: shell mutation feedback | Bounded descriptor-safe before/after detection; changed/new/deleted/protected/unavailable tests; stored result feedback | In progress |
| C1+C2/m9-03: bounded line/multi-path reads | Strict argument and golden plans; policy/D9 closure; UTF-8, EOF, long lines, aggregate limits and per-path truncation tests | In progress |
| D: no additional headline feature | No MCP/subagent/checkpoint/parallel-executor change | Scope retained |
| E/m9-05: v0.3.0 readiness | Workspace/lock/changelog, guide/contracts, exact-head gate and acceptance evidence | Pending; no tag or publication authorized here |
| F/m9-05: rerun T1–T6 | Same method/config/providers with pinned provenance and honest comparison against baseline | Pending live access; no cells fabricated or historical cells relabeled |
| m9-04: conditional Anthropic/live switching | Existing opt-in live suites, sanitized evidence or explicit key-unavailable partial | `ANTHROPIC_API_KEY` unavailable; remains partial, not a pass |
| Offline gate | `./scripts/check.sh`, including format, Clippy, all workspace/conformance/PTY/doc/smoke/security checks | Pending integration |
| Real sandbox gate | Existing independent S02 CI suite on the exact final head | Local sandbox probe fails; CI required |
| Material TUI changes | Fresh interaction checks and safely publishable appearance evidence | Pending integration |

## Environment evidence

The installed Bubblewrap 0.9.0 cannot establish its isolated network on this
Modal runtime: `bwrap --unshare-user --unshare-net --ro-bind / / true` returns
`loopback: Failed RTM_NEWADDR: No child processes`. The new ignored test
`s02_pipeline_failure_survives_output_filtering` fails at the required backend
probe, before executing a pipeline. That is **not product isolation proof**.
No fallback, relaxed namespace policy or successful-skip claim is used. The
existing GitHub `Host sandbox security (S02)` gate is the verification path.

Live-provider credentials are not present in this workspace. Their absence
is not a provider failure or evidence of compatibility. Historical M8 cells
remain historical and do not stand in for M9 measurements.
