//! The `sessions undo` report text: paths, calls and no pre-image bytes.

use super::*;
use harness_core::session::{CallId, CallRef, EntryId, TurnId};

fn call(
    name: &str,
    arguments: &str,
    status: Option<harness_core::session::ToolStatus>,
) -> UndoOmittedCall {
    UndoOmittedCall {
        call: CallRef {
            response_entry_id: EntryId::new(),
            call_id: CallId::new(),
        },
        tool: name.into(),
        arguments: arguments.into(),
        status,
    }
}

#[test]
fn the_preview_names_restores_removals_and_omitted_calls() {
    let preview = UndoPreview {
        undo_id: harness_core::session::UndoId::new(),
        target_turn_id: TurnId::new(),
        head: None,
        sequence: 7,
        configuration: harness_core::session::Digest::of(b"configuration"),
        targets: vec![
            harness_core::runtime::UndoPreviewTarget {
                path: "src/a.rs".into(),
                restores: true,
                bytes: Some(12),
            },
            harness_core::runtime::UndoPreviewTarget {
                path: "src/b.rs".into(),
                restores: false,
                bytes: None,
            },
        ],
        omitted: vec![call(
            "bash",
            r#"{"command":"cargo test","cwd":"."}"#,
            Some(harness_core::session::ToolStatus::Succeeded),
        )],
    };
    let text = preview_text(&preview);
    assert!(text.contains("restore: \"src/a.rs\""), "{text}");
    assert!(
        text.contains("remove (created by the agent): \"src/b.rs\""),
        "{text}"
    );
    assert!(text.contains("Not reversible: bash"), "{text}");
    assert!(
        text.contains("arguments: {\"command\":\"cargo test\""),
        "{text}"
    );
    assert!(
        text.ends_with("Other effects are not reversed. No redo."),
        "{text}"
    );
}

#[test]
fn an_empty_target_set_says_so_and_never_lists_bytes() {
    let preview = UndoPreview {
        undo_id: harness_core::session::UndoId::new(),
        target_turn_id: TurnId::new(),
        head: None,
        sequence: 1,
        configuration: harness_core::session::Digest::of(b"configuration"),
        targets: Vec::new(),
        omitted: Vec::new(),
    };
    let text = preview_text(&preview);
    assert!(
        text.contains("no trusted file changes require restoration"),
        "{text}"
    );
    assert!(
        !text.contains("sha256"),
        "pre-images never appear in a report"
    );
}

#[test]
fn a_not_executed_call_is_labelled_and_a_conflict_lists_its_rows() {
    let text = omitted_text(&[call("bash", "", None)]);
    assert!(text.contains("(not executed)"), "{text}");
    let refusal = refusal(Rejection::Undo {
        code: "undo_conflict",
        detail: String::new(),
        conflicts: vec![harness_core::session::UndoConflict {
            path: "src/a.rs".into(),
            reason: "changed",
        }],
        omitted: Vec::new(),
    });
    match refusal {
        Failure::Refusal(text) => assert_eq!(text, "undo_conflict\n  changed \"src/a.rs\""),
        Failure::Storage(_) => panic!("a conflict is a refusal"),
    }
}

#[test]
fn every_undo_path_escapes_terminal_controls() {
    use harness_core::{
        runtime::UndoPreviewTarget,
        session::{Digest, FileOutcome, UndoConflict, UndoFileOutcome, UndoId},
    };
    let path = "file\x1b[31m\nname";
    let preview = UndoPreview {
        undo_id: UndoId::new(),
        target_turn_id: TurnId::new(),
        head: None,
        sequence: 1,
        configuration: Digest::of(b"configuration"),
        targets: vec![
            UndoPreviewTarget {
                path: path.into(),
                restores: true,
                bytes: Some(1),
            },
            UndoPreviewTarget {
                path: path.into(),
                restores: false,
                bytes: None,
            },
        ],
        omitted: Vec::new(),
    };
    let Failure::Refusal(conflict) = refusal(Rejection::Undo {
        code: "undo_conflict",
        detail: String::new(),
        conflicts: vec![UndoConflict {
            path: path.into(),
            reason: "changed",
        }],
        omitted: Vec::new(),
    }) else {
        panic!("expected refusal");
    };
    let rows = [
        preview_text(&preview),
        result_row(&UndoFileOutcome {
            path: path.into(),
            outcome: FileOutcome::Restored,
        }),
        conflict,
    ];
    for row in rows {
        assert!(!row.contains('\x1b'), "terminal escape leaked: {row:?}");
        assert!(row.contains(&format!("{path:?}")), "{row}");
    }
}
