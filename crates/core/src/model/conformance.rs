//! Fixture-driven adapter contract checks, available with the `conformance` feature.
//!
//! An adapter's tests implement [`FixtureFactory`] over recorded native events.
//! Each [`Case`] maps a mandatory [`CaseKind`] to a fixture, request, and independently
//! specified normalized [`Expected`] result. Call [`run`] from a Tokio test.
//! [`Fixture::observe`] must inspect the fixture *transport*, not merely copy the
//! incoming `ModelRequest`: decode the actual submitted native request back into
//! [`SubmittedRequest`] and observe transport release. The scripted provider is an
//! identity transport, so its recorded request is sufficient for its own tests.
//! Adapters emit native state only on completed responses. This is stricter than
//! the core's defensive retention of already accepted bounded blocks when the
//! core itself fails a response (ADR 0014); no core retention rules are changed.
//! No network, credentials, provider-owned state payloads, or runtime is created here.

use std::{collections::BTreeMap, future::poll_fn, sync::Arc, task::Poll, time::Duration};

use serde_json::Value;

use super::{
    InferenceOptions, ModelProvider, ModelRequest, OperationControl, ProviderEvent, ProviderOutcome,
};
use crate::{
    session::{FinishReason, ModelError, ModelErrorCode, Usage},
    tools::ToolDefinition,
};

/// Required fixture scenarios. A complete manifest contains each exactly once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CaseKind {
    /// Ordered text and absent usage.
    Text,
    /// One reconstructed tool call.
    OneTool,
    /// Several calls with interleaved fragments.
    InterleavedTools,
    /// Invalid JSON fails without a ready call.
    MalformedToolJson,
    /// Completed refusal.
    Refusal,
    /// Completed output-limit truncation.
    OutputLimit,
    /// Provider failure with retry guidance.
    ProviderError,
    /// Rate limit, without automatic retry.
    RateLimit,
    /// Failure before Started.
    SetupFailure,
    /// Multiple cumulative, nullable usage snapshots.
    Usage,
    /// Bounded native blocks and continuation on completion.
    NativeState,
    /// Cancellation while setup is pending.
    CancelSetup,
    /// Cancellation after Started while streaming is pending.
    CancelStreaming,
    /// Dropping a pending stream releases its transport.
    Drop,
    /// An expired deadline is honored or harmlessly ignored.
    Deadline,
    /// Every tool and configured option survives native request serialization.
    Request,
    /// A required option is explicitly refused by name.
    UnsupportedOption,
}
impl CaseKind {
    /// Mandatory cases, in stable order.
    pub const ALL: [Self; 17] = [
        Self::Text,
        Self::OneTool,
        Self::InterleavedTools,
        Self::MalformedToolJson,
        Self::Refusal,
        Self::OutputLimit,
        Self::ProviderError,
        Self::RateLimit,
        Self::SetupFailure,
        Self::Usage,
        Self::NativeState,
        Self::CancelSetup,
        Self::CancelStreaming,
        Self::Drop,
        Self::Deadline,
        Self::Request,
        Self::UnsupportedOption,
    ];
}

/// A fixture's independently specified normalized result. Fragment boundaries
/// need not match a particular transport's chunking.
#[derive(Clone, Debug, PartialEq)]
pub struct Expected {
    /// Terminal outcome.
    pub outcome: ProviderOutcome,
    /// Present exactly on completion.
    pub finish_reason: Option<FinishReason>,
    /// Exact safe structured failure, including provider code and retry delay.
    pub error: Option<ModelError>,
    /// Reassembled text, keyed by block index.
    pub text: BTreeMap<u32, String>,
    /// Reassembled displayable reasoning, keyed by block index.
    pub reasoning: BTreeMap<u32, String>,
    /// Ready calls, keyed by block index, with expected parsed JSON objects.
    pub tools: BTreeMap<u32, ExpectedTool>,
    /// Cumulative snapshots in order, not their sum. Empty means unavailable.
    pub usage: Vec<Usage>,
    /// Native block metadata only, keyed by block index.
    pub native: BTreeMap<u32, NativeMetadata>,
    /// Continuation metadata, never its sensitive payload.
    pub continuation: Option<NativeMetadata>,
}
impl Default for Expected {
    fn default() -> Self {
        Self {
            outcome: ProviderOutcome::Completed,
            finish_reason: Some(FinishReason::Stop),
            error: None,
            text: BTreeMap::new(),
            reasoning: BTreeMap::new(),
            tools: BTreeMap::new(),
            usage: Vec::new(),
            native: BTreeMap::new(),
            continuation: None,
        }
    }
}
/// A complete, executable proposal after assembly (schema validation is separate).
#[derive(Clone, Debug, PartialEq)]
pub struct ExpectedTool {
    /// Tool name.
    pub name: String,
    /// Native identifier retained for replay.
    pub provider_call_id: Option<String>,
    /// Independently specified object, not expected fragments.
    pub arguments: Value,
}
/// Safe native metadata. Failure diagnostics never print payloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeMetadata {
    /// Provider schema.
    pub schema: String,
    /// Payload length.
    pub bytes: usize,
}
/// One manifest row. The factory receives `fixture`, not `expected`.
#[derive(Clone, Debug)]
pub struct Case {
    /// Required scenario being exercised.
    pub kind: CaseKind,
    /// Adapter-defined recorded native fixture name.
    pub fixture: String,
    /// Input supplied unchanged to the real ModelProvider.
    pub request: ModelRequest,
    /// Independent normalized oracle.
    pub expected: Expected,
    /// For UnsupportedOption, the unsupported option's exact field/namespace name.
    pub unsupported_option: Option<String>,
}
/// Native request evidence decoded by the adapter's fixture transport.
#[derive(Clone, Debug, PartialEq)]
pub struct SubmittedRequest {
    /// Every serialized tool, including description and schema.
    pub tools: Vec<SubmittedTool>,
    /// Every serialized required option.
    pub options: InferenceOptions,
}
/// Provider-facing fields of a serialized tool definition. Execution backend
/// and declared effects belong to the harness, not the provider wire request.
#[derive(Clone, Debug, PartialEq)]
pub struct SubmittedTool {
    /// Tool name.
    pub name: String,
    /// Full description.
    pub description: String,
    /// Full input JSON schema.
    pub input_schema: Value,
}
impl From<&ToolDefinition> for SubmittedTool {
    fn from(tool: &ToolDefinition) -> Self {
        Self {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
        }
    }
}
/// Observable fixture transport state. Evidence must come from the transport.
#[derive(Clone, Debug, Default)]
pub struct Observation {
    /// Actual submitted request, or None if rejected before submission.
    pub submitted: Option<SubmittedRequest>,
    /// All stream-owned transport/producer resources have been released.
    pub released: bool,
}
/// A fresh provider configured over one fixture and its transport probe.
pub struct Fixture {
    /// The adapter under test, not a replay of normalized expected events.
    pub provider: Arc<dyn ModelProvider>,
    /// Snapshot the transport's submission and release evidence.
    pub observe: Arc<dyn Fn() -> Observation + Send + Sync>,
}
/// Application-supplied fixture factory, usable across crate boundaries.
pub trait FixtureFactory {
    /// Build a fresh provider and probe over a named native fixture.
    fn build(&self, fixture: &str) -> Fixture;
}
impl<F: Fn(&str) -> Fixture> FixtureFactory for F {
    fn build(&self, fixture: &str) -> Fixture {
        self(fixture)
    }
}
/// Safe check failure. Contains fixture name and check name, never event payloads.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("conformance {fixture}: {check}")]
pub struct Failure {
    /// Manifest fixture name.
    pub fixture: String,
    /// Stable failed check name.
    pub check: &'static str,
}
fn failure(case: &Case, check: &'static str) -> Failure {
    Failure {
        fixture: case.fixture.clone(),
        check,
    }
}

/// Run a complete manifest, checking required coverage before opening streams.
/// Each case is bounded by the core's default cancellation grace (two seconds).
/// Use `run_case` for focused failure diagnostics or deliberately broken fixtures.
pub async fn run(factory: &impl FixtureFactory, manifest: &[Case]) -> Result<(), Failure> {
    for kind in CaseKind::ALL {
        if manifest.iter().filter(|case| case.kind == kind).count() != 1 {
            return Err(Failure {
                fixture: format!("{kind:?}"),
                check: "manifest_coverage",
            });
        }
    }
    for case in manifest {
        run_case(factory, case).await?;
    }
    Ok(())
}

/// Run one row through the provider's real owned stream and transport probes.
/// This also validates that the row exercises its advertised scenario.
pub async fn run_case(factory: &impl FixtureFactory, case: &Case) -> Result<(), Failure> {
    validate_case(case)?;
    let fixture = factory.build(&case.fixture);
    let control = OperationControl {
        deadline: (case.kind == CaseKind::Deadline).then(tokio::time::Instant::now),
        ..OperationControl::default()
    };
    let cancel = control.cancel.clone();
    let mut stream = fixture.provider.stream(case.request.clone(), control);
    let mut events = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(2), async {
        if matches!(
            case.kind,
            CaseKind::CancelSetup | CaseKind::CancelStreaming | CaseKind::Drop
        ) {
            // Poll until genuinely pending, so cancellation/drop occurs during work,
            // not on an unopened or already exhausted stream.
            loop {
                let next = poll_fn(|cx| Poll::Ready(stream.as_mut().poll_next(cx))).await;
                match next {
                    Poll::Pending
                        if case.kind == CaseKind::CancelStreaming
                            && !events
                                .iter()
                                .any(|event| matches!(event, ProviderEvent::Started)) =>
                    {
                        // Real adapters commonly wait asynchronously during setup.
                        // Reach Started before trying to cancel *streaming*.
                        let event = poll_fn(|cx| stream.as_mut().poll_next(cx)).await;
                        let Some(event) = event else {
                            return Err(failure(case, "control_not_pending"));
                        };
                        events.push(event);
                        if events.len() > 4096 {
                            return Err(failure(case, "event_bound"));
                        }
                    }
                    Poll::Pending => break,
                    Poll::Ready(Some(event)) => {
                        events.push(event);
                        if events.len() > 4096 {
                            return Err(failure(case, "event_bound"));
                        }
                    }
                    Poll::Ready(None) => return Err(failure(case, "control_not_pending")),
                }
            }
            let started = events
                .iter()
                .any(|event| matches!(event, ProviderEvent::Started));
            if (case.kind == CaseKind::CancelSetup && !events.is_empty())
                || (case.kind == CaseKind::CancelStreaming && !started)
            {
                return Err(failure(case, "control_phase"));
            }
            if case.kind == CaseKind::Drop {
                return Ok(());
            }
            cancel.cancel();
        }
        while let Some(event) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
            events.push(event);
            if events.len() > 4096 {
                return Err(failure(case, "event_bound"));
            }
        }
        check_events(case, &events)
    })
    .await
    .unwrap_or_else(|_| Err(failure(case, "control_grace")));
    drop(stream);
    result?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while !(fixture.observe)().released {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .map_err(|_| failure(case, "drop_release"))?;
    let observation = (fixture.observe)();
    if case.kind == CaseKind::UnsupportedOption {
        let name = case.unsupported_option.as_deref().unwrap_or_default();
        let Some(error) = &case.expected.error else {
            return Err(failure(case, "unsupported_option"));
        };
        if error.code != ModelErrorCode::UnsupportedCapability
            || !(error.message.contains(name) || error.provider_code.as_deref() == Some(name))
        {
            return Err(failure(case, "unsupported_option"));
        }
    } else if observation.submitted.is_some()
        || !matches!(
            case.kind,
            CaseKind::CancelSetup | CaseKind::SetupFailure | CaseKind::Drop | CaseKind::Deadline
        )
    {
        let expected = SubmittedRequest {
            tools: case
                .request
                .input
                .tools
                .iter()
                .map(SubmittedTool::from)
                .collect(),
            options: case.request.input.options.clone(),
        };
        if observation.submitted.as_ref() != Some(&expected) {
            return Err(failure(case, "request_preservation"));
        }
    }
    Ok(())
}

fn validate_case(case: &Case) -> Result<(), Failure> {
    use CaseKind::*;
    let expected = &case.expected;
    let valid = match case.kind {
        Text => expected.text.values().any(|text| !text.is_empty()) && expected.usage.is_empty(),
        OneTool => expected.tools.len() == 1,
        InterleavedTools => expected.tools.len() >= 2,
        MalformedToolJson => expected
            .error
            .as_ref()
            .is_some_and(|e| e.code == ModelErrorCode::MalformedStream),
        Refusal => expected.finish_reason == Some(FinishReason::Refusal),
        OutputLimit => expected.finish_reason == Some(FinishReason::OutputLimit),
        ProviderError => expected.error.as_ref().is_some_and(|e| {
            e.code == ModelErrorCode::ProviderFailure && e.retry_after_ms.is_some()
        }),
        RateLimit => expected
            .error
            .as_ref()
            .is_some_and(|e| e.code == ModelErrorCode::RateLimit && e.retry_after_ms.is_some()),
        SetupFailure => expected.outcome == ProviderOutcome::Failed,
        Usage => expected.usage.len() >= 2 && expected.usage.windows(2).any(|v| v[0] != v[1]),
        NativeState => {
            !expected.native.is_empty()
                && expected.continuation.is_some()
                && expected.outcome == ProviderOutcome::Completed
        }
        Request => {
            case.request.input.tools.len() >= 2
                && case.request.input.options.reasoning_effort.is_some()
                && !case.request.input.options.provider_options.is_empty()
        }
        UnsupportedOption => case
            .unsupported_option
            .as_ref()
            .is_some_and(|name| !name.is_empty() && option_present(&case.request, name)),
        CancelSetup | CancelStreaming | Drop | Deadline => true,
    };
    if valid {
        Ok(())
    } else {
        Err(failure(case, "manifest_scenario"))
    }
}
fn option_present(request: &ModelRequest, name: &str) -> bool {
    match name {
        "max_output_tokens" => true,
        "reasoning_effort" => request.input.options.reasoning_effort.is_some(),
        _ => request.input.options.provider_options.contains_key(name),
    }
}

#[derive(Default)]
struct Tool {
    name: String,
    provider_call_id: Option<String>,
    fragments: String,
    ready: bool,
}
fn check_events(case: &Case, events: &[ProviderEvent]) -> Result<(), Failure> {
    let mut actual = Expected::default();
    let mut started = false;
    let mut finished = false;
    let mut kinds = BTreeMap::new();
    let mut tools = BTreeMap::<u32, Tool>::new();
    let mut native_bytes = 0usize;
    let mut fragment_order = Vec::new();
    for (position, event) in events.iter().enumerate() {
        if finished {
            return Err(failure(case, "finished_last"));
        }
        if !started
            && !matches!(
                event,
                ProviderEvent::Started | ProviderEvent::Finished { .. }
            )
        {
            return Err(failure(case, "started_first"));
        }
        match event {
            ProviderEvent::Started => {
                if started || position != 0 {
                    return Err(failure(case, "started_once"));
                }
                started = true;
            }
            ProviderEvent::TextDelta { block_index, text }
            | ProviderEvent::ReasoningSummaryDelta { block_index, text } => {
                let reasoning = matches!(event, ProviderEvent::ReasoningSummaryDelta { .. });
                let kind = if reasoning { "reasoning" } else { "text" };
                if kinds
                    .insert(*block_index, kind)
                    .is_some_and(|old| old != kind)
                {
                    return Err(failure(case, "block_kind_reused"));
                }
                let content = if reasoning {
                    &mut actual.reasoning
                } else {
                    &mut actual.text
                };
                content.entry(*block_index).or_default().push_str(text);
            }
            ProviderEvent::ToolCallStarted {
                block_index,
                name,
                provider_call_id,
            } => {
                if kinds.insert(*block_index, "tool").is_some() {
                    return Err(failure(case, "duplicate_block"));
                }
                if name.is_empty() {
                    return Err(failure(case, "tool_name"));
                }
                tools.insert(
                    *block_index,
                    Tool {
                        name: name.clone(),
                        provider_call_id: provider_call_id.clone(),
                        ..Tool::default()
                    },
                );
            }
            ProviderEvent::ToolArgumentsDelta {
                block_index,
                fragment,
            } => {
                let Some(tool) = tools.get_mut(block_index) else {
                    return Err(failure(case, "tool_fragment_lifecycle"));
                };
                if tool.ready {
                    return Err(failure(case, "tool_fragment_lifecycle"));
                }
                tool.fragments.push_str(fragment);
                fragment_order.push(*block_index);
            }
            ProviderEvent::ToolCallReady { block_index } => {
                let Some(tool) = tools.get_mut(block_index) else {
                    return Err(failure(case, "tool_ready_lifecycle"));
                };
                if tool.ready {
                    return Err(failure(case, "tool_ready_lifecycle"));
                }
                let Ok(arguments @ Value::Object(_)) =
                    crate::session::codec::unique_json(tool.fragments.as_bytes())
                else {
                    return Err(failure(case, "tool_json_object"));
                };
                tool.ready = true;
                actual.tools.insert(
                    *block_index,
                    ExpectedTool {
                        name: tool.name.clone(),
                        provider_call_id: tool.provider_call_id.clone(),
                        arguments,
                    },
                );
            }
            ProviderEvent::ProviderState {
                block_index,
                schema,
                payload,
            } => {
                if kinds.insert(*block_index, "native").is_some() {
                    return Err(failure(case, "duplicate_block"));
                }
                check_native(case, schema, payload.len(), &mut native_bytes)?;
                actual.native.insert(
                    *block_index,
                    NativeMetadata {
                        schema: schema.clone(),
                        bytes: payload.len(),
                    },
                );
            }
            ProviderEvent::UsageUpdated { usage } => actual.usage.push(usage.clone()),
            ProviderEvent::Finished {
                outcome,
                finish_reason,
                error,
                continuation,
            } => {
                finished = true;
                if (*outcome == ProviderOutcome::Completed) != finish_reason.is_some()
                    || (*outcome == ProviderOutcome::Failed) != error.is_some()
                {
                    return Err(failure(case, "terminal_shape"));
                }
                if continuation.is_some() && *outcome != ProviderOutcome::Completed {
                    return Err(failure(case, "continuation_on_failure"));
                }
                if !actual.native.is_empty() && *outcome != ProviderOutcome::Completed {
                    return Err(failure(case, "native_state_on_failure"));
                }
                if *outcome == ProviderOutcome::Completed && !started {
                    return Err(failure(case, "started_first"));
                }
                if let Some(native) = continuation {
                    check_native(
                        case,
                        &native.schema,
                        native.payload.len(),
                        &mut native_bytes,
                    )?;
                    actual.continuation = Some(NativeMetadata {
                        schema: native.schema.clone(),
                        bytes: native.payload.len(),
                    });
                }
                if *outcome == ProviderOutcome::Completed
                    && *finish_reason == Some(FinishReason::ToolCalls)
                    && (tools.is_empty() || tools.values().any(|tool| !tool.ready))
                {
                    return Err(failure(case, "tool_finish"));
                }
                if *outcome == ProviderOutcome::Completed
                    && *finish_reason != Some(FinishReason::ToolCalls)
                    && tools.values().any(|tool| tool.ready)
                {
                    return Err(failure(case, "tool_finish"));
                }
                actual.outcome = *outcome;
                actual.finish_reason = *finish_reason;
                actual.error = error.clone();
            }
        }
    }
    if !finished && !actual.native.is_empty() {
        return Err(failure(case, "native_state_on_failure"));
    }
    if matches!(
        case.kind,
        CaseKind::CancelSetup | CaseKind::CancelStreaming | CaseKind::Deadline
    ) && (!finished || actual.outcome == ProviderOutcome::Cancelled)
    {
        return Ok(());
    }
    if !finished {
        return Err(failure(case, "finished_once"));
    }
    if matches!(case.kind, CaseKind::CancelSetup | CaseKind::CancelStreaming) {
        return Err(failure(case, "cancel_outcome"));
    }
    if case.kind == CaseKind::SetupFailure && started {
        return Err(failure(case, "setup_failure_started"));
    }
    if case.kind == CaseKind::InterleavedTools
        && !fragment_order
            .windows(3)
            .any(|v| v[0] == v[2] && v[0] != v[1])
    {
        return Err(failure(case, "interleaved_fragments"));
    }
    if case.kind == CaseKind::MalformedToolJson
        && !tools.values().any(|tool| {
            !matches!(
                crate::session::codec::unique_json(tool.fragments.as_bytes()),
                Ok(Value::Object(_))
            )
        })
    {
        return Err(failure(case, "malformed_fixture"));
    }
    if actual != case.expected {
        return Err(failure(case, "normalized_outcome"));
    }
    Ok(())
}
fn check_native(case: &Case, schema: &str, bytes: usize, total: &mut usize) -> Result<(), Failure> {
    *total = total.saturating_add(bytes);
    if schema.is_empty() {
        return Err(failure(case, "native_schema"));
    }
    if bytes > 256 * 1024 || *total > 4 * 1024 * 1024 {
        return Err(failure(case, "native_bounds"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
