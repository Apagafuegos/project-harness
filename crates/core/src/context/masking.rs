//! Deterministic, model-free observation masking (ADR 0026).
//!
//! The strategy renders the covered span exactly as the session model saw it,
//! keeps user messages, assistant text and a one-line form of every tool call,
//! and replaces each tool result body with a placeholder naming the tool, the
//! status, the body size and the artifact digest when there is one. The same
//! narrowed source always renders to byte-identical text, and the strategy has
//! no provider capability, so generation opens no stream.
use std::{collections::HashMap, future::Future, pin::Pin};

use super::{compaction::*, *};
use crate::{model::*, session::*};

/// Longest single-line form of one tool call's arguments, in UTF-8 bytes.
pub const ARGUMENT_BYTES: usize = 512;

/// Masking of tool result bodies with no model call.
#[derive(Clone, Copy, Debug, Default)]
pub struct ObservationMasking;

impl ObservationMasking {
    /// Stable journal strategy name.
    pub const NAME: &'static str = StrategyId::ObservationMasking.as_str();
    /// Rendering and failure policy version.
    pub const VERSION: &'static str = StrategyId::ObservationMasking.version();

    fn generate(
        &self,
        input: CompactionInput,
        _control: OperationControl,
    ) -> Result<CompactionPreview, CompactionError> {
        let mut prepared = prepare(&input)?;
        let config = prepared.config.clone();
        let masked = masked_span(&prepared.source().items);
        // Bounded output: an over-budget masking is an explicit failure, never a
        // silent truncation. Only individual tool-call argument lines are
        // bounded, with an explicit elision marker and the omitted byte count.
        if masked.trim().is_empty() {
            return Err(CompactionError::Invalid("empty_summary"));
        }
        if input.context.estimator.text(&masked).tokens > u64::from(config.max_summary_tokens) {
            return Err(CompactionError::Invalid("masked_span_too_long"));
        }
        assemble(input, prepared, masked, None)
    }
}

impl CompactionStrategy for ObservationMasking {
    fn propose(
        &self,
        input: CompactionInput,
        control: OperationControl,
    ) -> Pin<Box<dyn Future<Output = Result<CompactionPreview, CompactionError>> + Send + '_>> {
        let inner = control.clone();
        Box::pin(controlled(
            control,
            async move { self.generate(input, inner) },
        ))
    }
}

/// Render the covered items in order. Pure, deterministic and total: no clock,
/// no identities and no I/O.
pub(super) fn masked_span(items: &[ContextItem]) -> String {
    // A result names its call, and the tool name lives on the assistant block
    // that proposed it, so collect the pairs before rendering any result.
    let mut names: HashMap<&CallId, &str> = HashMap::new();
    for item in items {
        if let ContextItem::Assistant { content, .. } = item {
            for block in content {
                if let ContentBlock::ToolCall { call_id, name, .. } = block {
                    names.insert(call_id, name);
                }
            }
        }
    }
    let mut out = String::new();
    for item in items {
        match item {
            ContextItem::Instructions { .. } => {}
            ContextItem::User { content, .. } => {
                out.push_str("--- user ---\n");
                push_blocks(&mut out, content);
            }
            ContextItem::Assistant {
                outcome, content, ..
            } => {
                out.push_str(&format!("--- assistant ({}) ---\n", outcome_name(*outcome)));
                push_blocks(&mut out, content);
            }
            ContextItem::ToolResult { result, .. } => {
                out.push_str("--- tool result ---\n");
                let name = names
                    .get(&result.call.call_id)
                    .copied()
                    .unwrap_or("unknown");
                let bytes: usize = result.content.iter().map(block_bytes).sum();
                let digest = result
                    .metadata
                    .artifacts
                    .first()
                    .map(|artifact| format!(" digest sha256:{}", artifact.sha256))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "[tool result: {name} {} {bytes} B{digest}]\n",
                    status_name(result.status)
                ));
            }
            ContextItem::Summary { summary, .. } => {
                out.push_str("--- summary ---\n");
                out.push_str(summary);
                if !summary.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// Text blocks verbatim, tool calls as one bounded line, media as a placeholder.
/// Generated reasoning and provider state are not summary text and are omitted.
fn push_blocks(out: &mut String, content: &[ContentBlock]) {
    for block in content {
        match block {
            ContentBlock::Text { text } => {
                out.push_str(text);
                if !text.ends_with('\n') {
                    out.push('\n');
                }
            }
            ContentBlock::ToolCall {
                name, arguments, ..
            } => {
                let arguments =
                    serde_json::to_string(&arguments).expect("tool arguments serialize");
                out.push_str(&format!("[call {name} {}]\n", bounded(&arguments)));
            }
            ContentBlock::Image {
                artifact,
                mime_type,
            } => out.push_str(&format!(
                "[image {mime_type} {} B sha256:{}]\n",
                artifact.bytes, artifact.sha256
            )),
            ContentBlock::Audio {
                artifact,
                mime_type,
            } => out.push_str(&format!(
                "[audio {mime_type} {} B sha256:{}]\n",
                artifact.bytes, artifact.sha256
            )),
            ContentBlock::File {
                artifact,
                mime_type,
                name,
            } => out.push_str(&format!(
                "[file {name} {mime_type} {} B sha256:{}]\n",
                artifact.bytes, artifact.sha256
            )),
            ContentBlock::ReasoningSummary { .. } | ContentBlock::ProviderState { .. } => {}
        }
    }
}

/// Bytes of one result body, which the placeholder replaces.
fn block_bytes(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => text.len(),
        ContentBlock::ReasoningSummary { text } => text.len(),
        ContentBlock::Image { artifact, .. }
        | ContentBlock::Audio { artifact, .. }
        | ContentBlock::File { artifact, .. } => usize::try_from(artifact.bytes).unwrap_or(0),
        ContentBlock::ToolCall { arguments, .. } => arguments.len(),
        ContentBlock::ProviderState { .. } => 0,
    }
}

/// Byte-bounded single line with an explicit elision marker and omitted count.
fn bounded(text: &str) -> String {
    if text.len() <= ARGUMENT_BYTES {
        return text.into();
    }
    let mut end = ARGUMENT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… (+{} B)", &text[..end], text.len() - end)
}

/// Persisted `snake_case` status spelling.
pub(super) fn status_name(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Succeeded => "succeeded",
        ToolStatus::Failed => "failed",
        ToolStatus::InvalidArguments => "invalid_arguments",
        ToolStatus::Denied => "denied",
        ToolStatus::TimedOut => "timed_out",
        ToolStatus::Cancelled => "cancelled",
        ToolStatus::InfrastructureFailure => "infrastructure_failure",
        ToolStatus::Uncertain => "uncertain",
    }
}

/// Persisted `snake_case` outcome spelling.
pub(super) fn outcome_name(outcome: ResponseOutcome) -> &'static str {
    match outcome {
        ResponseOutcome::Completed => "completed",
        ResponseOutcome::Failed => "failed",
        ResponseOutcome::Cancelled => "cancelled",
        ResponseOutcome::Interrupted => "interrupted",
    }
}
