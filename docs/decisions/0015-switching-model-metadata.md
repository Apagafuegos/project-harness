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

## Amendment (M7 step 02): explicit ConvertSupported

`ConvertSupported` is implemented, using the existing stored
`ModelChanged.portability = convert_supported`. Storage already accepts the enum
in all journal schemas; neither the record version nor a schema changes.
The last model-change marker on the selected ancestry governs the entire
projection, including continuation-prefix rebuilds. No marker means no conversion.
A capable model override or switch-back always projects the original blocks.

Only displayable, required content converts, and only when the target lacks its
capability:

- A user image/audio/file becomes `[image omitted for this model: image/png,
  5 bytes, sha256 721c9525ade2]` (using its actual kind, stored MIME type, byte
  count and 12-hex SHA-256 prefix). Files additionally include the stored name,
  JSON-quoted. No artifact is read or described.
- An answered tool call becomes `[tool call NAME COMPACT_JSON]`; arguments are
  limited to 2,000 UTF-8 bytes, including `[truncated]` when needed. Original
  part indices and call order are retained.
- Its result becomes a user-role text item `[tool result NAME STATUS CODE?:
  TEXT]`, with original result order. Text is limited to 4,000 UTF-8 bytes,
  including the same truncation marker; any result media is represented by
  stored metadata, never interpreted. Status and an existing code are retained.
  No tool-shaped item/block or tool definition reaches a tool-less target.
- Native provider state and continuations retain exactly the owner/prefix/latest
  checks of OmitOptional. They have no conversion kind and are never translated.

`PreviewReport.would_convert` lists each original `entry_id`, `part`, missing
`capability` and conversion `kind` (`media_reference`, `tool_call_text`,
`tool_result_text`). `required_incompatibilities` continues to classify the
original required dialogue, even when conversion is authorized; Reject and
OmitOptional still refuse it. ConvertSupported refuses only a required part
without a corresponding supported conversion (there is no such part in v0.1).
Optional portability losses continue to be reported, and ConvertSupported
permits their omission.

`ContextReport.included[].conversions` is the included-item conversion marker,
using the same metadata. Both new lists default empty on decoding and are omitted
when empty; existing report readers in eval decode them through the shared core
types. The CLI's JSON context report exposes them and the TUI inspector renders
them explicitly. Estimates count the actual reference/rendered text. Projection
version 5 hashes that text; builder version 6 versions the new report fields.
The six context report goldens deliberately change versions and fingerprints.
History, artifacts, journal schemas and adapters remain unchanged.

`ask --switch-policy convert-supported` selects this policy explicitly. Ctrl-L
still tries Reject first; a refused preview with convertible items shows their
count and offers `c convert`, alongside `o confirm omit optional` (which still
cannot omit required dialogue). Nothing converts merely because it is shown.

Evidence: the real-storage driver media policy matrix, multiple-call builder
pairing test, opaque-state driver test, switch-back and capable-override checks,
canonical fingerprint/estimate checks, CLI flag test, TUI loop choice test and
randomized durable `ConvertSupported` coverage. All are offline.

Validation against `b2e627b`: core unit tests increase from 458 to 465 passing
(three ignored unchanged); ask CLI tests increase from five to six; the existing
14-test TUI navigation suite now exercises explicit conversion. The complete
`CARGO_NET_OFFLINE=true ./scripts/check.sh` passed, including sandbox/eval gates,
conformance, docs and deterministic examples, with a two-hour command timeout.

All four deliberate mutations failed their named behavioral check and were
reverted before the clean gate:

| Mutation | Failing test | Observed failure |
| --- | --- | --- |
| Convert under OmitOptional | `convert_supported_policy_matrix_is_projection_only` | OmitOptional projected text instead of the original image |
| Textualize signed native state as schema/digest text | `convert_supported_opaque_state_is_omitted_never_textualized` | Native thinking schema appeared in the recorded request |
| Hash original media instead of converted text | `switch_required_media_rejected_under_both_policies` | Raw and converted fingerprints were equal |
| Rewrite stored user media during storage replay | `convert_supported_switch_back_restores_original_media` | Switch-back request no longer contained the original image |

Each mutation test exited 101 with an assertion failure, not a compilation error.
The storage-replay mutation was reverted without any final change to storage.

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

## Amendment (M5): enumerating the catalog

`ModelCatalog::iter()` lists every configured model with its metadata, in
`ModelRef` order (provider instance, then model). It's read-only and doesn't
change how limits or capabilities are resolved. The TUI's model pickers use it
over the merged catalog that the shared wiring builds.
