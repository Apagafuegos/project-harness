use harness_core::session::{ContentBlock, ToolStatus};
use std::fmt::Debug;

pub fn assert_status(
    actual: ToolStatus,
    expected: ToolStatus,
    code: impl Debug,
    content: &[ContentBlock],
) {
    let text: String = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(actual, expected, "code={code:?}; text={text}");
}

#[test]
fn status_failure_includes_code_and_model_text() {
    let panic = std::panic::catch_unwind(|| {
        assert_status(
            ToolStatus::InfrastructureFailure,
            ToolStatus::Failed,
            Some("sandbox_unavailable"),
            &[ContentBlock::Text {
                text: "sandbox_unavailable: acquire_init: diagnostic sentinel".into(),
            }],
        );
    })
    .unwrap_err();
    let message = panic.downcast_ref::<String>().unwrap();
    assert!(
        message.contains("code=Some(\"sandbox_unavailable\")"),
        "{message}"
    );
    assert!(
        message.contains("text=sandbox_unavailable: acquire_init: diagnostic sentinel"),
        "{message}"
    );
}
