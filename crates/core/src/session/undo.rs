//! Undo selection facts and the deterministic model note text, shared by
//! storage validation and the runtime so the two can never disagree.

use super::{CallRef, FileOutcome, TurnId, UndoFileOutcome, UndoId, UndoOutcome, UndoTarget};

/// Why one target of a selected turn cannot be undone, without file contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoConflict {
    /// Canonical workspace path the conflict is about.
    pub path: String,
    /// Stable, content-free reason code.
    pub reason: &'static str,
}

/// The outcome of selecting an undo candidate on one ancestry.
#[derive(Clone, Debug, PartialEq)]
pub enum UndoSelection {
    /// The newest unconsumed finished turn with trusted file mutations.
    Candidate {
        /// The selected turn.
        turn_id: TurnId,
        /// Targets to restore, unique by path and sorted bytewise.
        targets: Vec<UndoTarget>,
        /// Calls the undo does not reverse, in ancestral proposal order.
        omitted_calls: Vec<CallRef>,
    },
    /// No turn can be undone. `omitted_calls` lists the bash calls that were
    /// skipped on the way, which the refusal still reports.
    Nothing {
        /// Bash proposals of the skipped turns, in ancestral order.
        omitted_calls: Vec<CallRef>,
    },
    /// The selected head stops inside the target turn's recorded entries.
    Incomplete(TurnId),
    /// The target turn has a trusted mutation recorded before capture existed.
    Legacy(TurnId),
    /// The target turn has a mutation whose application is unknown.
    UnknownEffect(TurnId),
    /// The target turn's stored images do not chain; nothing is restorable.
    Conflict(Vec<UndoConflict>),
}

/// The exact note text of one returned undo. Deterministic UTF-8, LF-separated
/// with a final LF; `omitted` is `(call, tool name)` in intent order.
pub(crate) fn note_text(
    turn: &TurnId,
    undo: &UndoId,
    outcome: UndoOutcome,
    targets: &[UndoTarget],
    files: &[UndoFileOutcome],
    omitted: &[(CallRef, String)],
    resolution_note: Option<&str>,
) -> String {
    let quote = |text: &str| serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
    let mut lines = Vec::new();
    match outcome {
        UndoOutcome::Succeeded => lines.push(format!(
            "The user reverted the agent's trusted file edits from turn {turn} (undo {undo})."
        )),
        UndoOutcome::NotApplied => lines.push(format!(
            "The user requested undo of turn {turn} (undo {undo}); no files were reverted."
        )),
        UndoOutcome::Observed => lines.push(format!(
            "The user resolved interrupted undo {undo} of turn {turn}; restoration was not \
             confirmed all-or-nothing."
        )),
        UndoOutcome::AcknowledgedUnknown => lines.push(format!(
            "The user acknowledged unknown effects of interrupted undo {undo} of turn {turn}; \
             which files were reverted is unknown."
        )),
    }
    if targets.is_empty() && matches!(outcome, UndoOutcome::Succeeded | UndoOutcome::NotApplied) {
        lines.push("No trusted file changes required restoration.".into());
    }
    // Rows are one per target in target order, which storage validates.
    for (target, row) in targets.iter().zip(files) {
        let path = quote(&row.path);
        let line = match outcome {
            UndoOutcome::Succeeded => {
                if target.pre_image.is_some() {
                    format!("Restored: {path}")
                } else {
                    format!("Removed (created by the agent): {path}")
                }
            }
            UndoOutcome::NotApplied => format!("Unchanged: {path}"),
            UndoOutcome::Observed => {
                format!("User observed {}: {path}", row.outcome.as_str())
            }
            UndoOutcome::AcknowledgedUnknown => format!("Unknown: {path}"),
        };
        lines.push(line);
    }
    if matches!(
        outcome,
        UndoOutcome::Observed | UndoOutcome::AcknowledgedUnknown
    ) {
        lines.push(format!(
            "User resolution note: {}",
            quote(resolution_note.unwrap_or_default())
        ));
    }
    for (call, name) in omitted {
        lines.push(format!(
            "Not reversed: {name} call {} from response {}.",
            call.call_id, call.response_entry_id
        ));
    }
    lines.push(format!(
        "Bash and other effects were not reversed. {}",
        if outcome == UndoOutcome::Succeeded {
            "Do not assume the reverted edits are present."
        } else {
            "Do not assume either the original edits or their restoration are present."
        }
    ));
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// `not_applied` reports every row as unchanged; acknowledged-unknown reports
/// every row as unknown. A one-row-per-target list is checked separately.
pub(crate) fn consistent(outcome: UndoOutcome, files: &[UndoFileOutcome]) -> bool {
    files.iter().all(|row| match outcome {
        UndoOutcome::Succeeded => {
            matches!(row.outcome, FileOutcome::Restored | FileOutcome::Removed)
        }
        UndoOutcome::NotApplied => row.outcome == FileOutcome::Unchanged,
        UndoOutcome::Observed => matches!(
            row.outcome,
            FileOutcome::ObservedRestored
                | FileOutcome::ObservedRemoved
                | FileOutcome::ObservedOther
                | FileOutcome::Unknown
        ),
        UndoOutcome::AcknowledgedUnknown => row.outcome == FileOutcome::Unknown,
    })
}
