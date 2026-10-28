//! The single D9 storage exception and its negative boundary.
use super::*;

fn result(snapshot: &SessionSnapshot) -> Commit {
    let invocation = &snapshot.unresolved_invocations()[0].intent;
    let mut commit = tool_result(
        snapshot,
        invocation.call.clone(),
        Some(invocation.invocation_id.clone()),
        ToolStatus::Cancelled,
        Provenance::Runtime,
    );
    commit.entries[0].turn_id = Some(invocation.turn_id.clone());
    let EntryPayload::ToolResult(r) = &mut commit.entries[0].payload else {
        unreachable!()
    };
    r.result.metadata.code = Some("process_restart".into());
    commit
}
fn setup(
    effects: &[EffectClass],
    recovering: bool,
    uncertain: bool,
) -> (tempfile::TempDir, Arc<DataRoot>, JournalStore) {
    let (temp, root, mut store, call) = authorizing();
    let backend = if effects.contains(&EffectClass::Process) {
        ExecutionBackend::Bubblewrap
    } else {
        ExecutionBackend::TrustedFs
    };
    let commit = intent(
        &root,
        &store.snapshot(),
        &call,
        backend,
        EffectSet::new(effects.iter().copied()).unwrap(),
    );
    store.append(commit, at()).unwrap();
    if recovering {
        let snapshot = store.snapshot();
        let event = if uncertain {
            JournalEvent::ExecutionUncertain(ExecutionUncertain {
                turn_id: snapshot.active_turn().unwrap().clone(),
                invocation_id: snapshot.unresolved_invocations()[0]
                    .intent
                    .invocation_id
                    .clone(),
                reason: "process_restart".into(),
            })
        } else {
            JournalEvent::StateChanged(StateChanged {
                turn_id: snapshot.active_turn().unwrap().clone(),
                from: RuntimeState::Authorizing,
                to: RuntimeState::Recovering,
            })
        };
        store.append(audit(&snapshot, vec![event]), at()).unwrap();
    }
    (temp, root, store)
}
#[test]
fn d9_accepts_only_unresolved_nonuncertain_read_during_recovery() {
    let (_temp, root, mut store) = setup(&[EffectClass::Read], true, false);
    store.append(result(&store.snapshot()), at()).unwrap();
    assert!(store.snapshot().unresolved_invocations().is_empty());
    let id = store.snapshot().session_id().clone();
    drop(store);
    assert!(
        JournalStore::open(root, &id)
            .unwrap()
            .snapshot()
            .unresolved_invocations()
            .is_empty()
    );
}
#[test]
fn d9_rejects_other_effects_and_outside_recovery() {
    use EffectClass::*;
    for effects in [
        &[FilesystemMutation][..],
        &[Process],
        &[Read, FilesystemMutation],
    ] {
        let (_temp, _, mut store) = setup(effects, true, false);
        assert!(
            store.append(result(&store.snapshot()), at()).is_err(),
            "{effects:?}"
        );
    }
    let (_temp, _, mut store) = setup(&[Read], false, false);
    assert!(matches!(
        store.append(result(&store.snapshot()), at()),
        Err(StoreError::Invalid("not_recovering"))
    ));
}
#[test]
fn d9_rejects_uncertain_read() {
    let (_temp, _, mut store) = setup(&[EffectClass::Read], true, true);
    assert!(matches!(
        store.append(result(&store.snapshot()), at()),
        Err(StoreError::Invalid("invalid_executed_provenance"))
    ));
}
#[test]
fn d9_rejects_other_status_and_code() {
    let (_temp, _, mut store) = setup(&[EffectClass::Read], true, false);
    for (status, code) in [
        (ToolStatus::Denied, "process_restart"),
        (ToolStatus::Cancelled, "cancelled"),
    ] {
        let mut commit = result(&store.snapshot());
        let EntryPayload::ToolResult(r) = &mut commit.entries[0].payload else {
            unreachable!()
        };
        r.result.status = status;
        r.result.metadata.code = Some(code.into());
        assert!(store.append(commit, at()).is_err());
    }
}
#[test]
fn d9_rejects_v1_unknown_effects() {
    let (_temp, root, id) = fixture("interrupted-execution");
    let mut store = JournalStore::open(root, &id).unwrap();
    assert!(
        store.snapshot().unresolved_invocations()[0]
            .intent
            .effects
            .is_none()
    );
    let snapshot = store.snapshot();
    store
        .append(
            transition(&snapshot, snapshot.state(), RuntimeState::Recovering),
            at(),
        )
        .unwrap();
    assert!(store.append(result(&store.snapshot()), at()).is_err());
}
