//! Deterministic, metadata-only media references and bounded displayable tools.
use super::{ContextError, ConversionKind, InputMediaKind, ModelCapabilities, RequiredCapability};
use crate::session::{ContentBlock, ToolResult};

pub(super) fn classify(
    block: &ContentBlock,
    capabilities: &ModelCapabilities,
) -> Option<(RequiredCapability, ConversionKind)> {
    let (media, required) = match block {
        ContentBlock::Image { .. } => (InputMediaKind::Image, RequiredCapability::Image),
        ContentBlock::Audio { .. } => (InputMediaKind::Audio, RequiredCapability::Audio),
        ContentBlock::File { .. } => (InputMediaKind::File, RequiredCapability::File),
        ContentBlock::ToolCall { .. } if !capabilities.tools => {
            return Some((RequiredCapability::Tools, ConversionKind::ToolCallText));
        }
        _ => return None,
    };
    (!capabilities.input_media.contains(&media))
        .then_some((required, ConversionKind::MediaReference))
}

/// Bound the payload in UTF-8 bytes, including an explicit truncation marker.
fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit - " [truncated]".len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [truncated]", &text[..end])
}

pub(super) fn block(block: &ContentBlock) -> Result<String, ContextError> {
    let (kind, artifact, mime_type, name) = match block {
        ContentBlock::Image {
            artifact,
            mime_type,
        } => ("image", artifact, mime_type, None),
        ContentBlock::Audio {
            artifact,
            mime_type,
        } => ("audio", artifact, mime_type, None),
        ContentBlock::File {
            artifact,
            mime_type,
            name,
        } => ("file", artifact, mime_type, Some(name)),
        ContentBlock::ToolCall {
            name, arguments, ..
        } => {
            let arguments = serde_json::to_string(arguments)
                .map_err(|_| ContextError::Invariant("unserializable_arguments"))?;
            return Ok(format!("[tool call {name} {}]", bounded(&arguments, 2_000)));
        }
        _ => return Err(ContextError::Invariant("non_displayable_conversion")),
    };
    let name = name
        .map(|name| format!(", name {}", serde_json::to_string(name).expect("string")))
        .unwrap_or_default();
    Ok(format!(
        "[{kind} omitted for this model: {mime_type}, {} bytes, sha256 {}{name}]",
        artifact.bytes,
        &artifact.sha256.as_str()[..12]
    ))
}

pub(super) fn tool_result(name: &str, result: &ToolResult) -> String {
    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            // Tool media is represented by its stored metadata, never described.
            ContentBlock::Image { .. } | ContentBlock::Audio { .. } | ContentBlock::File { .. } => {
                block_reference(block)
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let status = serde_json::to_value(result.status).expect("status");
    let code = result
        .metadata
        .code
        .as_ref()
        .map(|code| format!(" {code}"))
        .unwrap_or_default();
    format!(
        "[tool result {name} {}{code}: {}]",
        status.as_str().expect("status string"),
        bounded(&text, 4_000)
    )
}

fn block_reference(content: &ContentBlock) -> Option<String> {
    block(content).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn media_reference_uses_only_stored_metadata_and_optional_name() {
        use crate::session::{ArtifactRef, Digest};
        let artifact = ArtifactRef {
            sha256: Digest::of(b"media"),
            bytes: 5,
        };
        assert_eq!(
            block(&ContentBlock::Image {
                artifact: artifact.clone(),
                mime_type: "image/png".into()
            })
            .unwrap(),
            "[image omitted for this model: image/png, 5 bytes, sha256 721c9525ade2]"
        );
        assert_eq!(
            block(&ContentBlock::Audio {
                artifact: artifact.clone(),
                mime_type: "audio/wav".into()
            })
            .unwrap(),
            "[audio omitted for this model: audio/wav, 5 bytes, sha256 721c9525ade2]"
        );
        assert_eq!(
            block(&ContentBlock::File {
                artifact,
                mime_type: "application/pdf".into(),
                name: "file.pdf".into()
            })
            .unwrap(),
            "[file omitted for this model: application/pdf, 5 bytes, sha256 721c9525ade2, name \"file.pdf\"]"
        );
    }

    #[test]
    fn tool_renderings_bound_the_actual_payload_not_character_count() {
        use crate::session::{CallId, CallRef, EntryId, Provenance, ToolMetadata, ToolStatus};
        let long = "💡".repeat(2_000);
        let call = ContentBlock::ToolCall {
            call_id: CallId::new(),
            name: "read".into(),
            arguments: serde_json::from_value(serde_json::json!({"path": long})).unwrap(),
            provider_call_id: None,
        };
        let rendered = block(&call).unwrap();
        assert!(rendered.len() <= "[tool call read ]".len() + 2_000);
        assert!(rendered.ends_with(" [truncated]]"));
        let result = ToolResult {
            call: CallRef {
                response_entry_id: EntryId::new(),
                call_id: CallId::new(),
            },
            invocation_id: None,
            status: ToolStatus::Failed,
            content: vec![ContentBlock::Text { text: long }],
            metadata: ToolMetadata {
                exit_code: None,
                duration_ms: None,
                output_truncated: false,
                provenance: Provenance::Runtime,
                artifacts: vec![],
                code: Some("failed".into()),
            },
        };
        let rendered = tool_result("read", &result);
        assert!(rendered.starts_with("[tool result read failed failed: "));
        assert!(rendered.len() <= "[tool result read failed failed: ]".len() + 4_000);
        assert!(rendered.ends_with(" [truncated]]"));
        assert_eq!(rendered, tool_result("read", &result));
        assert!(
            block(&ContentBlock::ProviderState {
                owner: crate::session::ModelRef {
                    provider_instance: "mock".into(),
                    model: "signed".into()
                },
                schema: "anthropic.messages.thinking.v1".into(),
                artifact: crate::session::ArtifactRef {
                    sha256: crate::session::Digest::of(b"signed"),
                    bytes: 6
                }
            })
            .is_err()
        );
    }

    #[test]
    fn bounded_utf8_marks_truncation_within_byte_limit() {
        let text = "💡".repeat(2_000);
        for limit in [2_000, 4_000] {
            let output = bounded(&text, limit);
            assert!(output.len() <= limit);
            assert!(output.ends_with(" [truncated]"));
            assert_eq!(output, bounded(&text, limit));
        }
    }
}
