//! Opt-in live-fixture recording. Raw SSE, IDs and opaque state never hit disk.
//!
//! This is deliberately lossy: rebuild only fields consumed by the mapper,
//! replace all content strings, and derive fake item identities from numeric
//! output indices. Unknown events and provider diagnostics are not recorded.

use std::{io::Write, path::PathBuf};

use serde_json::{Value, json};

use crate::sse::SseEvent;

pub(crate) struct Recorder {
    path: PathBuf,
    body: String,
}

impl Recorder {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            body: "# RECORDED: ChatGptCodex live stream, 2026-10-30; IDs, content and encrypted payloads replaced before writing.\nstatus: 200\nheader: content-type: text/event-stream\n---\n".into(),
        }
    }

    pub(crate) fn push(&mut self, event: &SseEvent) {
        let Some(value) = sanitize(&event.data) else {
            return;
        };
        let kind = value["type"].as_str().expect("sanitizer type");
        self.body
            .push_str(&format!("event: {kind}\ndata: {value}\n\n"));
    }

    /// Save only the first successfully completed stream; never overwrite an
    /// earlier capture in a multi-turn check. No error includes paths/content.
    pub(crate) fn save(&self) -> Result<(), ()> {
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            Err(_) => return Err(()),
        };
        file.write_all(self.body.as_bytes()).map_err(|_| ())
    }
}

fn sanitize(data: &str) -> Option<Value> {
    let raw: Value = serde_json::from_str(data).ok()?;
    let kind = raw["type"].as_str()?;
    let index = raw["output_index"].as_u64().unwrap_or(0);
    let mut event = json!({"type": kind});
    match kind {
        "response.created" | "response.in_progress" | "response.completed" => {
            // Do not copy response.output, ids, metadata, account or usage counts.
            event["response"] = json!({});
            if raw.pointer("/response/usage").is_some_and(Value::is_object) {
                // Synthetic counts preserve only availability, never live totals.
                event["response"]["usage"] = json!({"input_tokens":1,"output_tokens":1});
            }
        }
        "response.output_item.added" | "response.output_item.done" => {
            let item_type = raw.pointer("/item/type")?.as_str()?;
            let mut item = json!({"id":format!("recorded-item-{index}"),"type":item_type});
            match item_type {
                "message" => {
                    item["role"] = json!("assistant");
                    item["content"] = json!([]);
                }
                "reasoning" => {
                    item["summary"] = json!([]);
                    if raw
                        .pointer("/item/encrypted_content")
                        .is_some_and(Value::is_string)
                    {
                        item["encrypted_content"] = json!("[redacted encrypted content]");
                    }
                }
                // The recorded first turn is text-only; no tool names/arguments
                // or hosted-tool outputs are retained by this recorder.
                _ => return None,
            }
            event["item"] = item;
            event["output_index"] = json!(index);
        }
        "response.output_text.delta" | "response.reasoning_summary_text.delta" => {
            event["item_id"] = json!(format!("recorded-item-{index}"));
            event["output_index"] = json!(index);
            event["delta"] = json!("[recorded text]");
        }
        _ => return None,
    }
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorder_strips_ids_credentials_state_and_text_before_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stream.sse");
        let mut recorder = Recorder::new(path.clone());
        for raw in [
            json!({"type":"response.created","response":{"id":"SENTINEL-id","account_id":"SENTINEL-account","token":"SENTINEL-token"}}),
            json!({"type":"response.output_item.done","output_index":2,"item":{"type":"reasoning","id":"SENTINEL-item","encrypted_content":"SENTINEL-payload","summary":[{"text":"SENTINEL-summary"}]}}),
            json!({"type":"response.output_text.delta","output_index":3,"item_id":"SENTINEL-item","delta":"SENTINEL-text"}),
            json!({"type":"response.completed","response":{"id":"SENTINEL-id","output":[{"text":"SENTINEL-text"}],"usage":{"input_tokens":999999}}}),
        ] {
            recorder.push(&SseEvent {
                data: raw.to_string(),
                ..Default::default()
            });
        }
        recorder.save().unwrap();
        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(!bytes.contains("SENTINEL"));
        assert!(!bytes.contains("999999"));
        assert!(bytes.contains("RECORDED"));
        assert!(bytes.contains("recorded-item-2"));
        assert!(bytes.contains("[redacted encrypted content]"));
        // Another completed stream may not replace the first capture.
        Recorder::new(path.clone()).save().unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), bytes);
    }
}
