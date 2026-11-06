# Milestone 9 acceptance — in progress, not accepted

The owner authorized the recommended options in the [M9 plan](milestone-9-plan.md)
and requested implementation through verification. The base is `6467b9f`,
containing the merged v0.2.1 fixes and review follow-up. This document is an
acceptance checklist, **not a claim that the milestone is complete**.
Exact-head CI and review results are attached to [PR #2](https://github.com/Apagafuegos/project-harness/pull/2);
this document does not predict a pending run's outcome.

## Required evidence

| Requirement | Acceptance evidence | Current state |
| --- | --- | --- |
| m9-00: v0.2.1 baseline, T1–T5 unchanged, separate T6 | Sanitized original provider cells and report pinned to the exact base/config/model/oracle; one run per cell, no silent retries | Pending: Codex login and DeepSeek key unavailable in this workspace; v0.2.1 not yet published |
| T6 mechanical oracle | Pristine/reference and negative oracle self-tests; per-call undoability counts from actual journal evidence | Implemented; see exact-head CI in PR #2 |
| A1/m9-01: honest pipeline status | Exact legacy/new argv tests, bound digest/goldens, both client previews; failing pipeline through real Bubblewrap | Shell-suffix tests pass (2), shared ask previews pass (2), digest/golden checks pass; real S02 tracked in PR #2 |
| B1/m9-02a: reviewed edit design | ADR with limits, repeated-path semantics, complete preflight, partial commit, recovery, persistence and versioning | ADR 0034 records the implementation and its non-atomic commit boundary |
| B1/m9-02b: reversible batched edits | Behavioral tool/policy/descriptor/pre-image/partial-failure/undo/replay/client tests; T6 undo proof | Implemented; see exact-head CI in PR #2 |
| B2: shell mutation feedback | Bounded descriptor-safe before/after detection; changed/new/deleted/protected/unavailable tests; stored result feedback | Implemented; see exact-head CI in PR #2 |
| C1+C2/m9-03: bounded line/multi-path reads | Strict argument and golden plans; policy/D9 closure; UTF-8, EOF, long lines, aggregate limits and per-path truncation tests | Implemented; see exact-head CI in PR #2 |
| D: no additional headline feature | No MCP/subagent/checkpoint/parallel-executor change | Scope retained |
| E/m9-05: v0.3.0 readiness | Workspace/lock/changelog, guide/contracts, exact-head gate and acceptance evidence | Workspace/lock/changelog and guide prepared at 0.3.0; no tag or publication |
| F/m9-05: rerun T1–T6 | Same method/config/providers with pinned provenance and honest comparison against baseline | Pending live access; no cells fabricated or historical cells relabeled |
| m9-04: conditional Anthropic/live switching | Existing opt-in live suites, sanitized evidence or explicit key-unavailable partial | `ANTHROPIC_API_KEY` unavailable; remains partial, not a pass |
| Offline gate | `./scripts/check.sh`, including format, Clippy, all workspace/conformance/PTY/doc/smoke/security checks | Local gate limitations below; exact-head full gate tracked in PR #2 |
| Real sandbox gate | Existing independent S02 CI suite on the exact final head | Local probe unavailable; required Linux S02 tracked in PR #2 |
| Material TUI changes | Fresh interaction checks and safely publishable appearance evidence | Scripted real-terminal flow passes; fresh approval/undo captures inspected |

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

## Current local verification

- `scripts/dogfood.sh --self-test` passes: T1–T6 positive/negative oracles,
  T6 excluded from original comparison totals, actual batched-edit journal
  metrics, trusted undo restoration, config forwarding and `/dev/tty`.
- `cargo clippy --workspace --all-targets --locked -- -D warnings` passes.
- The real-terminal test `batch_approval_and_undo_in_the_running_tui` passes:
  an auto-approved batched line read, three explicit file previews, one human
  mutation approval, all files changed, `y` undo,
  all files restored, a durable successful three-path undo result and restored
  termios. Fresh terminal captures were inspected; they use the debug-only
  scripted provider, not a live-provider compatibility claim.
- Final-version release-packaging regressions pass (wrong-tag refusal,
  reproducible archives, dependency notices and required-guide checks).
- Feature-enabled client Clippy, core conformance Clippy and workspace rustdoc
  with warnings denied pass. Read-many tests pass (16), sandbox helper tests
  pass (44), and v1–v4 context/continuation compatibility checks pass.
- `./scripts/check.sh` has been run and is not claimed green locally. This
  sandbox executes tools as uid 0, so the existing chmod-based interrupted-undo
  test can still remove a directory entry and fails its expected permission
  refusal. Running that **unchanged** test binary as the ordinary `hoplite`
  user passes. The existing private-storage symlink test independently fails
  because this runtime follows directory symlinks despite `O_NOFOLLOW`, as
  demonstrated by a fresh minimal `os.open` reproduction. Neither test was
  skipped or weakened. Exact-head Linux CI remains required for acceptance.

## Known limitations retained

Batched renames are not a cross-file transaction. Explicit uncertainty is not
silently retried. Bash changes, external effects and unknown effects are not
made reversible by scanning. Scans are bounded, exclude protected names/mounts,
and cannot establish causation or prevent unrelated host races. Parallel tool
execution, MCP, subagents and checkpoints remain outside this milestone.
