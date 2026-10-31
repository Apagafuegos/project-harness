//! Native Messages events. Raw thinking is private replay data, never display text.
use super::{Counters, Profile};
use crate::sse::SseEvent;
use harness_core::{
    model::{ProviderEvent, ProviderOutcome},
    session::{FinishReason, ModelError, ModelErrorCode, Usage},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::Ordering},
};

const MAX_NATIVE_BYTES: usize = 256 * 1024;
struct Block {
    value: Value,
    fragments: String,
    closed: bool,
}
/// Response-local state; native indices preserve content order.
pub(crate) struct Mapper {
    profile: Profile,
    started: bool,
    finished: bool,
    blocks: BTreeMap<u32, Block>,
    reason: Option<FinishReason>,
    usage: Usage,
    counters: Arc<Counters>,
}
impl Mapper {
    pub(crate) fn new(counters: Arc<Counters>, profile: Profile) -> Self {
        Self {
            profile,
            started: false,
            finished: false,
            blocks: BTreeMap::new(),
            reason: None,
            usage: Usage {
                input_tokens: None,
                output_tokens: None,
                cached_input_tokens: None,
                reasoning_tokens: None,
            },
            counters,
        }
    }
    pub(crate) fn finished(&self) -> bool {
        self.finished
    }
    pub(crate) fn finish_eof(self) -> Result<Vec<ProviderEvent>, ModelError> {
        if self.finished {
            Ok(vec![])
        } else {
            Err(malformed("the stream ended without message_stop"))
        }
    }
    pub(crate) fn push(&mut self, event: &SseEvent) -> Result<Vec<ProviderEvent>, ModelError> {
        let v: Value =
            serde_json::from_str(&event.data).map_err(|_| malformed("invalid event JSON"))?;
        let Some(kind) = v.get("type").and_then(Value::as_str) else {
            // DeepSeek documents a start-up timeout as an unnamed `data` frame
            // holding only `{"error":{...}}`: no SSE event name and no top-level
            // `type`. It is a coded failure, never a malformed-stream guess, a
            // hang, or a silent success.
            let error = v
                .get("error")
                .filter(|error| error.is_object())
                .ok_or_else(|| malformed("missing event type"))?;
            if event.event.is_some() || self.finished {
                return Err(malformed("invalid event lifecycle"));
            }
            self.finished = true;
            return Ok(vec![failed(provider_error(
                self.profile,
                Some(error),
                v.get("retry_after_ms").and_then(Value::as_u64),
            ))]);
        };
        if event.event.as_deref().is_some_and(|name| name != kind) || self.finished {
            return Err(malformed("invalid event lifecycle"));
        }
        let mut out = vec![];
        match kind {
            "ping" => {
                self.counters.ignored_events.fetch_add(1, Ordering::Relaxed);
            }
            "error" => {
                self.finished = true;
                out.push(failed(provider_error(
                    self.profile,
                    v.get("error"),
                    v.get("retry_after_ms").and_then(Value::as_u64),
                )));
            }
            "message_start" => {
                if self.started {
                    return Err(malformed("duplicate message_start"));
                }
                let msg = &v["message"];
                if msg["role"] != "assistant"
                    || !msg["content"].as_array().is_some_and(Vec::is_empty)
                {
                    return Err(malformed("invalid initial message"));
                }
                self.started = true;
                out.push(ProviderEvent::Started);
                self.update_usage(msg.get("usage"), &mut out)?;
            }
            "content_block_start" | "content_block_delta" | "content_block_stop" => {
                if !self.started || self.reason.is_some() {
                    return Err(malformed("content outside message"));
                }
                let index = v["index"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| malformed("invalid block index"))?;
                match kind {
                    "content_block_start" => self.start(index, &v["content_block"], &mut out)?,
                    "content_block_delta" => self.delta(index, &v["delta"], &mut out)?,
                    _ => self.stop(index, &mut out)?,
                }
            }
            "message_delta" => {
                if !self.started || self.blocks.values().any(|b| !b.closed) {
                    return Err(malformed("message_delta before blocks closed"));
                }
                if let Some(reason) = v["delta"].get("stop_reason").filter(|v| !v.is_null()) {
                    let tools = self
                        .blocks
                        .values()
                        .filter(|b| b.value["type"] == "tool_use")
                        .count();
                    self.reason = Some(match reason.as_str() {
                        Some("end_turn" | "stop_sequence") if tools == 0 => FinishReason::Stop,
                        Some("tool_use") if tools > 0 => FinishReason::ToolCalls,
                        Some("max_tokens") if tools == 0 => FinishReason::OutputLimit,
                        Some("refusal") if tools == 0 => FinishReason::Refusal,
                        Some("model_context_window_exceeded") => {
                            self.finished = true;
                            out.push(failed(error(
                                ModelErrorCode::ContextOverflow,
                                "model context window exceeded",
                            )));
                            return Ok(out);
                        }
                        // pause_turn requires a server-tool continuation, not a completed answer.
                        Some("pause_turn") => {
                            self.finished = true;
                            out.push(failed(error(
                                ModelErrorCode::UnsupportedCapability,
                                "pause_turn requires unsupported server-tool continuation",
                            )));
                            return Ok(out);
                        }
                        _ => return Err(malformed("unknown or inconsistent stop reason")),
                    });
                }
                self.update_usage(v.get("usage"), &mut out)?;
            }
            "message_stop" => {
                let reason = self
                    .reason
                    .ok_or_else(|| malformed("message_stop without stop reason"))?;
                if !self.started || self.blocks.values().any(|b| !b.closed) {
                    return Err(malformed("unfinished message"));
                }
                self.finished = true;
                out.push(ProviderEvent::Finished {
                    outcome: ProviderOutcome::Completed,
                    finish_reason: Some(reason),
                    error: None,
                    continuation: None,
                });
            }
            _ => {
                self.counters.unknown_events.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(out)
    }
    fn start(
        &mut self,
        index: u32,
        value: &Value,
        out: &mut Vec<ProviderEvent>,
    ) -> Result<(), ModelError> {
        if self.blocks.contains_key(&index) {
            return Err(malformed("duplicate content block"));
        }
        let kind = field(value, "type")?;
        match kind {
            "text" => out.push(ProviderEvent::TextDelta {
                block_index: index,
                text: field(value, "text")?.into(),
            }),
            "thinking" => {
                field(value, "thinking")?;
                // Anthropic requires a signature field here and fills it with a
                // delta; DeepSeek documents thinking blocks without one, so a
                // present signature must be a string but a missing one is legal.
                if self.profile.requires_thinking_signature() {
                    field(value, "signature")?;
                } else if let Some(signature) = value.get("signature") {
                    signature
                        .as_str()
                        .ok_or_else(|| malformed("invalid thinking signature"))?;
                }
            }
            "redacted_thinking" if self.profile.supports_redacted_thinking() => {
                field(value, "data")?;
            }
            "redacted_thinking" => {
                // Not part of this profile's vocabulary: never store state the
                // profile would refuse to replay.
                self.counters.unknown_events.fetch_add(1, Ordering::Relaxed);
            }
            "tool_use" => {
                let name = field(value, "name")?;
                let id = field(value, "id")?;
                if name.is_empty() || id.is_empty() || !value["input"].is_object() {
                    return Err(malformed("invalid tool start"));
                }
                out.push(ProviderEvent::ToolCallStarted {
                    block_index: index,
                    name: name.into(),
                    provider_call_id: Some(id.into()),
                });
            }
            _ => {
                self.counters.unknown_events.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.blocks.insert(
            index,
            Block {
                value: value.clone(),
                fragments: String::new(),
                closed: false,
            },
        );
        Ok(())
    }
    fn delta(
        &mut self,
        index: u32,
        delta: &Value,
        out: &mut Vec<ProviderEvent>,
    ) -> Result<(), ModelError> {
        let b = self
            .blocks
            .get_mut(&index)
            .filter(|b| !b.closed)
            .ok_or_else(|| malformed("delta for unknown or closed block"))?;
        let kind = field(delta, "type")?;
        match (kind, b.value["type"].as_str()) {
            ("text_delta", Some("text")) => out.push(ProviderEvent::TextDelta {
                block_index: index,
                text: field(delta, "text")?.into(),
            }),
            ("thinking_delta", Some("thinking")) => {
                // Raw thought is replay data, never display text: it is kept for
                // the signed block and counted as deliberately ignored.
                append(&mut b.value, "thinking", field(delta, "thinking")?)?;
                self.counters.ignored_events.fetch_add(1, Ordering::Relaxed);
            }
            ("signature_delta", Some("thinking")) => {
                append(&mut b.value, "signature", field(delta, "signature")?)?
            }
            ("input_json_delta", Some("tool_use")) => {
                let fragment = field(delta, "partial_json")?;
                b.fragments.push_str(fragment);
                out.push(ProviderEvent::ToolArgumentsDelta {
                    block_index: index,
                    fragment: fragment.into(),
                });
            }
            ("text_delta" | "thinking_delta" | "signature_delta" | "input_json_delta", _) => {
                return Err(malformed("delta block kind mismatch"));
            }
            // Citations, server-tool deltas and future deltas have no normalized form.
            _ => {
                self.counters.unknown_events.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }
    fn stop(&mut self, index: u32, out: &mut Vec<ProviderEvent>) -> Result<(), ModelError> {
        let b = self
            .blocks
            .get_mut(&index)
            .filter(|b| !b.closed)
            .ok_or_else(|| malformed("stop for unknown or closed block"))?;
        b.closed = true;
        match b.value["type"].as_str() {
            Some("tool_use") => {
                if b.fragments.is_empty() {
                    b.fragments = b.value["input"].to_string();
                    out.push(ProviderEvent::ToolArgumentsDelta {
                        block_index: index,
                        fragment: b.fragments.clone(),
                    });
                }
                if !matches!(
                    serde_json::from_str::<Value>(&b.fragments),
                    Ok(Value::Object(_))
                ) {
                    return Err(malformed("tool-call arguments are not one JSON object"));
                }
                out.push(ProviderEvent::ToolCallReady { block_index: index });
            }
            Some("thinking") => {
                if self.profile.requires_thinking_signature()
                    && field(&b.value, "signature")?.is_empty()
                {
                    return Err(malformed("thinking block has no signature"));
                }
                let value = b.value.clone();
                native_state(self.profile.state_schema(), index, value, out)?;
            }
            // State the profile does not document is never stored: it could not
            // be replayed either, so an empty entry would only cost a turn.
            Some("redacted_thinking") if self.profile.supports_redacted_thinking() => {
                let value = b.value.clone();
                native_state(self.profile.state_schema(), index, value, out)?;
            }
            _ => {}
        }
        Ok(())
    }
    fn update_usage(
        &mut self,
        value: Option<&Value>,
        out: &mut Vec<ProviderEvent>,
    ) -> Result<(), ModelError> {
        let Some(v) = value.filter(|v| !v.is_null()) else {
            return Ok(());
        };
        if !v.is_object() {
            return Err(malformed("invalid usage"));
        }
        // message_delta usage is a partial cumulative snapshot, not a new increment.
        for (key, target) in [
            ("input_tokens", &mut self.usage.input_tokens),
            ("output_tokens", &mut self.usage.output_tokens),
            (
                "cache_read_input_tokens",
                &mut self.usage.cached_input_tokens,
            ),
        ] {
            if let Some(n) = v.get(key) {
                *target = if n.is_null() {
                    None
                } else {
                    Some(n.as_u64().ok_or_else(|| malformed("invalid usage count"))?)
                };
            }
        }
        out.push(ProviderEvent::UsageUpdated {
            usage: self.usage.clone(),
        });
        Ok(())
    }
}
/// Emit one completed native block as owner-scoped replay state.
fn native_state(
    schema: &str,
    index: u32,
    value: Value,
    out: &mut Vec<ProviderEvent>,
) -> Result<(), ModelError> {
    let payload = serde_json::to_vec(&value).map_err(|_| malformed("invalid native block"))?;
    if payload.len() > MAX_NATIVE_BYTES {
        return Err(error(
            ModelErrorCode::LimitExceeded,
            "native thinking exceeds 256 KiB",
        ));
    }
    out.push(ProviderEvent::ProviderState {
        block_index: index,
        schema: schema.into(),
        payload,
    });
    Ok(())
}
fn append(value: &mut Value, key: &str, delta: &str) -> Result<(), ModelError> {
    let mut s = field(value, key)?.to_owned();
    s.push_str(delta);
    value[key] = json!(s);
    Ok(())
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str, ModelError> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| malformed("missing event string"))
}
pub(crate) fn error(code: ModelErrorCode, message: &str) -> ModelError {
    ModelError {
        code,
        message: message.into(),
        provider_code: None,
        retry_after_ms: None,
    }
}
pub(crate) fn malformed(message: &str) -> ModelError {
    error(ModelErrorCode::MalformedStream, message)
}
pub(crate) fn failed(error: ModelError) -> ProviderEvent {
    ProviderEvent::Finished {
        outcome: ProviderOutcome::Failed,
        finish_reason: None,
        error: Some(error),
        continuation: None,
    }
}
pub(crate) fn provider_error(
    profile: Profile,
    value: Option<&Value>,
    retry: Option<u64>,
) -> ModelError {
    let v = value.unwrap_or(&Value::Null);
    let native = v["type"].as_str().unwrap_or("");
    let message = v["message"].as_str().unwrap_or("").to_ascii_lowercase();
    let code = match native {
        "authentication_error" | "permission_error" => ModelErrorCode::Authentication,
        "rate_limit_error" => ModelErrorCode::RateLimit,
        "invalid_request_error" | "request_too_large" => {
            if message.contains("context")
                || message.contains("prompt is too long")
                || message.contains("too many tokens")
            {
                ModelErrorCode::ContextOverflow
            } else {
                ModelErrorCode::InvalidRequest
            }
        }
        _ => ModelErrorCode::ProviderFailure,
    };
    // Never copy arbitrary provider strings: even an unknown error type can echo a key.
    let known = [
        "authentication_error",
        "permission_error",
        "rate_limit_error",
        "invalid_request_error",
        "request_too_large",
        "overloaded_error",
        "api_error",
        "not_found_error",
    ];
    ModelError {
        provider_code: known.contains(&native).then(|| native.into()),
        retry_after_ms: retry
            .or_else(|| v["retry_after_ms"].as_u64())
            .or_else(|| v["error"]["retry_after_ms"].as_u64()),
        ..error(code, profile.error_message())
    }
}
