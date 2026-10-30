//! Pure translation from `ModelInput` to one Messages request body.
//!
//! Deterministic: no I/O, no clock, no credential. Artifact bytes (images and
//! replayed thinking blocks) are read beforehand by [`collect`].

use base64::{Engine as _, engine::general_purpose::STANDARD};
use harness_core::{
    context::{ContextItem, InputMediaKind},
    model::{ArtifactSource, ModelInput, ReasoningEffort},
    session::{
        ArtifactRef, ContentBlock, ModelError, ModelErrorCode, Provenance, ToolResult, ToolStatus,
    },
    tools::ToolDefinition,
};
use serde_json::{Map, Value, json};

use super::{STATE_SCHEMA, models, preserved};
use crate::native_id::native_call_id;

/// Artifact bytes permitted for this request, keyed by exact reference.
#[derive(Default)]
pub(crate) struct Reads {
    entries: Vec<(ArtifactRef, Vec<u8>)>,
}

impl Reads {
    fn get(&self, reference: &ArtifactRef) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|(known, _)| known == reference)
            .map(|(_, bytes)| bytes.as_slice())
    }
}

#[cfg(test)]
impl Reads {
    /// Test-only: artifact bytes already in hand, no storage read.
    pub(crate) fn from_bytes(reference: ArtifactRef, bytes: Vec<u8>) -> Self {
        Self {
            entries: vec![(reference, bytes)],
        }
    }

    /// Test-only: add one more already-read artifact.
    pub(crate) fn with(mut self, reference: ArtifactRef, bytes: Vec<u8>) -> Self {
        self.entries.push((reference, bytes));
        self
    }
}

/// Read exactly the artifacts request building consumes: user images and
/// same-owner thinking state this adapter wrote. Nothing else is fetched.
pub(crate) async fn collect(
    input: &ModelInput,
    artifacts: &ArtifactSource,
) -> Result<Reads, ModelError> {
    let mut reads = Reads::default();
    let mut wanted: Vec<ArtifactRef> = Vec::new();
    for item in &input.items {
        let content = match item {
            ContextItem::User { content, .. } | ContextItem::Assistant { content, .. } => content,
            _ => continue,
        };
        for block in content {
            let reference = match block {
                ContentBlock::Image { artifact, .. } => artifact,
                ContentBlock::ProviderState {
                    owner,
                    schema,
                    artifact,
                } if owner == &input.model && schema == STATE_SCHEMA => artifact,
                _ => continue,
            };
            if !wanted.contains(reference) {
                wanted.push(reference.clone());
            }
        }
    }
    for reference in wanted {
        let bytes = artifacts
            .read(&reference)
            .await
            .map_err(|_| invalid("a referenced artifact could not be read for the request"))?;
        reads.entries.push((reference, bytes));
    }
    Ok(reads)
}

/// One outbound message. Content blocks are appended in projection order.
struct Message {
    role: &'static str,
    content: Vec<Value>,
}

impl Message {
    fn push(role: &'static str, blocks: impl IntoIterator<Item = Value>, messages: &mut Vec<Self>) {
        let blocks: Vec<Value> = blocks.into_iter().collect();
        if blocks.is_empty() {
            return;
        }
        // The API combines consecutive same-role turns; do it here so the
        // submitted body is already strictly alternating and content order is
        // preserved across item boundaries.
        match messages.last_mut() {
            Some(last) if last.role == role => last.content.extend(blocks),
            _ => messages.push(Self {
                role,
                content: blocks,
            }),
        }
    }
}

/// Build the JSON body for one Messages request.
pub(crate) fn build(input: &ModelInput, reads: &Reads) -> Result<Value, ModelError> {
    let capabilities = models::capabilities(&input.model.model);
    if !input.tools.is_empty() && !capabilities.tools {
        return Err(unsupported("the model does not accept tool definitions"));
    }
    if input.options.reasoning_effort.is_some() && !capabilities.reasoning_effort {
        return Err(unsupported(
            "reasoning_effort is not supported by this model",
        ));
    }
    if input.options.max_output_tokens == 0 {
        return Err(invalid("max_output_tokens must be positive"));
    }
    let options = provider_options(input)?;
    let mut messages = Vec::new();
    let mut calls = Vec::new();
    for item in &input.items {
        match item {
            ContextItem::Instructions { .. } => {}
            ContextItem::User { content, .. } => {
                let blocks = user_blocks(content, &capabilities, reads)?;
                Message::push("user", blocks, &mut messages);
            }
            ContextItem::Assistant { content, .. } => {
                let blocks = assistant_blocks(content, input, reads, &mut calls)?;
                Message::push("assistant", blocks, &mut messages);
            }
            ContextItem::ToolResult {
                provenance, result, ..
            } => {
                let block = tool_result(*provenance, result, &calls)?;
                Message::push("user", [block], &mut messages);
            }
            ContextItem::Summary { summary, .. } => Message::push(
                "user",
                [json!({
                    "type": "text",
                    "text": format!("Summary of earlier conversation:\n\n{summary}"),
                })],
                &mut messages,
            ),
        }
    }
    if messages.is_empty() {
        return Err(invalid("a Messages request needs at least one message"));
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(input.model.model));
    body.insert("max_tokens".into(), json!(input.options.max_output_tokens));
    body.insert(
        "messages".into(),
        Value::Array(
            messages
                .into_iter()
                .map(|message| json!({"role": message.role, "content": message.content}))
                .collect(),
        ),
    );
    body.insert("stream".into(), json!(true));
    if let Some(system) = instruction_text(input) {
        body.insert("system".into(), json!(system));
    }
    if !input.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(input.tools.iter().map(api_tool).collect()),
        );
    }
    if let Some(effort) = input.options.reasoning_effort {
        // Current models use adaptive thinking; depth is `output_config.effort`.
        // `type: "enabled"` with `budget_tokens` is rejected on the 4.7+ family,
        // and `type: "disabled"` is rejected by the always-on models, so the
        // adapter only ever asks for adaptive thinking.
        body.insert("thinking".into(), json!({"type": "adaptive"}));
        let effort = match effort {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        };
        body.insert("output_config".into(), json!({"effort": effort}));
    }
    if let Some(tier) = options.service_tier {
        body.insert("service_tier".into(), json!(tier));
    }
    let mut body = Value::Object(body);
    if replays_thinking(&body) {
        // Preserved thinking binds a block to the exact prefix that produced it
        // (system, tools, and every earlier message). Compaction replaces
        // earlier turns with a summary and the kept turns keep their thinking,
        // so replaying those blocks unchanged is a 400 for accounts the API
        // enforces this on. Ask the documented control to drop a failing block
        // instead. `block_binding` rides inside `thinking`, so an adaptive
        // configuration is sent with it; the beta header `preserved::BETA` must
        // travel with this body, and `mod.rs` adds it from the same predicate.
        let Some(object) = body.as_object_mut() else {
            return Ok(body);
        };
        let mut thinking = object
            .get("thinking")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(|| {
                let mut thinking = Map::new();
                thinking.insert("type".into(), json!("adaptive"));
                thinking
            });
        thinking.insert("block_binding".into(), preserved::drop_block());
        object.insert("thinking".into(), Value::Object(thinking));
    }
    Ok(body)
}

/// Whether this body sends a thinking block back to the API.
///
/// One predicate chooses both the `block_binding` control in the body and the
/// beta header that makes it legal, so the two can never disagree.
pub(crate) fn replays_thinking(body: &Value) -> bool {
    body.get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| {
                        blocks.iter().any(|block| {
                            matches!(
                                block.get("type").and_then(Value::as_str),
                                Some("thinking" | "redacted_thinking")
                            )
                        })
                    })
            })
        })
}

/// Validated `anthropic.*` provider options.
#[derive(Default)]
struct Options {
    service_tier: Option<String>,
}

const SERVICE_TIERS: [&str; 2] = ["auto", "standard_only"];

fn provider_options(input: &ModelInput) -> Result<Options, ModelError> {
    let mut options = Options::default();
    for (namespace, value) in &input.options.provider_options {
        if namespace != "anthropic" {
            return Err(unsupported(&format!(
                "provider option namespace {namespace} is not supported by the Anthropic adapter"
            )));
        }
        let Some(object) = value.as_object() else {
            return Err(invalid("anthropic provider options must be a JSON object"));
        };
        for (key, value) in object {
            match key.as_str() {
                "service_tier" => {
                    let tier = value
                        .as_str()
                        .filter(|tier| SERVICE_TIERS.contains(tier))
                        .ok_or_else(|| {
                            invalid("anthropic.service_tier must be auto or standard_only")
                        })?;
                    options.service_tier = Some(tier.to_owned());
                }
                other => {
                    return Err(invalid(&format!(
                        "unknown anthropic provider option {other}"
                    )));
                }
            }
        }
    }
    Ok(options)
}

/// Instruction texts in projection order, joined. The Messages API has one
/// `system` field, so system and developer levels keep their order and text but
/// cannot keep distinct roles; the seam passes no richer precedence.
fn instruction_text(input: &ModelInput) -> Option<String> {
    let mut parts = Vec::new();
    for item in &input.items {
        if let ContextItem::Instructions { instructions } = item {
            for instruction in instructions {
                if !instruction.text.is_empty() {
                    parts.push(instruction.text.clone());
                }
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn user_blocks(
    content: &[ContentBlock],
    capabilities: &harness_core::context::ModelCapabilities,
    reads: &Reads,
) -> Result<Vec<Value>, ModelError> {
    let mut blocks = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text { text } if !text.is_empty() => {
                blocks.push(json!({"type": "text", "text": text}));
            }
            ContentBlock::Text { .. } => {}
            ContentBlock::Image {
                artifact,
                mime_type,
            } => {
                if !capabilities.input_media.contains(&InputMediaKind::Image) {
                    return Err(unsupported("the model does not accept image input"));
                }
                let bytes = reads
                    .get(artifact)
                    .ok_or_else(|| invalid("image artifact bytes are unavailable"))?;
                blocks.push(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": mime_type,
                        "data": STANDARD.encode(bytes),
                    },
                }));
            }
            ContentBlock::Audio { .. } => {
                return Err(unsupported("the model does not accept audio input"));
            }
            ContentBlock::File { .. } => {
                return Err(unsupported("the model does not accept file input"));
            }
            _ => return Err(invalid("a user item holds a block only assistants produce")),
        }
    }
    Ok(blocks)
}

/// Assistant content becomes text, `tool_use` blocks and replayed thinking.
fn assistant_blocks(
    content: &[ContentBlock],
    input: &ModelInput,
    reads: &Reads,
    calls: &mut Vec<(String, String)>,
) -> Result<Vec<Value>, ModelError> {
    let mut blocks = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text { text } if !text.is_empty() => {
                blocks.push(json!({"type": "text", "text": text}));
            }
            ContentBlock::Text { .. } => {}
            ContentBlock::ToolCall {
                call_id,
                name,
                arguments,
                provider_call_id,
            } => {
                // A native id from another provider is sent verbatim when it is
                // a plain identifier, otherwise replaced deterministically; see
                // `native_id`. The same id is looked up for the result below.
                let native = native_call_id(provider_call_id.as_deref(), call_id.as_str());
                calls.push((call_id.as_str().to_owned(), native.clone()));
                blocks.push(json!({
                    "type": "tool_use",
                    "id": native,
                    "name": name,
                    "input": Value::Object(arguments.clone()),
                }));
            }
            ContentBlock::ProviderState {
                owner,
                schema,
                artifact,
            } => {
                // The core already scopes state to the request's owner; a
                // foreign owner is skipped, never recovered.
                if owner != &input.model {
                    continue;
                }
                if schema != STATE_SCHEMA {
                    return Err(unsupported(&format!(
                        "native state schema {schema} is not supported by the Anthropic adapter"
                    )));
                }
                let bytes = reads
                    .get(artifact)
                    .ok_or_else(|| invalid("native state bytes are unavailable"))?;
                let value: Value = serde_json::from_slice(bytes)
                    .map_err(|_| invalid("native state is not a JSON content block"))?;
                let kind = value.get("type").and_then(Value::as_str);
                // Exact native shape: `thinking` keeps its signature and
                // `redacted_thinking` keeps its opaque `data`. Anything else is
                // not a block this adapter wrote.
                let valid = match kind {
                    Some("thinking") => {
                        value.get("thinking").is_some_and(Value::is_string)
                            && value.get("signature").is_some_and(Value::is_string)
                    }
                    Some("redacted_thinking") => value.get("data").is_some_and(Value::is_string),
                    _ => false,
                };
                if !valid {
                    return Err(invalid("native thinking state is not a replayable block"));
                }
                blocks.push(value);
            }
            // Reasoning summaries are display-only; replay uses the opaque
            // signed block so raw or summarized thought is never resent.
            ContentBlock::ReasoningSummary { .. } => {}
            _ => return Err(invalid("an assistant item holds unsupported content")),
        }
    }
    Ok(blocks)
}

fn api_tool(tool: &ToolDefinition) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.input_schema,
    })
}

fn tool_result(
    provenance: Provenance,
    result: &ToolResult,
    calls: &[(String, String)],
) -> Result<Value, ModelError> {
    let call_id = result.call.call_id.as_str();
    let native = calls
        .iter()
        .find(|(harness, _)| harness == call_id)
        .map(|(_, native)| native.clone())
        .ok_or_else(|| invalid("a tool result has no matching tool call"))?;
    let failed = !matches!(result.status, ToolStatus::Succeeded);
    Ok(json!({
        "type": "tool_result",
        "tool_use_id": native,
        "content": tool_output_text(provenance, result),
        "is_error": failed,
    }))
}

/// Deterministic plain-text rendering of a stored result. Provenance is
/// explicit so the model can tell a user's statement from real tool output.
fn tool_output_text(provenance: Provenance, result: &ToolResult) -> String {
    let status = match result.status {
        ToolStatus::Succeeded => "succeeded",
        ToolStatus::Failed => "failed",
        ToolStatus::InvalidArguments => "invalid_arguments",
        ToolStatus::Denied => "denied",
        ToolStatus::TimedOut => "timed_out",
        ToolStatus::Cancelled => "cancelled",
        ToolStatus::InfrastructureFailure => "infrastructure_failure",
        ToolStatus::Uncertain => "uncertain",
    };
    let source = match provenance {
        Provenance::Executor => "executor",
        Provenance::Runtime => "runtime",
        Provenance::UserObserved => "user_observed",
        Provenance::UserAcknowledgedUnknown => "user_acknowledged_unknown",
    };
    let exit = result
        .metadata
        .exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "none".to_owned());
    let mut text = String::new();
    for block in &result.content {
        match block {
            ContentBlock::Text { text: body } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(body);
            }
            ContentBlock::Image { mime_type, .. }
            | ContentBlock::Audio { mime_type, .. }
            | ContentBlock::File { mime_type, .. } => {
                text.push_str(&format!("\n[artifact: {mime_type}]"));
            }
            _ => {}
        }
    }
    format!(
        "status: {status}\nprovenance: {source}\nexit_code: {exit}\noutput_truncated: {}\n\n{text}",
        result.metadata.output_truncated
    )
}

pub(crate) fn invalid(message: &str) -> ModelError {
    error(ModelErrorCode::InvalidRequest, message)
}

pub(crate) fn unsupported(message: &str) -> ModelError {
    error(ModelErrorCode::UnsupportedCapability, message)
}

fn error(code: ModelErrorCode, message: &str) -> ModelError {
    ModelError {
        code,
        message: message.to_owned(),
        provider_code: None,
        retry_after_ms: None,
    }
}
