//! Undo selection facts and the deterministic model note text, shared by
//! storage validation and the runtime so the two can never disagree.

use super::{
    CURRENT_VERSION, CallRef, Commit, Digest, Entry, EntryId, EntryPayload, FileOutcome,
    HeadChange, HeadReason, JournalEvent, Record, RecordId, RecordPayload, SessionId, StoreError,
    Timestamp, TurnId, UndoFileOutcome, UndoId, UndoIntent, UndoNote, UndoOutcome, UndoResult,
    UndoTarget, codec,
};

/// The most bytes a resolution note may hold. One constant, used by the
/// runtime's pre-commit refusal and by storage's record rules, so a client
/// refusal can never disagree with what the journal accepts.
pub const MAX_RESOLUTION_NOTE_BYTES: usize = 64 * 1024;
/// The most bytes an uncertainty reason may hold.
pub const MAX_UNCERTAINTY_REASON_BYTES: usize = 1024;

/// A reason string of at most [`MAX_UNCERTAINTY_REASON_BYTES`], cut at a UTF-8
/// boundary. Runtime reasons are fixed short strings; this keeps a future
/// longer one from faulting a session instead of being recorded.
pub(crate) fn bounded_reason(reason: &str) -> String {
    if reason.len() <= MAX_UNCERTAINTY_REASON_BYTES {
        return reason.to_owned();
    }
    let mut end = MAX_UNCERTAINTY_REASON_BYTES;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_owned()
}

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

/// Whether the intent and every terminal shape it could produce fit one
/// journal record (ADR 0032 §8): the intent, then each outcome with a maximum
/// resolution note under worst-case JSON escaping, every target and every
/// omitted call. Checked before commit A and before any target changes, so a
/// terminal commit can never exceed the physical limit only after files moved.
pub(crate) fn fits_record_limit(intent: &UndoIntent, sequence: u64) -> bool {
    // One byte per character and six characters of escaping per byte is the
    // widest a `serde_json` string can get, before the note text is escaped a
    // second time as it enters the record.
    let note: String = "\u{1}".repeat(MAX_RESOLUTION_NOTE_BYTES);
    let omitted: Vec<(CallRef, String)> = intent
        .omitted_calls
        .iter()
        .map(|call| (call.clone(), "bash".into()))
        .collect();
    let anchor = intent.anchor_head.clone();
    let intent_record = record(
        sequence.saturating_add(1),
        Commit {
            command_id: Some(super::CommandId::new()),
            command_fingerprint: Some(Digest::of(b"preflight")),
            head: HeadChange {
                from: anchor.clone(),
                to: anchor.clone(),
                reason: HeadReason::Retain,
            },
            entries: Vec::new(),
            events: vec![JournalEvent::UndoIntent(intent.clone())],
        },
    );
    if !bounded(&intent_record) {
        return false;
    }
    // An uncertainty record is small, but it shares the journal.
    let uncertain = record(
        sequence.saturating_add(2),
        Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: anchor.clone(),
                to: anchor.clone(),
                reason: HeadReason::Retain,
            },
            entries: Vec::new(),
            events: vec![JournalEvent::UndoUncertain(super::UndoUncertain {
                undo_id: intent.undo_id.clone(),
                reason: "r".repeat(MAX_UNCERTAINTY_REASON_BYTES),
            })],
        },
    );
    if !bounded(&uncertain) {
        return false;
    }
    for outcome in [
        UndoOutcome::Succeeded,
        UndoOutcome::NotApplied,
        UndoOutcome::Observed,
        UndoOutcome::AcknowledgedUnknown,
    ] {
        let rows: Vec<UndoFileOutcome> = intent
            .targets
            .iter()
            .map(|target| UndoFileOutcome {
                path: target.path.clone(),
                outcome: match outcome {
                    UndoOutcome::Succeeded if target.pre_image.is_some() => FileOutcome::Restored,
                    UndoOutcome::Succeeded => FileOutcome::Removed,
                    UndoOutcome::NotApplied => FileOutcome::Unchanged,
                    UndoOutcome::Observed => FileOutcome::ObservedRestored,
                    UndoOutcome::AcknowledgedUnknown => FileOutcome::Unknown,
                },
            })
            .collect();
        let resolution = matches!(
            outcome,
            UndoOutcome::Observed | UndoOutcome::AcknowledgedUnknown
        )
        .then_some(note.as_str());
        let text = note_text(
            &intent.target_turn_id,
            &intent.undo_id,
            outcome,
            &intent.targets,
            &rows,
            &omitted,
            resolution,
        );
        let note_entry = EntryId::new();
        let terminal = record(
            sequence.saturating_add(4),
            Commit {
                command_id: None,
                command_fingerprint: None,
                head: HeadChange {
                    from: anchor.clone(),
                    to: Some(note_entry.clone()),
                    reason: HeadReason::Advance,
                },
                entries: vec![Entry {
                    id: note_entry.clone(),
                    parent_id: anchor.clone(),
                    turn_id: None,
                    payload: EntryPayload::UndoNote(UndoNote {
                        undo_id: intent.undo_id.clone(),
                        target_turn_id: intent.target_turn_id.clone(),
                        outcome,
                        text,
                    }),
                }],
                events: vec![JournalEvent::UndoResult(UndoResult {
                    undo_id: intent.undo_id.clone(),
                    outcome,
                    file_outcomes: rows,
                    note_entry_id: note_entry,
                    resolution_note: resolution.map(str::to_owned),
                })],
            },
        );
        if !bounded(&terminal) {
            return false;
        }
    }
    true
}

fn record(sequence: u64, commit: Commit) -> Record {
    Record {
        version: CURRENT_VERSION,
        seq: sequence,
        record_id: RecordId::new(),
        session_id: SessionId::new(),
        at: Timestamp::now(),
        previous_checksum: Some(Digest::of(b"preflight")),
        payload: RecordPayload::Commit(commit),
    }
}

fn bounded(record: &Record) -> bool {
    !matches!(codec::frame(record), Err(StoreError::Limit))
}
