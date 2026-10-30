//! Decision 16: opaque native payload bytes never reach `Debug`.

use super::{NativeContinuation, ProviderEvent, ProviderOutcome};

const SENTINEL: &[u8] = b"SENTINEL-ENCRYPTED";

fn assert_redacted(rendered: &str) {
    assert!(!rendered.contains("SENTINEL"), "{rendered}");
    assert!(!rendered.contains("ENCRYPTED"), "{rendered}");
    assert!(rendered.contains("[redacted; 18 bytes]"), "{rendered}");
}

#[test]
fn provider_state_debug_redacts_payload_and_keeps_other_fields() {
    let rendered = format!(
        "{:?}",
        ProviderEvent::ProviderState {
            block_index: 3,
            schema: "openai.responses.reasoning.v1".into(),
            payload: SENTINEL.to_vec(),
        }
    );
    assert_redacted(&rendered);
    assert!(
        rendered.contains("openai.responses.reasoning.v1"),
        "{rendered}"
    );
    assert!(rendered.contains("block_index: 3"), "{rendered}");
}

#[test]
fn native_continuation_debug_redacts_payload_and_keeps_other_fields() {
    let rendered = format!(
        "{:?}",
        NativeContinuation {
            schema: "handle.v1".into(),
            payload: SENTINEL.to_vec(),
        }
    );
    assert_redacted(&rendered);
    assert!(rendered.contains("handle.v1"), "{rendered}");
}

#[test]
fn finished_debug_redacts_a_nested_continuation() {
    let rendered = format!(
        "{:?}",
        ProviderEvent::Finished {
            outcome: ProviderOutcome::Completed,
            finish_reason: Some(crate::session::FinishReason::Stop),
            error: None,
            continuation: Some(NativeContinuation {
                schema: "handle.v1".into(),
                payload: SENTINEL.to_vec(),
            }),
        }
    );
    assert_redacted(&rendered);
    assert!(rendered.contains("Completed"), "{rendered}");
}

#[test]
fn transparent_events_keep_their_derived_shape() {
    assert_eq!(format!("{:?}", ProviderEvent::Started), "Started");
    assert_eq!(
        format!("{:?}", ProviderEvent::ToolCallReady { block_index: 7 }),
        "ToolCallReady { block_index: 7 }"
    );
}
