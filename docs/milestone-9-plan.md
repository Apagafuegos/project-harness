# Milestone 9 plan — implementation in progress

**Status: implementation authorized, in progress.** The owner requested
implementation on 2026-11-05 and continued work until milestone acceptance.
The recommended options are the implementation baseline: A1, B1+B2, C1+C2,
D (no headline capability), E (v0.3.0) and F (opt-in live runs with the M8 budget).
Authorization to run is not evidence that credentials or live results exist.
No release or milestone-completion claim is made here.

## Starting point

- v0.2.0 is published. The v0.2.1 fixes are on `v021-fixes` (`b541554`, CI
  42114190337 green on both jobs). Since planning, the fixes were integrated on
  `main` as `406cda4` plus review follow-up `6467b9f`. The original commit changes
  the undo runtime, the sandbox settings and the dogfood driver, so M9
  implementation now builds on the integrated v0.2.1 head. The published release
  remains v0.2.0; live baseline availability must be recorded, not inferred from
  the version number.
- The M8 rerun ([report](dogfood/m8-rerun.md)) and its DeepSeek addendum left
  these items open, and they are the evidence for this plan:
  - **F07:** T4's rename ran as one `bash` call with a `python3` heredoc, so undo
    cannot reverse it, and the two edits shared one broad approval instead of two
    exact previews.
  - **F08:** T1, a read-only question, cost OpenAI 6 inferences and 15,396 input
    tokens (baseline: 3 and 4,312), because every small `list`/`read` result is
    re-sent as context.
  - **Masked exit status:** DeepSeek piped `cargo test` through `tail` in all five
    attempts, so the recorded exit status is `tail`'s, not cargo's. `bash` runs
    `bash --noprofile --norc -c COMMAND` without `pipefail`
    (`crates/core/src/tools/sandbox/args.rs`).
  - **Partial since M4:** Anthropic live compatibility and live cross-provider
    switching, because no Anthropic key has been supplied.
- Still deferred, and not part of M9: parallel read-only tools (m8-04). Decision
  7's share was 0% for OpenAI and 0.40% for DeepSeek, against a 10% threshold.

## Goal

The agent's edits are reviewable and reversible by default, its tool results
report what really happened, and a read-only answer costs less context. Every
change is measured with the same five-task dogfood method before and after.

## Steps

| Step | Scope | Exit evidence |
| --- | --- | --- |
| m9-00: baseline on v0.2.1 | Run T1–T5 unchanged on the released v0.2.1 binary, with the M8 rerun configuration and both providers, one run per cell. Add **T6**, a probe task asking for a multi-file mechanical change across three files, as a new cell kept outside the T1–T5 comparison. | Sanitized cells under `docs/dogfood/m9-baseline/`, `docs/dogfood/m9-baseline.md`, and the T6 oracle proven both ways by `--self-test` |
| m9-01: honest `bash` exit status | Decision A below. New plans record the shell options they run with, so approval binds them; recorded plans keep their recorded semantics. | Sandbox tests for a failing producer in a pipeline, an exact golden plan, and replay of an old plan unchanged; S02 green |
| m9-02a: edit-routing design | ADR 0034 for decision B: why models reach for `bash` mutations, the chosen remedy, its approval preview, undo coverage and record-version impact. Design only; reviewed before m9-02b. | Accepted ADR |
| m9-02b: implementation | Implement the accepted ADR 0034. A trusted batched edit is undoable through the existing ADR 0032 path, all-or-nothing, with an exact per-file preview. | Core, storage, undo and both client tests; the T6 cell is undoable in the rerun |
| m9-03: cheaper reads | Decision C below: line-addressed and multi-path `read`, keeping `read`'s policy, limits and D9 closure. The existing `read` definition and its golden plans stay byte-identical unless the decision changes them. | Tool, policy and golden-plan tests; T1 inferences and input tokens in the rerun |
| m9-04: Anthropic live check (conditional) | Run the existing opt-in live suites and a live cross-provider switch, only if a key is supplied. Otherwise the partial is carried forward unchanged. | Sanitized live evidence, or an explicit `key_unavailable` row |
| m9-05: rerun and v0.3.0 readiness | Rerun T1–T6 with the same method and providers, compare against m9-00, then write the M9 acceptance document, the changelog section, roadmap §9 and the user guide. Tagging stays with the owner. | `docs/dogfood/m9-rerun.md`, `docs/milestone-9-acceptance.md`, exact-head CI |

Order: m9-00 and m9-02a can run in parallel once accepted. m9-01, m9-02b and
m9-03 need v0.2.1 on `main` and touch separate files, except for the shared tool
registry, which m9-02b and m9-03 must edit in sequence. m9-05 comes last.

## Implementation decisions

**A. Masked exit status (m9-01).** Recommended: **A1**.

- **A1:** new `bash` plans run with `-o pipefail`, which is shown in the approval
  preview and recorded in the plan. This changes what a pipeline's status means,
  so it applies only to new plans.
- **A2:** leave the shell unchanged and append the `PIPESTATUS` array to the
  result when a pipeline's statuses differ. The model sees the truth, but the
  recorded exit code is still the last command's.
- **A3:** no product change, only a sentence in the base instruction.

**B. Mutations routed through `bash` (F07, m9-02).** Recommended: **B1 + B2**,
with B3 left as a design note.

- **B1:** a trusted **multi-edit** (several exact replacements, across one or more
  files, in one call and one approval with a per-file preview). This removes the
  reason the model collapsed T4 into one `bash` call, and keeps the change
  undoable.
- **B2:** feedback in the result. When an approved `bash` call changed workspace
  files, its result says those changes are outside undo and that `edit` or the
  multi-edit tool is the reversible path.
- **B3:** workspace checkpoints taken before each mutating `bash` call. This
  covers every effect, but it is the "filesystem checkpoints" item from Outside
  v0.2, and it has real disk, ignore-rule and recovery costs. Recommended as an
  ADR for a later milestone, not as M9 code.

**C. Read cost (F08, m9-03).** Recommended: **C1 + C2**.

- **C1:** `read` accepts an optional line range (`start_line`, `line_count`)
  next to its byte offsets, with the same 1 MiB bound.
- **C2:** `read` accepts up to 8 paths in one call, each with its own bound and
  its own explicit truncation. It remains one `Read` plan under
  `workspace_read`.
- **C3:** no tool change. Rely on `observation_masking` and record the cost
  only.

**D. A headline capability.** Recommended: **none in M9**.

From the Outside v0.2 list, the MCP client and subagents are the realistic
candidates. An MCP server is an external executable, so it must run under
Bubblewrap with network denied. Until a constrained-egress backend exists, only
local, offline servers would work. Subagents conflict with the
sequential-execution and single-unresolved-intent rules and would need their own
journal design. Each one needs an ADR before code. If you want one in M9, it
replaces m9-03 rather than adding to it, to keep the milestone the same size.

**E. Release target.** Recommended: M9 ends in **v0.3.0**, because new tool
arguments and possibly a new tool are a minor change. Whether a record-version
bump is needed is settled in ADR 0034, not assumed here.

**F. Live runs.** Approve the same budget as M8: OpenAI through the read-only
Codex login and the DeepSeek key, one run per cell, for m9-00 and m9-05. An
Anthropic key is optional (m9-04).

## Constraints carried forward

Rust only. History stays append-only, and tool execution stays sequential. Every
external executable uses Bubblewrap, with network denied and no unrestricted
fallback. Approval binds the exact plan. No change may weaken protected paths,
the sandbox or approval binding to cut the prompt count. Tests stay offline, and
live checks stay opt-in. Readers keep accepting record versions 1–4.
