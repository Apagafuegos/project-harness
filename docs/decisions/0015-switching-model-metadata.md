# 0015: Model metadata and explicit switch policy

Status: Implemented for Milestone 4, step 01b.

Date: 2026-10-29

## Decision

The application supplies `ModelCatalog`. Public `ModelInfo` combines
`ModelLimits` with `ModelCapabilities`: accepted `InputMediaKind`s (image,
audio, file), tool support and reasoning-effort support. A missing catalog
entry is text-only with tools, no reasoning-effort control and unknown limits.
Limits-only entries use the same capability defaults. Adapters can construct
static metadata; there is no discovery, model-name guessing or HTTP in core.
Reasoning metadata describes support; adapters still validate every option.

`Command::SwitchModel { model, policy }` is idle-only and recovery-blocked.
`Command::switch_model(model)` chooses `Reject`. Before allocating an entry or
persisting, the pure context builder builds the existing target preview. Its
`would_stop_being_portable` lists optional native state and continuations with
entry ID, original part, owner, schema and byte length, never payload. Required
incompatibilities name every projected user media block the target cannot take,
and every projected tool call and result when tools are unsupported. Covered,
failed and orphaned dialogue is classified according to the existing projection,
not by scanning unrelated history. Source block indices remain journal indices.

- `Reject` refuses any optional loss or required incompatibility, returning
  `Rejection::PortabilityLoss` (`portability_loss`) with the classified preview.
- `OmitOptional` refuses required incompatibilities and otherwise persists
  `ModelChanged` with `omit_optional`. `Accepted::ModelSwitched` and the durable
  `EventKind::ModelSwitched` carry the same full preview.
- `ConvertSupported` always returns `Invalid("unsupported_policy")` in M4.
  Conversion is planned, not implemented; opaque state is never translated.

No storage rule, journal schema or `ModelChanged` field changes. Only the
existing portability value is persisted. Reports are derived command/event
metadata. A refusal writes nothing. Historical entries remain unchanged and
switching back can restore owner-scoped state; only the latest eligible response
can supply a continuation, so a later B response prevents reuse of A's handle.
Unknown budgets remain inspectable and still prevent inference, not switching.

## Shared adapter contract

The public `model::conformance` module is behind the off-by-default
`conformance` feature, with no additional dependencies. Adapters provide named
native fixtures and normalized expectations, a provider factory, and request and
release observations. The same suite runs against `ScriptedProvider`, including
intentionally broken scripts that prove each validator can reject violations.
Native adapter queue bounds and live-provider compatibility remain separate gates.
See the model contract for the fixture-suite API and usage.

## Evidence

- `switch_required_media_rejected_under_both_policies`: image, audio and file,
  both supported policies, no writes, unsupported conversion.
- `switch_required_tools_rejected_and_clean_switch_policies`: tool call/result,
  clean switches, default constructor and conversion refusal.
- `switch_preview_classifies_every_projected_tool_part_and_unknown_model_defaults`:
  exact original indices and catalog defaults.
- `native_replay_survives_crash_reopen_and_is_omitted_after_model_switch`:
  reject with classified metadata, explicit omission, next request owner scoping,
  switch back restores state but not an older continuation.
- The feature-gated conformance tests run in `scripts/check.sh` and both CI jobs.
