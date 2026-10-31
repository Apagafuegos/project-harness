//! The idle-only `SetContextControl` command: acceptance, every refusal, the
//! typed mapping of storage's codes, reopening, and the event it publishes.

use super::pipeline::{enabled, propose};
use super::scenarios::{begin, plain_turn};
use super::*;
use crate::session::ContextMode;
use serde_json::json;

/// The last entry's turn on the current branch.
fn last_turn(d: &Driver) -> TurnId {
    let snapshot = d.runtime.snapshot();
    snapshot
        .ancestry(snapshot.head())
        .unwrap()
        .iter()
        .rev()
        .find_map(|stored| stored.entry.turn_id.clone())
        .expect("a turn on this branch")
}

fn control(d: &mut Driver, turn: &TurnId, mode: ContextMode) -> RequestId {
    d.command(Command::SetContextControl {
        turn_id: turn.clone(),
        mode,
    })
}

#[test]
fn set_context_control_is_accepted_while_idle() {
    let mut d = Driver::new();
    plain_turn(&mut d, "one", "first");
    let turn = last_turn(&d);
    let sequence = d.runtime.snapshot().sequence();
    let request = control(&mut d, &turn, ContextMode::Pinned);
    d.settle();
    let Accepted::ContextControlSet { ack, turn_id, mode } = d.accepted(request) else {
        panic!("not a control acceptance")
    };
    assert_eq!(turn_id, &turn);
    assert_eq!(*mode, ContextMode::Pinned);
    assert_eq!(ack.sequence, sequence + 1);
    // The entry is at the head, and the head moved to it.
    let snapshot = d.runtime.snapshot();
    let stored = snapshot.entry(snapshot.head().unwrap()).unwrap();
    let EntryPayload::ContextControl(entry) = &stored.entry.payload else {
        panic!("the head is not a control")
    };
    assert_eq!(entry.turn_id, turn);
    assert_eq!(entry.mode, ContextMode::Pinned);
    assert_eq!(stored.entry.turn_id, None, "a control is not dialogue");
    assert!(d.segment.events().any(|event| matches!(
        &event.kind,
        EventKind::ContextControlSet { turn_id: id, mode } if *id == turn && *mode == ContextMode::Pinned
    )));

    // A repeat appends nothing: it is a no-op control, refused by code.
    let sequence = snapshot.sequence();
    let repeat = control(&mut d, &turn, ContextMode::Pinned);
    assert_eq!(
        d.rejected(repeat),
        Rejection::Invalid("context_control_noop")
    );
    assert_eq!(d.runtime.snapshot().sequence(), sequence);
    assert!(d.pending.is_none(), "nothing was written");

    // Undo is another control, and it is accepted while idle.
    let reset = control(&mut d, &turn, ContextMode::Default);
    d.settle();
    assert!(matches!(
        d.accepted(reset),
        Accepted::ContextControlSet {
            mode: ContextMode::Default,
            ..
        }
    ));
}

#[test]
fn set_context_control_is_refused_when_not_idle() {
    // Busy: a turn is active and a commit is in flight.
    let mut d = Driver::new();
    plain_turn(&mut d, "one", "first");
    let turn = last_turn(&d);
    begin(&mut d, "busy");
    let request = control(&mut d, &turn, ContextMode::Pinned);
    assert_eq!(d.rejected(request), Rejection::Busy);

    // Busy while a durable approval ticket waits for the user.
    let mut d = enabled();
    propose(&mut d, &[("bash", json!({"command":"true"}))]);
    d.settle();
    assert!(d.plan());
    d.settle();
    assert!(d.runtime.view().pending_approval.is_some());
    let turn = d.runtime.snapshot().active_turn().unwrap().clone();
    let request = control(&mut d, &turn, ContextMode::Pinned);
    assert_eq!(d.rejected(request), Rejection::Busy);

    // RecoveryRequired: attached with unfinished work, before `Start` runs.
    let mut d = Driver::new();
    plain_turn(&mut d, "one", "first");
    let turn = last_turn(&d);
    let (open, _) = d.start("unfinished");
    d.settle();
    d.accepted(open);
    d.reopen_unstarted();
    let request = control(&mut d, &turn, ContextMode::Pinned);
    assert_eq!(d.rejected(request), Rejection::RecoveryRequired);

    // ResolutionRequired: an uncertain invocation waits for the user.
    let mut d = super::recovery::fixture();
    d.settle();
    assert_eq!(d.runtime.phase(), Phase::AwaitingResolution);
    let turn = d
        .runtime
        .snapshot()
        .turns()
        .next()
        .map(|(turn, _)| turn.clone())
        .expect("a turn in the fixture");
    let request = control(&mut d, &turn, ContextMode::Pinned);
    assert_eq!(d.rejected(request), Rejection::ResolutionRequired);

    // Faulted: a persistence failure stopped this runtime.
    let mut d = Driver::new();
    plain_turn(&mut d, "one", "first");
    let turn = last_turn(&d);
    begin(&mut d, "fault");
    d.step(Input::StorageFailed {
        error: PersistError::WorkerLost,
    });
    assert_eq!(d.runtime.phase(), Phase::Faulted);
    let request = control(&mut d, &turn, ContextMode::Pinned);
    assert_eq!(d.rejected(request), Rejection::Faulted);
}

#[test]
fn a_storage_rejection_is_typed_and_writes_nothing() {
    let mut d = Driver::new();
    plain_turn(&mut d, "one", "first");
    let turn = last_turn(&d);
    let sequence = d.runtime.snapshot().sequence();
    for (target, mode, code) in [
        (
            TurnId::new(),
            ContextMode::Pinned,
            "context_control_unfinished_turn",
        ),
        (turn.clone(), ContextMode::Default, "context_control_noop"),
    ] {
        let request = d.command(Command::SetContextControl {
            turn_id: target,
            mode,
        });
        assert_eq!(d.rejected(request), Rejection::Invalid(code));
        assert_eq!(d.runtime.snapshot().sequence(), sequence, "{code}");
        assert!(d.pending.is_none(), "{code}");
    }
    // Navigating away also makes the target off-branch, with the same typed code.
    let root = d.command(Command::SelectHead { head: None });
    d.settle();
    assert!(matches!(d.accepted(root), Accepted::HeadSelected { .. }));
    let request = control(&mut d, &turn, ContextMode::Pinned);
    assert_eq!(
        d.rejected(request),
        Rejection::Invalid("context_control_not_ancestral")
    );
}

#[test]
fn a_control_survives_a_reopen() {
    let mut d = Driver::new();
    plain_turn(&mut d, "one", "first");
    let turn = last_turn(&d);
    let request = control(&mut d, &turn, ContextMode::Excluded);
    d.settle();
    d.accepted(request);
    let sequence = d.runtime.snapshot().sequence();
    d.reopen();
    d.settle();
    let snapshot = d.runtime.snapshot();
    assert_eq!(snapshot.sequence(), sequence);
    let stored = snapshot.entry(snapshot.head().unwrap()).unwrap();
    let EntryPayload::ContextControl(entry) = &stored.entry.payload else {
        panic!("the control is still the head")
    };
    assert_eq!(entry.mode, ContextMode::Excluded);
    // The fold survived, so a reset is a real change and the pin is a no-op.
    let repeat = control(&mut d, &turn, ContextMode::Excluded);
    assert_eq!(
        d.rejected(repeat),
        Rejection::Invalid("context_control_noop")
    );
    let reset = control(&mut d, &turn, ContextMode::Default);
    d.settle();
    assert!(matches!(
        d.accepted(reset),
        Accepted::ContextControlSet { .. }
    ));
}
