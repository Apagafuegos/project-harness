# 0036: Bind pipeline status semantics to bash plans

Status: Accepted for M9 option A1; implementation in progress.

Date: 2026-11-05

## Context

The DeepSeek M8 rerun piped test output through `tail`. A successful output
filter hid the producer's failure because bash normally returns only the last
pipeline component's status. Exit zero was not proof that tests passed.

## Decision

New bash plans append `"shell":{"pipefail":true}` after the optional sandbox
settings in their canonical bytes. This is trusted planner metadata, not a
model-supplied argument or a configurable mount/environment setting. Both
clients render the option from those exact bytes before approval, even without
an opt-in toolchain. The digest binds the option together with the command,
cwd, mounts, timeout, policy revision and call identity.

The production argv suffix is `bash --noprofile --norc -o pipefail -c COMMAND`.
A failing pipeline returns its rightmost nonzero component status; an entirely
successful pipeline returns zero. This does not enable `errexit`: a later
successful command in a command list can still determine the shell's final
status. Commands can explicitly change shell options or handle a pipeline
failure. A deliberately short-lived filter can cause the producer to receive
SIGPIPE. The result is the shell's status, not a claim that every command or
test succeeded. No retry, shell parsing or command-text rewriting is added.

Legacy plans omit `shell`, retaining their exact bytes and the old argv suffix.
An explicit `pipefail:false` is decoded as old pipeline behavior; new planners
never emit it. Invalid option types and unknown shell-option fields fail
parsing. Stored plans remain opaque artifacts; journal versions stay 1–4
readable and this option alone requires no journal-version change. M9 batched file images
independently require v5 writers (ADR 0034). No recorded approval or
result is rewritten. An interrupted bash intent is still uncertain and never
re-executed on reopen. New calls in an old session receive new plans with the
new option and require the existing authorization path.

## Verification

- Legacy bash goldens are retained; new goldens contain the bound shell option.
- Exact argv tests cover absent, false and true options. A separate integration
  target runs the native shell suffix: process spawning must not share a test
  process with journal-lock tests (ADR 0012). This does not prove isolation.
- Plan tests require changed digests when shell semantics change.
- Shared approval-summary tests cover new and legacy plans without toolchains.
- The ignored S02 test `s02_pipeline_failure_survives_output_filtering` runs
  failing/successful/multiple-failure pipelines through real Bubblewrap. Local
  shell tests are not a substitute for that required security gate.
