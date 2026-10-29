# Contract 1: Model Provider

Status: High-level behavior provisionally agreed. Concrete types, lifecycle,
and defaults are proposed in [RFC-001](../rfcs/001-core-types-state-persistence.md).
The stream seam and deterministic scripted mock are implemented in
`harness_core::model` ([decision 0005](../decisions/0005-provider-stream.md)).
Milestone 4 step 01a adds complete tool/options input and fingerprints,
owner-scoped native state/validated continuation replay, read-only request
artifact access and core-enforced inference deadlines
([decision 0014](../decisions/0014-model-seam.md)).
No native provider adapter exists yet.

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
- Instructions with explicit precedence; the RFC proposes ordered system/developer text.
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
The eventual Rust shape must support both without exposing hidden reasoning as
ordinary user-visible text.

State must be serializable if needed to resume a persisted session, treated as
sensitive, and omitted from ordinary logs/inspection displays. A provider/model
switch must reject incompatible state or explicitly rebuild supported context.
Never silently forward it to another adapter. Remote continuation may expire;
failure or replay fallback must be visible and must not duplicate tool effects.

## Streaming output

The conceptual interface is:

```rust,ignore
pub trait ModelProvider: Send + Sync {
    fn stream(&self, request: ModelRequest) -> ModelEventStream;
}
```

The RFC refines this sketch with an owned stream and operation controls for
cancellation/deadlines. Setup failures become terminal stream events.

Proposed logical events:

- `ResponseStarted`: stable response identity, selected model, and safe metadata.
- `TextDelta`: append text to an identified output item/block.
- `ToolCallStarted`: identify the response-scoped call, item, and tool name.
- `ToolArgumentsDelta`: append an argument fragment for that call; it may be
  incomplete JSON and is never executable.
- `ToolCallReady`: the fully assembled argument value and stable call association.
- `UsageUpdated`: usage reported so far, with explicit availability and semantics.
- `ResponseFinished`: successful completion, finish reason, and available final
  usage/continuation state.

An error or cancellation is a distinct terminal outcome, not successful
completion. The RFC proposes a single terminal event with completed, failed,
or cancelled outcomes and nullable cumulative usage snapshots.

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
8. Stream backpressure must be bounded. Adapters must not silently discard tool
   fragments or lifecycle events. Buffer limits, overflow behavior, and cleanup
   must be specified and tested with the eventual async interface.
9. Transport/protocol failures preserve the distinction between partial output
   and a completed response. Adapters do not silently restart a stream after
   publishing content; retries are explicit engine policy.

## Failures

Structured failures need a category, safe diagnostic, optional provider code,
retry guidance or delay where known, and enough information to recognize partial
output. Proposed categories are invalid request, unsupported capability,
authentication, rate limit, transport, provider failure, malformed stream,
context overflow, and cancellation. Provider refusal is a finish outcome when
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

A common conformance suite should run against mock/native-event fixtures for
each adapter. The first live adapter is planned for OpenAI Responses, followed
by native Anthropic Messages. Verify their current official API specifications
when implementing; this draft does not prescribe their wire formats.

Live streaming text, complete tool-call reconstruction, usage, errors, and
cancellation must be checked explicitly. A second provider must work without
provider-specific branches in orchestration before this abstraction is
considered validated. Routine tests must not require credentials or network.

## Concrete specification

RFC-001 proposes owned boxed streams, cancellation tokens/deadlines, explicit
terminal outcomes, cumulative usage, bounded buffering, multimodal content,
scoped native state, configured capabilities, and diagnostic model switching.
Review those details there before implementation; they replace the original
list of unresolved interface choices. The second native provider remains an
abstraction-validation gate, not an excuse to discard native differences.
