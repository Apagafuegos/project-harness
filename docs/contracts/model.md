# Contract 1: Model Provider

Status: Implemented v0.1 boundary. [RFC-001](../rfcs/001-core-types-state-persistence.md)
is the original design; accepted ADRs and source tests below settle concrete details.
The stream seam and deterministic scripted mock are implemented in
`harness_core::model` ([decision 0005](../decisions/0005-provider-stream.md)).
Milestone 4 step 01a adds complete tool/options input and fingerprints,
owner-scoped native state/validated continuation replay, read-only request
artifact access and core-enforced inference deadlines
([decision 0014](../decisions/0014-model-seam.md)).
Milestone 4 step 01b adds catalog capabilities, enforced model-switch policies
([decision 0015](../decisions/0015-switching-model-metadata.md)) and a public
fixture conformance suite. OpenAI Responses, Anthropic Messages and the DeepSeek
Messages-compatible profile run that suite through real native adapters
([providers](../providers.md), [ADR 0024](../decisions/0024-deepseek-profile.md),
[Milestone 4 acceptance](../milestone-4-acceptance.md)). M7 step 02 adds explicit
projection-only `ConvertSupported` ([ADR 0015 amendment](../decisions/0015-switching-model-metadata.md));
M7-01 records sanitized live Codex and DeepSeek answer evidence while Anthropic
live compatibility and live cross-provider switching remain partial.

## Purpose and ownership

The model contract lets provider-independent orchestration request inference and
observe its lifecycle. Provider adapters translate native APIs without hiding
capabilities or discarding information required for valid continuation.

It does not execute tools, authorize actions, persist sessions, choose context,
or create a Tokio runtime. Secrets belong to adapter configuration, not model
requests, event payloads, or session history.

## Identity and capabilities

Distinguish the configured provider instance from the selected model identifier.
Two instances can use the same adapter/model with different endpoints or accounts.
Identity must not embed credentials.

Model metadata describes supported input/output modalities, tool calling,
reasoning controls, continuation/caching support, and known context/output
limits. Unknown limits remain unknown, rather than being invented defaults.
Pricing is optional metadata and must not drive correctness.

Adapters validate requested capabilities before opening a stream where possible.
Unsupported required features produce a structured error rather than being
silently dropped. Provider-specific options are namespaced and validated by the
owning adapter.

## Input

A `ModelRequest` conceptually contains:

- Provider instance and model identity.
- Ordered conversation items with structured content, including text, supported
  media references, assistant tool-call proposals, and associated tool results.
- Ordered system/developer instructions recorded at creation, including user,
  root and bounded nested project instructions ([ADR 0022](../decisions/0022-recorded-model-instructions.md)).
- Enabled tool definitions with names, descriptions, and input schemas.
- Options such as output budget, reasoning controls, or sampling parameters,
  subject to capability validation.
- Optional provider-owned continuation and replay state.

Conversation items cannot be reduced to role plus a string. Tool-call/result
associations and provider-owned blocks must survive storage and projection.
Ordering across multiple content blocks must remain representable.

The context builder produces the logical input. The adapter owns native request
formatting. Whether instructions use dedicated fields or native message roles
is an adapter concern, subject to preserving their intended precedence.

## Provider-specific state

Use an opaque, tagged envelope for continuation or native replay data that the
common format cannot represent safely. Its owner identity and schema version
must be explicit; its payload is not something orchestration interprets.

Response-level continuation and per-item native state are different lifetimes.
A previous-response handle alone cannot represent every provider's replay needs.
The Rust seam supports both without exposing hidden reasoning as ordinary
user-visible text. Neither shipped native adapter emits response-level continuations;
core continuation tests are not evidence of that provider feature.

State must be serializable if needed to resume a persisted session, treated as
sensitive, and omitted from ordinary logs/inspection displays. A provider/model
switch must reject incompatible state or explicitly rebuild supported context.
Never silently forward it to another adapter. Remote continuation may expire;
failure or replay fallback must be visible and must not duplicate tool effects.

## Streaming output

The conceptual interface is:

```rust,ignore
pub trait ModelProvider: Send + Sync {
    fn stream(&self, request: ModelRequest, control: OperationControl) -> ModelEventStream;
}
```

The owned stream accepts cancellation/deadline controls. Setup failures become
terminal stream events. The concrete `ProviderEvent` names (ADR 0005/0014) are:

<!-- The descriptions below summarize the typed seam, not wire event names. -->

- `Started`: response opened; identity comes from its request.
- `TextDelta` / `ReasoningSummaryDelta`: displayable text by block index.
- `ToolCallStarted` / `ToolArgumentsDelta` / `ToolCallReady`: response-local
  indices, native identity and fully parsed arguments; fragments never execute.
- `UsageUpdated`: nullable cumulative replacement snapshots.
- `ProviderState`: opaque replay bytes/schema at a fresh block index; the core
  publishes owner-tagged artifacts.
- `Finished`: one completed, failed or cancelled outcome with finish reason/error
  and optional native continuation (completed only).

The core allocates engine call IDs. Failure/cancellation is never success.

## Behavioral guarantees

1. A successful opened response has one start before its content and one terminal
   outcome. A setup failure may occur before a start. A stream that ends without
   a terminal outcome is interrupted, never implicitly successful.
2. Events are observed in adapter emission order. Each item/block/call has stable
   identity within the response; interleaved calls cannot be associated by arrival
   position alone. Native identifiers are retained where required for replay.
3. Argument fragments are assembled in order per call. `ToolCallReady` is emitted
   at most once per call, after assembly and JSON parsing. Tool schema, semantic,
   and authorization checks still belong to the tool runtime.
4. The engine must wait for successful response completion with an acceptable
   finish reason before dispatching proposals. No tool is executed from partial,
   cancelled, malformed, or interrupted output in the initial implementation.
5. A finish reason distinguishes a normal answer, tool proposals requiring
   orchestration, output-limit truncation, and provider refusal where available.
   An incomplete tool call is never promoted to a ready call on truncation.
6. Usage is optional. Missing usage is not zero. Reports identify whether counts
   are cumulative or incremental; final usage must not be double-counted. Cached
   and reasoning token details remain distinguishable when provided.
7. Cancellation stops future engine consumption and promptly requests transport
   cleanup. It does not promise that the provider stopped billing or computation.
   Partial content remains identifiable as partial. Dropping a stream must not
   leave a detached task producing events indefinitely.
8. Stream backpressure is bounded by event count and bytes. Adapters do not
   silently discard tool fragments or lifecycle events; overflow and cleanup
   behavior is explicit and covered by the shared transport tests.
9. Transport/protocol failures preserve the distinction between partial output
   and a completed response. Adapters do not silently restart a stream after
   publishing content; retries are explicit engine policy.

## Failures

Structured failures carry a category, safe diagnostic, optional provider code,
retry guidance or delay where known, and enough information to recognize partial
output. The implemented codes are `invalid_request`, `unsupported_capability`,
`authentication`, `rate_limit`, `transport`, `provider_failure`,
`malformed_stream`, `limit_exceeded` and `context_overflow`; cancellation is a
terminal outcome. Provider-supplied `code`/`type` strings are admitted only from
an explicit protocol vocabulary (or the transport's `http_<status>`), because an
echoed value could otherwise persist request text or a credential (audit A02). Provider refusal is a finish outcome when
the API represents it as a completed response.

Raw headers, credentials, and unrestricted request/response bodies must not be
part of routine error messages. Retryable means another inference attempt may
be appropriate; it does not authorize replaying tools or hiding duplicate text.

## Validation gates

The mock adapter must support deterministic scripts covering:

- Ordered text and interleaved tool-call fragments, including malformed JSON.
- Setup failure, interruption after partial output, and provider failure.
- Optional usage, usage updates, truncation, and refusal.
- Cancellation during setup/streaming, stream drop, and bounded buffering.
- Native replay state preservation and rejection of incompatible state.

The common conformance suite runs against mock and native-event fixtures for
each adapter with exactly one of each of the 17 scenarios. OpenAI Responses,
native Anthropic Messages and the DeepSeek Messages profile are implemented;
their ADRs cite the official API specifications and own their wire mappings.
This contract does not prescribe provider wire formats.

A second provider works without provider-specific branches in orchestration,
which is what validates the abstraction; the shipped adapter count exceeds that
bar. Live streaming text, complete tool-call reconstruction, usage, errors and
cancellation are checked only in the separate, explicit opt-in live scripts.
Routine tests require neither credentials nor network, and no automatic
inference retry, refresh-token rotation or expired-continuation replay exists.

## Public adapter conformance suite

Enable core's off-by-default `conformance` Cargo feature in the adapter's test
dependency (no new dependencies are added by that feature). The public module is
`harness_core::model::conformance`:

```rust,ignore
#[tokio::test]
async fn recorded_native_fixtures_conform() {
    harness_core::model::conformance::run(&RecordedFixtures, &manifest())
        .await.unwrap();
}
```

Implement `FixtureFactory::build(&self, fixture: &str) -> Fixture`. Each fresh
fixture contains `Arc<dyn ModelProvider>` and an `observe` closure over its native
fixture transport. `Observation` reports `submitted: Option<SubmittedRequest>`
(decoded from the **actual native request**, including every tool name,
description, schema and option) and `released: bool` (all stream-owned resources
released). Merely copying the incoming logical request is not transport evidence.

A manifest is `&[Case]`: each row has `kind: CaseKind`, a named `fixture`, the
full input `request`, independent normalized `expected: Expected`, and optional
`unsupported_option` (the exact required field/namespace name). `Expected`
specifies terminal outcome/reason/error, reassembled text/reasoning, tool names,
native call IDs and parsed JSON objects, ordered cumulative usage snapshots,
and metadata-only native block/continuation expectations. It never prints opaque
payloads. `run` requires exactly one of each of the 17 `CaseKind::ALL` scenarios;
`run_case` runs an individual row for focused diagnostics. See the external
consumer example in `crates/core/tests/model_conformance.rs` and complete
scripted manifest in `crates/core/src/model/conformance/tests.rs`.

Checks cover start/finish order, block identity and call-fragment lifecycle,
JSON object reconstruction, outcomes and retry delays, nullable replacement
usage snapshots, native schema/bounds, cancellation during pending setup and
streaming, drop release, an expired deadline, and lossless request options/tools
or a named `unsupported_capability`. Control and release checks are bounded by
the core's default two-second grace. Adapters may harmlessly ignore deadlines;
the core still enforces its own deadline. The adapter suite requires native
state only on `Completed`; this does not alter ADR 0014's defensive core
retention when the core itself fails an already partially received response.

`ScriptedProvider` runs the complete suite. Deliberately broken scripts test
every validator, including cross-kind index reuse and continuation on failure.
`./scripts/check.sh` also builds/tests the public feature and its external
consumer; no credentials or network are required. Native queue bounds are tested by the shared transport tests, and explicit
opt-in live checks remain separate from ordinary validation.

## Switching and implementation limits

[ADR 0015](../decisions/0015-switching-model-metadata.md) and its M7 amendment
implement idle-only Reject, OmitOptional and explicit ConvertSupported.
Conversion affects the projection only: unsupported user media becomes metadata
references; tool-less targets receive bounded text calls/results. Every converted
part is reported and the fingerprint hashes the converted input. Opaque native
state is omitted only under the explicit policy, never translated. Switching
back restores original capable content; history and artifacts are unchanged.
See `convert_supported_policy_matrix_is_projection_only`,
`convert_supported_opaque_state_is_omitted_never_textualized` and
`convert_supported_switch_back_restores_original_media` in core driver tests.

Both native adapters and DeepSeek's Messages profile pass fixture conformance;
Anthropic live compatibility and live cross-provider switching remain partial
(no key supplied). Unsupported native features are explicit capability errors;
no automatic retry, refresh-token rotation or remote-expiry replay fallback is
implemented. Provider-specific media/options and preserved-thinking limits live
in [providers](../providers.md). [M7 acceptance](../milestone-7-acceptance.md)
separates offline gates from bounded live evidence.
