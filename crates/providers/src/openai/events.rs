//! Responses streaming events to normalized `ProviderEvent`s.
//!
//! The mapping is total for the documented event vocabulary: every event is
//! either mapped, deliberately ignored, or counted as unknown. Only a
//! structurally impossible stream (a delta for an unknown item, invalid JSON,
//! a missing terminal event) becomes `malformed_stream`.

use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::Ordering},
};

use harness_core::{
    model::{ProviderEvent, ProviderOutcome},
    session::{FinishReason, ModelError, ModelErrorCode, Usage},
};
use serde_json::Value;

use super::{Counters, Profile, STATE_SCHEMA};
use crate::sse::SseEvent;

/// Largest accepted encrypted-reasoning item; the core enforces the same bound.
const MAX_NATIVE_BYTES: usize = 256 * 1024;

/// Event types the adapter knows about but deliberately does not map.
const IGNORED: [&str; 11] = [
    "response.content_part.added",
    "response.content_part.done",
    "response.output_text.done",
    "response.refusal.done",
    "response.reasoning_text.delta",
    "response.reasoning_text.done",
    "response.reasoning_summary_part.added",
    "response.reasoning_summary_part.done",
    "response.reasoning_summary_text.done",
    "response.function_call_arguments.done",
    "response.queued",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Reasoning,
    Tool,
}

struct Block {
    index: u32,
    kind: BlockKind,
}

struct Tool {
    fragments: String,
    ready: bool,
}

/// One response's mapping state. Owned by the stream, never shared.
pub(crate) struct Mapper {
    profile: Profile,
    started: bool,
    finished: bool,
    next_index: u32,
    blocks: BTreeMap<String, Block>,
    tools: BTreeMap<u32, Tool>,
    refusal: bool,
    counters: Arc<Counters>,
}

impl Mapper {
    pub(crate) fn new(profile: Profile, counters: Arc<Counters>) -> Self {
        Self {
            profile,
            started: false,
            finished: false,
            next_index: 0,
            blocks: BTreeMap::new(),
            tools: BTreeMap::new(),
            refusal: false,
            counters,
        }
    }

    /// Whether a terminal event has been produced; the stream must then end.
    pub(crate) fn finished(&self) -> bool {
        self.finished
    }

    /// A clean end of stream before any terminal event is malformed.
    pub(crate) fn finish_eof(self) -> Result<Vec<ProviderEvent>, ModelError> {
        if self.finished {
            Ok(Vec::new())
        } else {
            Err(malformed("the stream ended without a terminal event"))
        }
    }

    /// Map one framed SSE event. Errors are terminal `malformed_stream`/bound
    /// failures; the caller drops the connection and reports them.
    pub(crate) fn push(&mut self, event: &SseEvent) -> Result<Vec<ProviderEvent>, ModelError> {
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|_| malformed("the SSE data field is not valid JSON"))?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| malformed("the SSE event has no type"))?
            .to_owned();
        let mut out = Vec::new();
        match kind.as_str() {
            "response.created" | "response.in_progress" => {
                self.start(&mut out);
                if let Some(usage) = usage_of(&value) {
                    out.push(ProviderEvent::UsageUpdated { usage });
                }
            }
            "response.output_item.added" => self.item_added(&value, &mut out)?,
            "response.output_item.done" => self.item_done(&value, &mut out)?,
            "response.output_text.delta" | "response.refusal.delta" => {
                let item_id = field(&value, "item_id")?;
                let index = self.existing(&item_id, BlockKind::Text)?;
                let delta = field(&value, "delta")?;
                if kind == "response.refusal.delta" {
                    self.refusal = true;
                }
                self.start(&mut out);
                out.push(ProviderEvent::TextDelta {
                    block_index: index,
                    text: delta,
                });
            }
            "response.reasoning_summary_text.delta" => {
                let item_id = field(&value, "item_id")?;
                let index = self.reasoning_block(&item_id)?;
                let delta = field(&value, "delta")?;
                self.start(&mut out);
                out.push(ProviderEvent::ReasoningSummaryDelta {
                    block_index: index,
                    text: delta,
                });
            }
            "response.function_call_arguments.delta" => {
                let item_id = field(&value, "item_id")?;
                let index = self.existing(&item_id, BlockKind::Tool)?;
                let delta = field(&value, "delta")?;
                let tool = self
                    .tools
                    .get_mut(&index)
                    .ok_or_else(|| malformed("a fragment arrived for an unknown tool call"))?;
                if tool.ready {
                    return Err(malformed("a fragment arrived for a ready tool call"));
                }
                tool.fragments.push_str(&delta);
                self.start(&mut out);
                out.push(ProviderEvent::ToolArgumentsDelta {
                    block_index: index,
                    fragment: delta,
                });
            }
            "response.completed" => {
                if let Some(usage) = usage_of(&value) {
                    out.push(ProviderEvent::UsageUpdated { usage });
                }
                if !self.tools.values().all(|tool| tool.ready) {
                    return Err(malformed("the response completed with an unfinished call"));
                }
                let reason = if self.tools.is_empty() {
                    if self.refusal {
                        FinishReason::Refusal
                    } else {
                        FinishReason::Stop
                    }
                } else {
                    FinishReason::ToolCalls
                };
                self.start(&mut out);
                out.push(finished_completed(reason));
                self.finished = true;
            }
            "response.incomplete" => {
                if let Some(usage) = usage_of(&value) {
                    out.push(ProviderEvent::UsageUpdated { usage });
                }
                let reason = value
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                // An incomplete response never promotes a partial call.
                let finish = match reason {
                    "max_output_tokens" => FinishReason::OutputLimit,
                    "content_filter" => FinishReason::Refusal,
                    _ => FinishReason::Stop,
                };
                self.start(&mut out);
                out.push(finished_completed(finish));
                self.finished = true;
            }
            "response.failed" => {
                if let Some(usage) = usage_of(&value) {
                    out.push(ProviderEvent::UsageUpdated { usage });
                }
                let error = provider_error(self.profile, value.pointer("/response/error"));
                out.push(finished_failed(error));
                self.finished = true;
            }
            "error" => {
                out.push(finished_failed(provider_error(self.profile, Some(&value))));
                self.finished = true;
            }
            other => {
                if IGNORED.contains(&other) {
                    self.counters.ignored_events.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.counters.unknown_events.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Ok(out)
    }

    fn start(&mut self, out: &mut Vec<ProviderEvent>) {
        if !self.started {
            self.started = true;
            out.push(ProviderEvent::Started);
        }
    }

    fn item_added(
        &mut self,
        value: &Value,
        out: &mut Vec<ProviderEvent>,
    ) -> Result<(), ModelError> {
        let item = value
            .get("item")
            .ok_or_else(|| malformed("an item event has no item"))?;
        let item_id = field(item, "id")?;
        match field(item, "type")?.as_str() {
            "message" => {
                self.insert_block(&item_id, BlockKind::Text)?;
            }
            "reasoning" => {
                // Registered lazily: a summary delta allocates its block, and a
                // completed item's encrypted content gets its own fresh index.
                self.blocks.remove(&item_id);
            }
            "function_call" => {
                let name = field(item, "name")?;
                let index = self.insert_block(&item_id, BlockKind::Tool)?;
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned);
                self.tools.insert(
                    index,
                    Tool {
                        fragments: String::new(),
                        ready: false,
                    },
                );
                self.start(out);
                out.push(ProviderEvent::ToolCallStarted {
                    block_index: index,
                    name,
                    provider_call_id: call_id,
                });
            }
            _ => {
                // Hosted tools and other output kinds have no normalized form.
                self.counters.unknown_events.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn item_done(&mut self, value: &Value, out: &mut Vec<ProviderEvent>) -> Result<(), ModelError> {
        let item = value
            .get("item")
            .ok_or_else(|| malformed("an item event has no item"))?;
        let item_id = field(item, "id")?;
        match field(item, "type")?.as_str() {
            "function_call" => {
                let index = self.existing(&item_id, BlockKind::Tool)?;
                let tool = self
                    .tools
                    .get_mut(&index)
                    .ok_or_else(|| malformed("a completed call was never started"))?;
                if tool.ready {
                    return Err(malformed("a tool call became ready twice"));
                }
                if !matches!(
                    serde_json::from_str::<Value>(&tool.fragments),
                    Ok(Value::Object(_))
                ) {
                    return Err(malformed("tool-call arguments are not one JSON object"));
                }
                tool.ready = true;
                self.start(out);
                out.push(ProviderEvent::ToolCallReady { block_index: index });
            }
            "reasoning" => {
                // The documented item to replay is the one from
                // `output_item.done`; in-progress items may be incomplete.
                let encrypted = item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| !content.is_empty());
                if !encrypted {
                    return Ok(());
                }
                let payload = serde_json::to_vec(item)
                    .map_err(|_| malformed("a reasoning item is not serializable"))?;
                if payload.len() > MAX_NATIVE_BYTES {
                    return Err(error(
                        ModelErrorCode::LimitExceeded,
                        "a native state item exceeds the 256 KiB bound",
                    ));
                }
                // A fresh index keeps the state block separate from the summary
                // block, as decision 12 requires.
                let index = self.allocate();
                self.start(out);
                out.push(ProviderEvent::ProviderState {
                    block_index: index,
                    schema: STATE_SCHEMA.to_owned(),
                    payload,
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn allocate(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index = self.next_index.saturating_add(1);
        index
    }

    fn insert_block(&mut self, item_id: &str, kind: BlockKind) -> Result<u32, ModelError> {
        if self.blocks.contains_key(item_id) {
            return Err(malformed("an output item was added twice"));
        }
        let index = self.allocate();
        self.blocks
            .insert(item_id.to_owned(), Block { index, kind });
        Ok(index)
    }

    fn reasoning_block(&mut self, item_id: &str) -> Result<u32, ModelError> {
        if let Some(block) = self.blocks.get(item_id) {
            return (block.kind == BlockKind::Reasoning)
                .then_some(block.index)
                .ok_or_else(|| malformed("a summary delta targeted a non-reasoning block"));
        }
        let index = self.allocate();
        self.blocks.insert(
            item_id.to_owned(),
            Block {
                index,
                kind: BlockKind::Reasoning,
            },
        );
        Ok(index)
    }

    fn existing(&self, item_id: &str, kind: BlockKind) -> Result<u32, ModelError> {
        let block = self
            .blocks
            .get(item_id)
            .ok_or_else(|| malformed("a delta arrived for an unknown output item"))?;
        if block.kind != kind {
            return Err(malformed("a delta arrived for the wrong block kind"));
        }
        Ok(block.index)
    }
}

fn field(value: &Value, name: &str) -> Result<String, ModelError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| malformed("an event is missing a required string field"))
}

/// Cumulative usage from a response object; absent details stay `None`.
fn usage_of(event: &Value) -> Option<Usage> {
    let usage = event.get("response")?.get("usage")?;
    if !usage.is_object() {
        return None;
    }
    Some(Usage {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        cached_input_tokens: usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64),
        reasoning_tokens: usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
    })
}

/// Structured failure from a `response.error` object or an `error` event.
fn provider_error(profile: Profile, value: Option<&Value>) -> ModelError {
    let code = value
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str);
    let message = value
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mapped = classify(code, message);
    ModelError {
        code: mapped,
        // The provider's own message may quote request content; use a fixed one.
        message: safe_message(mapped, profile),
        provider_code: code.and_then(safe_provider_code).map(str::to_owned),
        retry_after_ms: None,
    }
}

// Only protocol vocabulary may enter a journal; arbitrary codes can echo keys.
pub(super) fn safe_provider_code(code: &str) -> Option<&'static str> {
    const KNOWN: &[&str] = &[
        "context_length_exceeded",
        "rate_limit_exceeded",
        "insufficient_quota",
        "usage_limit_reached",
        "billing_hard_limit_reached",
        "invalid_api_key",
        "authentication_error",
        "token_expired",
        "invalid_request_error",
        "invalid_prompt",
        "invalid_value",
        "missing_required_parameter",
        "unknown_parameter",
        "model_not_found",
        "limit_exceeded",
        "server_error",
    ];
    KNOWN.iter().copied().find(|known| *known == code)
}

/// Provider error code to normalized category.
pub(crate) fn classify(code: Option<&str>, message: &str) -> ModelErrorCode {
    let code = code.unwrap_or_default();
    let message = message.to_ascii_lowercase();
    let context_length = code.contains("context_length")
        || message.contains("context length")
        || message.contains("maximum context");
    if context_length {
        return ModelErrorCode::ContextOverflow;
    }
    match code {
        "rate_limit_exceeded"
        | "insufficient_quota"
        | "usage_limit_reached"
        | "billing_hard_limit_reached" => ModelErrorCode::RateLimit,
        "invalid_api_key" | "authentication_error" | "token_expired" => {
            ModelErrorCode::Authentication
        }
        "invalid_request_error"
        | "invalid_prompt"
        | "invalid_value"
        | "missing_required_parameter"
        | "unknown_parameter"
        | "model_not_found" => ModelErrorCode::InvalidRequest,
        "limit_exceeded" => ModelErrorCode::LimitExceeded,
        _ => ModelErrorCode::ProviderFailure,
    }
}

/// Fixed diagnostic per category; provider text is never echoed.
pub(crate) fn safe_message(code: ModelErrorCode, profile: Profile) -> String {
    let base = match code {
        ModelErrorCode::Authentication => "provider authentication failed",
        ModelErrorCode::RateLimit => "provider rate limit reached; start a new turn",
        ModelErrorCode::ContextOverflow => "the request exceeded the model's context window",
        ModelErrorCode::InvalidRequest => "the provider rejected the request",
        ModelErrorCode::UnsupportedCapability => "the provider does not support the request",
        ModelErrorCode::Transport => "the connection to the provider failed",
        ModelErrorCode::MalformedStream => "the provider stream was malformed",
        ModelErrorCode::LimitExceeded => "a provider bound was exceeded",
        ModelErrorCode::ProviderFailure => "the provider failed the response",
    };
    match (profile, code) {
        (Profile::ChatGptCodex, ModelErrorCode::Authentication) => {
            format!("{base}; run `codex login`")
        }
        _ => base.to_owned(),
    }
}

pub(crate) fn malformed(message: &str) -> ModelError {
    error(ModelErrorCode::MalformedStream, message)
}

pub(crate) fn error(code: ModelErrorCode, message: &str) -> ModelError {
    ModelError {
        code,
        message: message.to_owned(),
        provider_code: None,
        retry_after_ms: None,
    }
}

pub(crate) fn finished_completed(reason: FinishReason) -> ProviderEvent {
    ProviderEvent::Finished {
        outcome: ProviderOutcome::Completed,
        finish_reason: Some(reason),
        error: None,
        // Stateless responses (decision 14) carry no server-side handle.
        continuation: None,
    }
}

pub(crate) fn finished_failed(error: ModelError) -> ProviderEvent {
    ProviderEvent::Finished {
        outcome: ProviderOutcome::Failed,
        finish_reason: None,
        error: Some(error),
        continuation: None,
    }
}

pub(crate) fn finished_cancelled() -> ProviderEvent {
    ProviderEvent::Finished {
        outcome: ProviderOutcome::Cancelled,
        finish_reason: None,
        error: None,
        continuation: None,
    }
}
