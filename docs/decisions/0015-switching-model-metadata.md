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

## Amendment (m4-07): cross-provider evidence

Date: 2026-10-30

The policy, the preview and the journal are unchanged. Step 07 proves them
between two real adapters instead of the scripted mock, and adds the one piece
of plumbing the driver needed: it holds a single `Arc<dyn ModelProvider>`, so
`harness_providers::Router` introduces a `ModelProvider` that dispatches each
request by `request.input.model.provider_instance` to a registered adapter. No
core change was needed: `ModelRef` already names the instance, `ProviderState`
already carries an owner, and core already scopes replay to it. Routing by
anything else — a model name, an adapter type, a guess — would break
`Reject`/`OmitOptional`, because the instance is what owns native state.

`crates/providers/tests/switch.rs` runs a real `SessionHandle` with the router
and both adapters against attributed loopback fixture servers. It inspects the
logical request the runtime produced, the exact body and headers the adapter
submitted, and the published snapshot:

- OpenAI → Anthropic is refused under `Reject` with `PortabilityLoss` naming the
  encrypted reasoning entry (owner `openai-codex/gpt-6.1-sol`, schema
  `openai.responses.reasoning.v1`, `part=block[1]`, its byte count), and the
  journal sequence does not move. `OmitOptional` accepts exactly that preview;
  the next request is entirely Anthropic — no `ProviderState`, no
  `continuation`, no artifact reads at all.
- Anthropic → OpenAI is refused the same way for signed thinking
  (`anthropic.messages.thinking.v1`, `part=block[0]`), and after explicit
  omission the OpenAI body carries no reasoning item and no encrypted payload.
- Switching back replays only the owner's own state (`ProviderState` blocks
  whose owner equals the request model) and never a continuation, in both
  directions. Core-side, `native_replay_survives_crash_reopen_and_is_omitted_after_model_switch`
  remains the same proof with the mock.
- A required incompatibility cannot be omitted across providers either: with an
  image in the projection, switching to a catalogued Anthropic-instance model
  without image input is `PortabilityLoss` under both policies, with
  `required_incompatibilities[0].capability == Image`.
- A compaction summary generated by the independently configured summary model
  on the *other* provider instance arrives through the router (the summary
  request names that instance), and the summary is later projected as plain
  text (`Summary of earlier conversation: …`) for the other adapter. See the
  preserved-thinking amendment in ADR 0018 for what the receiving adapter adds.

Native tool-call ids are the one thing a switch changes about a request's
content: the stored call was issued by the other provider. Both adapters send
the stored id verbatim when it is a plain ASCII identifier of at most 64 bytes
(every id either provider issues is), and otherwise derive a deterministic
substitute from the harness `CallId`, used by both the call and its result on
this and every later request. Rewriting never varies per request, so it cannot
unpair a call from its result and cannot edit a preserved-thinking prefix
either. See `crates/providers/src/native_id.rs` and the request goldens in each
adapter's tests.

`harness ask` exposes the policy as `--switch-policy reject|omit-optional`
(ADR 0020 amendment).
