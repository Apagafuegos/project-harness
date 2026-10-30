//! Pure translation from the logical `ModelInput` to a Responses request body.
//!
//! Everything here is deterministic: no I/O, no clock, no credentials. Artifact
//! bytes (images and replayed reasoning) are read beforehand by [`collect`].

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use harness_core::{
    context::{ContextItem, InputMediaKind},
    model::{ArtifactSource, ModelInput, ReasoningEffort},
    session::{
        ArtifactRef, ContentBlock, ModelError, ModelErrorCode, Provenance, ToolResult, ToolStatus,
    },
    tools::ToolDefinition,
};
use serde_json::{Map, Value, json};

use super::{Profile, STATE_SCHEMA, models};

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
}

/// Read exactly the artifacts request building consumes: user images and
/// same-owner native state this adapter wrote. Nothing else is fetched.
pub(crate) async fn collect(
    input: &ModelInput,
    artifacts: &ArtifactSource,
) -> Result<Reads, ModelError> {
    let mut reads = Reads::default();
    let mut wanted: Vec<ArtifactRef> = Vec::new();
    for item in &input.items {
        let (ContextItem::User { content, .. } | ContextItem::Assistant { content, .. }) = item
        else {
            continue;
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

/// Build the JSON body for one Responses request.
pub(crate) fn build(
    input: &ModelInput,
    profile: Profile,
    reads: &Reads,
) -> Result<Value, ModelError> {
    let capabilities = models::capabilities(profile, &input.model.model);
    if !input.tools.is_empty() && !capabilities.tools {
        return Err(unsupported("the model does not accept tool definitions"));
    }
    if input.options.reasoning_effort.is_some() && !capabilities.reasoning_effort {
        return Err(unsupported(
            "reasoning_effort is not supported by this model",
        ));
    }
    let options = provider_options(input)?;
    if options.summary.is_some() && !capabilities.reasoning_effort {
        return Err(unsupported(
            "openai.reasoning_summary is not supported by this model",
        ));
    }
    let instructions = instruction_text(input);
    let mut items = Vec::new();
    if profile.instructions_as_input()
        && let Some(text) = &instructions
    {
        items.push(json!({
            "type": "message",
            "role": "developer",
            "content": [{"type": "input_text", "text": text}],
        }));
    }
    let calls = provider_call_ids(input);
    for item in &input.items {
        match item {
            ContextItem::Instructions { .. } => {}
            ContextItem::User { content, .. } => {
                if let Some(message) = user_message(content, &capabilities, reads)? {
                    items.push(message);
                }
            }
            ContextItem::Assistant { content, .. } => {
                assistant_items(content, input, reads, &mut items)?;
            }
            ContextItem::ToolResult {
                provenance, result, ..
            } => items.push(function_call_output(*provenance, result, &calls)?),
            ContextItem::Summary { summary, .. } => items.push(json!({
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": format!("Summary of earlier conversation:\n\n{summary}"),
                }],
            })),
        }
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(input.model.model));
    body.insert("input".into(), Value::Array(items));
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));
    body.insert(
        "include".into(),
        json!(["reasoning.encrypted_content".to_string()]),
    );
    // The subscription backend selects the output budget: the Codex client has
    // no such request field, so only the platform profile sends one.
    if profile.sends_max_output_tokens() {
        body.insert(
            "max_output_tokens".into(),
            json!(input.options.max_output_tokens),
        );
    }
    if let Some(text) = instructions
        && !profile.instructions_as_input()
    {
        body.insert("instructions".into(), json!(text));
    }
    if !input.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(input.tools.iter().map(function_tool).collect()),
        );
    }
    if profile.always_sends_tool_choice() || !input.tools.is_empty() {
        body.insert("tool_choice".into(), json!("auto"));
    }
    if profile.sends_parallel_tool_calls() {
        // The harness executes calls sequentially, but the model may propose
        // several in one response; the core handles interleaved calls.
        body.insert("parallel_tool_calls".into(), json!(true));
    }
    if capabilities.reasoning_effort
        && (input.options.reasoning_effort.is_some() || options.summary.is_some())
    {
        let mut reasoning = Map::new();
        if let Some(effort) = input.options.reasoning_effort {
            reasoning.insert(
                "effort".into(),
                json!(match effort {
                    ReasoningEffort::Low => "low",
                    ReasoningEffort::Medium => "medium",
                    ReasoningEffort::High => "high",
                }),
            );
        }
        if let Some(summary) = options.summary {
            reasoning.insert("summary".into(), json!(summary));
        }
        body.insert("reasoning".into(), Value::Object(reasoning));
    }
    if let Some(key) = options.prompt_cache_key {
        body.insert("prompt_cache_key".into(), json!(key));
    }
    if let Some(tier) = options.service_tier {
        body.insert("service_tier".into(), json!(tier));
    }
    Ok(Value::Object(body))
}

/// Validated `openai.*` provider options.
#[derive(Default)]
struct Options {
    summary: Option<String>,
    prompt_cache_key: Option<String>,
    service_tier: Option<String>,
}

const SUMMARY_MODES: [&str; 3] = ["auto", "concise", "detailed"];

fn provider_options(input: &ModelInput) -> Result<Options, ModelError> {
    let mut options = Options::default();
    for (namespace, value) in &input.options.provider_options {
        if namespace != "openai" {
            return Err(unsupported(&format!(
                "provider option namespace {namespace} is not supported by the OpenAI adapter"
            )));
        }
        let Some(object) = value.as_object() else {
            return Err(invalid("openai provider options must be a JSON object"));
        };
        for (key, value) in object {
            match key.as_str() {
                "reasoning_summary" => {
                    let mode = text_option(key, value)?;
                    if !SUMMARY_MODES.contains(&mode.as_str()) {
                        return Err(invalid(
                            "openai.reasoning_summary must be auto, concise or detailed",
                        ));
                    }
                    options.summary = Some(mode);
                }
                "prompt_cache_key" => options.prompt_cache_key = Some(text_option(key, value)?),
                "service_tier" => options.service_tier = Some(text_option(key, value)?),
                other => {
                    return Err(invalid(&format!("unknown openai provider option {other}")));
                }
            }
        }
    }
    Ok(options)
}

fn text_option(key: &str, value: &Value) -> Result<String, ModelError> {
    value
        .as_str()
        .filter(|text| !text.is_empty() && text.len() <= 256)
        .map(str::to_owned)
        .ok_or_else(|| invalid(&format!("openai.{key} must be a nonempty short string")))
}

/// Instruction texts in projection order, as one block. `None` when absent.
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

fn user_message(
    content: &[ContentBlock],
    capabilities: &harness_core::context::ModelCapabilities,
    reads: &Reads,
) -> Result<Option<Value>, ModelError> {
    let mut parts = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text { text } => {
                parts.push(json!({"type": "input_text", "text": text}));
            }
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
                parts.push(json!({
                    "type": "input_image",
                    "image_url": format!("data:{mime_type};base64,{}", STANDARD.encode(bytes)),
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
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(json!({
        "type": "message",
        "role": "user",
        "content": parts,
    })))
}

/// Assistant content becomes messages, `function_call` items and replayed state.
fn assistant_items(
    content: &[ContentBlock],
    input: &ModelInput,
    reads: &Reads,
    items: &mut Vec<Value>,
) -> Result<(), ModelError> {
    let mut text = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text { text: body } => {
                text.push(json!({"type": "output_text", "text": body}));
            }
            ContentBlock::ToolCall {
                call_id,
                name,
                arguments,
                provider_call_id,
            } => {
                flush_text(&mut text, items);
                let call_id = provider_call_id.as_deref().unwrap_or(call_id.as_str());
                items.push(json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": Value::Object(arguments.clone()).to_string(),
                }));
            }
            ContentBlock::ProviderState {
                owner,
                schema,
                artifact,
            } => {
                // The core already scopes state to the request's owner; a
                // foreign owner is simply not replayed, never recovered.
                if owner != &input.model {
                    continue;
                }
                if schema != STATE_SCHEMA {
                    return Err(unsupported(&format!(
                        "native state schema {schema} is not supported by the OpenAI adapter"
                    )));
                }
                let bytes = reads
                    .get(artifact)
                    .ok_or_else(|| invalid("native state bytes are unavailable"))?;
                let item: Value = serde_json::from_slice(bytes)
                    .map_err(|_| invalid("native state is not a JSON item"))?;
                flush_text(&mut text, items);
                items.push(item);
            }
            // Reasoning summaries are display-only; reasoning replay uses the
            // opaque state block so raw or summarized thought is never resent.
            ContentBlock::ReasoningSummary { .. } => {}
            _ => return Err(invalid("an assistant item holds unsupported content")),
        }
    }
    flush_text(&mut text, items);
    Ok(())
}

fn flush_text(text: &mut Vec<Value>, items: &mut Vec<Value>) {
    if text.is_empty() {
        return;
    }
    items.push(json!({
        "type": "message",
        "role": "assistant",
        "content": std::mem::take(text),
    }));
}

/// Harness call id to the native id the model issued, for result pairing.
fn provider_call_ids(input: &ModelInput) -> Vec<(String, String)> {
    let mut calls = Vec::new();
    for item in &input.items {
        if let ContextItem::Assistant { content, .. } = item {
            for block in content {
                if let ContentBlock::ToolCall {
                    call_id,
                    provider_call_id,
                    ..
                } = block
                {
                    let native = provider_call_id
                        .clone()
                        .unwrap_or_else(|| call_id.as_str().to_owned());
                    calls.push((call_id.as_str().to_owned(), native));
                }
            }
        }
    }
    calls
}

fn function_tool(tool: &ToolDefinition) -> Value {
    // `strict` is deliberately omitted: strict mode requires every property to
    // be required and `additionalProperties: false`, which the harness's own
    // schemas do not guarantee. The tool runtime validates arguments against
    // the same schema after the call is ready.
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.input_schema,
    })
}

fn function_call_output(
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
    Ok(json!({
        "type": "function_call_output",
        "call_id": native,
        "output": tool_output_text(provenance, result),
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
