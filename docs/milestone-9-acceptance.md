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
| m9-00: v0.2.1 baseline, T1–T5 unchanged, separate T6 | Sanitized original provider cells and report pinned to the exact base/config/model/oracle; one run per cell, no silent retries | Pending by owner direction (2026-11-06): Codex login and DeepSeek key unavailable in this workspace; v0.2.1 is published |
| T6 mechanical oracle | Pristine/reference and negative oracle self-tests; per-call undoability counts from actual journal evidence | Implemented; see exact-head CI in PR #2 |
| A1/m9-01: honest pipeline status | Exact legacy/new argv tests, bound digest/goldens, both client previews; failing pipeline through real Bubblewrap | Shell-suffix tests pass (2), shared ask previews pass (2), digest/golden checks pass; real S02 tracked in PR #2 |
| B1/m9-02a: reviewed edit design | ADR with limits, repeated-path semantics, complete preflight, partial commit, recovery, persistence and versioning | ADR 0034 records the implementation and its non-atomic commit boundary |
| B1/m9-02b: reversible batched edits | Behavioral tool/policy/descriptor/pre-image/partial-failure/undo/replay/client tests; T6 undo proof | Normal execution and undo implemented; interrupted-batch per-path recovery remains incomplete (see recheck below) |
| B2: shell mutation feedback | Bounded descriptor-safe before/after detection; changed/new/deleted/protected/unavailable tests; stored result feedback | Implemented for scanned entries; workspace-root metadata omission found in recheck below |
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
`loopback: Failed RTM_NEWADDR` (the OS error suffix varies between probes).
The new ignored test
`s02_pipeline_failure_survives_output_filtering` fails at the required backend
probe, before executing a pipeline. That is **not product isolation proof**.
No fallback, relaxed namespace policy or successful-skip claim is used. The
existing GitHub `Host sandbox security (S02)` gate is the verification path.

Live-provider credentials are not present in this workspace. Their absence
is not a provider failure or evidence of compatibility. The owner requested
that live runs remain recorded as pending on 2026-11-06; this is not a waiver
of live acceptance or evidence that a provider passed. Historical M8 cells
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

## Recheck — 2026-11-06

The fetched PR head remained `37a1258` and `main` remained `78e7629`; no newer
remote implementation commits or review comments were present. The published
v0.2.1 release was confirmed. Existing exact-head CI is green, but source
review found gaps not covered by that gate:

- **Interrupted batch recovery:** `tools/recovery.rs` provides hints only for
  `write`/`edit`, and `runtime::UserResolution::Observed` accepts one file effect
  for the entire invocation. After a crash between two batch renames, the user
  cannot record mixed per-path effects. An aggregate applied result selects all
  captured paths for undo, so an untouched path can make undo preflight refuse;
  aggregate not-applied omits even committed paths. This does not meet ADR 0034's
  per-path recovery requirement. A crash-between-renames recovery regression is
  still needed; normal partial-result and successful undo tests do not cover it.
- **Root-only metadata feedback:** `sandbox/change_scan.rs` fingerprints child
  entries but not the workspace root. A root-only `chmod` can therefore compare
  as unchanged and omit outside-undo guidance, even when both scans complete.
  Existing mode-change coverage exercises a file and child directory, not root.
- **Schema clarity follow-up:** the read schema permits a top-level range next
  to `paths`, while semantic parsing rejects it. For example,
  `{"paths":[{"path":"a"}],"max_bytes":10}` is schema-valid but refused because
  the range belongs inside the item. This is a validation/usability mismatch,
  not an authorization bypass.

These are source-review findings, not newly added failing regression tests.
No product-code fixes are included in this recheck. The milestone remains
unaccepted for the implementation gaps as well as the pending live evidence.

Fresh local runs passed formatting, workspace/client/conformance Clippy,
dogfood self-tests, release-tooling and security/offline-gate regressions,
and all five feature-enabled PTY tests, including batch approval and undo.
The full local gate is still not green: root-only undo failures pass unchanged
as `hoplite` (all 13 core undo and five CLI undo tests), while the storage
symlink refusal fails under either UID. The workspace run also found that IPv6
loopback `::1` cannot bind (`EADDRNOTAVAIL`); adding that local address was denied
by the runtime. These limitations were not bypassed or hidden. Linux CI remains
the supported full-gate and real-sandbox evidence; it does not cover the source
findings above.

## Known limitations retained

Batched renames are not a cross-file transaction. Explicit uncertainty is not
silently retried. Bash changes, external effects and unknown effects are not
made reversible by scanning. Scans are bounded, exclude protected names/mounts,
and cannot establish causation or prevent unrelated host races. Parallel tool
execution, MCP, subagents and checkpoints remain outside this milestone.
