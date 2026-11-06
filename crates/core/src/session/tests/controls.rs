//! Durable whole-turn context controls: every storage rule and its code, the
//! two compaction rules, and replay equality against the scanning oracle.

use super::*;

/// Append one completed plain turn and return its identity.
fn completed_turn(store: &mut JournalStore, text: &str) -> TurnId {
    let commit = start_turn(&store.snapshot(), text);
    let turn = commit.entries[0].turn_id.clone().unwrap();
    store.append(commit, at()).unwrap();
    response(
        store,
        vec![ContentBlock::Text { text: "ok".into() }],
        FinishReason::Stop,
    );
    let snapshot = store.snapshot();
    store
        .append(
            audit(
                &snapshot,
                vec![JournalEvent::TurnFinished(TurnFinished {
                    turn_id: turn.clone(),
                    outcome: TurnOutcome::Completed,
                    code: None,
                })],
            ),
            at(),
        )
        .unwrap();
    turn
}

/// `turns` completed turns on one branch.
fn turns(store: &mut JournalStore, turns: usize) -> Vec<TurnId> {
    (0..turns)
        .map(|index| completed_turn(store, &format!("turn {index}")))
        .collect()
}

/// One `context_control` appended at the current head.
fn control(snapshot: &SessionSnapshot, turn: &TurnId, mode: ContextMode) -> Commit {
    let entry_id = EntryId::new();
    Commit {
        command_id: None,
        command_fingerprint: None,
        head: HeadChange {
            from: snapshot.head().cloned(),
            to: Some(entry_id.clone()),
            reason: HeadReason::Advance,
        },
        entries: vec![Entry {
            id: entry_id,
            parent_id: snapshot.head().cloned(),
            turn_id: None,
            payload: EntryPayload::ContextControl(ContextControl {
                turn_id: turn.clone(),
                mode,
            }),
        }],
        events: vec![],
    }
}

fn accepted(store: &mut JournalStore, turn: &TurnId, mode: ContextMode) {
    store
        .append(control(&store.snapshot(), turn, mode), at())
        .unwrap();
}

/// Assert storage refuses this control with `code` and writes nothing.
fn rejected(store: &mut JournalStore, turn: &TurnId, mode: ContextMode, code: &str) {
    let before = store.snapshot().checksum().clone();
    let commit = control(&store.snapshot(), turn, mode);
    assert!(
        matches!(store.append(commit, at()), Err(StoreError::Invalid(c)) if c == code),
        "expected {code}"
    );
    assert_eq!(
        store.snapshot().checksum(),
        &before,
        "{code}: nothing written"
    );
}

/// The effective mode at the head, probed through the shared rule: a control
/// that changes nothing is refused as a no-op.
fn mode(snapshot: &SessionSnapshot, turn: &TurnId) -> ContextMode {
    for mode in [ContextMode::Pinned, ContextMode::Excluded] {
        if snapshot
            .control_error(snapshot.head(), turn, mode)
            .is_err_and(|code| code == "context_control_noop")
        {
            return mode;
        }
    }
    ContextMode::Default
}

/// Navigate to `head`, which must be an idle-safe boundary.
fn navigate_to(store: &mut JournalStore, head: Option<EntryId>) {
    let snapshot = store.snapshot();
    store.append(navigate(&snapshot, head), at()).unwrap();
}

/// The branch entries of `turn`, in ancestral order.
fn turn_entries(snapshot: &SessionSnapshot, turn: &TurnId) -> Vec<EntryId> {
    snapshot
        .ancestry(snapshot.head())
        .unwrap()
        .iter()
        .filter(|stored| stored.entry.turn_id.as_ref() == Some(turn))
        .map(|stored| stored.entry.id.clone())
        .collect()
}

/// A compaction covering exactly `covered`, with a manifest bound to the
/// current snapshot.
fn compaction(root: &DataRoot, snapshot: &SessionSnapshot, covered: Vec<EntryId>) -> Commit {
    let configuration = snapshot.configuration().sha256.clone();
    let bytes =
        compaction_manifest(snapshot, &covered, &[], &configuration).expect("known entries");
    let manifest = root.put_artifact(&bytes).unwrap();
    let entry_id = EntryId::new();
    Commit {
        command_id: None,
        command_fingerprint: None,
        head: HeadChange {
            from: snapshot.head().cloned(),
            to: Some(entry_id.clone()),
            reason: HeadReason::Advance,
        },
        entries: vec![Entry {
            id: entry_id,
            parent_id: snapshot.head().cloned(),
            turn_id: None,
            payload: EntryPayload::Compaction(Compaction {
                strategy: "test".into(),
                strategy_version: "1".into(),
                covered_entry_ids: covered,
                source_fingerprint: manifest.sha256.clone(),
                source_manifest: manifest,
                supersedes: vec![],
                summary: "summary".into(),
                model: Some(snapshot.header().initial_model.clone()),
                configuration_fingerprint: configuration,
            }),
        }],
        events: vec![],
    }
}

/// The versions of a journal's records, in order.
fn versions(root: &DataRoot, id: &SessionId) -> Vec<u64> {
    records(&fs::read(journal_path(root, id)).unwrap())
        .iter()
        .map(|record| record["version"].as_u64().unwrap())
        .collect()
}

#[test]
fn controls_are_accepted_while_idle_and_are_idempotent() {
    let (_temp, _root, mut store) = fresh();
    let ids = turns(&mut store, 3);
    // A turn with no control is `default`; resetting it is a no-op.
    rejected(
        &mut store,
        &ids[0],
        ContextMode::Default,
        "context_control_noop",
    );
    accepted(&mut store, &ids[0], ContextMode::Pinned);
    // Repeating the same control changes nothing, so it is refused.
    rejected(
        &mut store,
        &ids[0],
        ContextMode::Pinned,
        "context_control_noop",
    );
    accepted(&mut store, &ids[1], ContextMode::Excluded);
    assert_eq!(mode(&store.snapshot(), &ids[0]), ContextMode::Pinned);
    assert_eq!(mode(&store.snapshot(), &ids[1]), ContextMode::Excluded);
    assert_eq!(mode(&store.snapshot(), &ids[2]), ContextMode::Default);
    // Undo is another control, never a rewrite: history keeps every entry.
    accepted(&mut store, &ids[0], ContextMode::Default);
    assert_eq!(mode(&store.snapshot(), &ids[0]), ContextMode::Default);
    let snapshot = store.snapshot();
    assert_eq!(
        snapshot
            .entries()
            .filter(|(_, stored)| matches!(stored.entry.payload, EntryPayload::ContextControl(_)))
            .count(),
        3,
        "every control stays in history"
    );
    // The last control for a turn wins: exclusion then pinning is a pin.
    accepted(&mut store, &ids[1], ContextMode::Pinned);
    assert_eq!(mode(&store.snapshot(), &ids[1]), ContextMode::Pinned);
}

#[test]
fn a_control_is_refused_inside_a_turn_and_in_a_turn_opening_commit() {
    let (_temp, _root, mut store) = fresh();
    let ids = turns(&mut store, 1);
    // While a turn is open, the session is not idle.
    store
        .append(start_turn(&store.snapshot(), "later turn"), at())
        .unwrap();
    rejected(&mut store, &ids[0], ContextMode::Pinned, "session_not_idle");

    // A control in the commit that starts a turn is refused even though the
    // active turn is only installed by that commit's event.
    let (_temp, _root, mut store) = fresh();
    let mut opening = start_turn(&store.snapshot(), "opening");
    let target = opening.entries[0].turn_id.clone().unwrap();
    let control_id = EntryId::new();
    opening.entries.push(Entry {
        id: control_id.clone(),
        parent_id: Some(opening.entries[0].id.clone()),
        turn_id: None,
        payload: EntryPayload::ContextControl(ContextControl {
            turn_id: target,
            mode: ContextMode::Pinned,
        }),
    });
    opening.head.to = Some(control_id);
    assert!(matches!(
        store.append(opening, at()),
        Err(StoreError::Invalid("context_control_inside_turn"))
    ));
}

#[test]
fn a_control_is_refused_inside_a_tool_exchange() {
    // An exchange with an unanswered call always belongs to an open turn, so
    // the idle rule answers first; the branch walk's own `head_inside_tool_exchange`
    // check stays unreachable defense. `session_not_idle` is the observable
    // refusal, exactly as for `model_changed`.
    let (_temp, root, mut store) = fresh();
    append_workload(&root, &mut store, Workload::ToolTurns, 3);
    let snapshot = store.snapshot();
    assert!(!snapshot.0.pending(snapshot.head()).unwrap().is_empty());
    let turn = snapshot.active_turn().unwrap().clone();
    rejected(&mut store, &turn, ContextMode::Pinned, "session_not_idle");
}

#[test]
fn a_control_target_must_be_a_finished_turn_on_this_ancestry() {
    let (_temp, _root, mut store) = fresh();
    // Unknown turns are not finished turns.
    rejected(
        &mut store,
        &TurnId::new(),
        ContextMode::Pinned,
        "context_control_unfinished_turn",
    );
    // An interrupted turn is finished, so it can be controlled.
    let started = start_turn(&store.snapshot(), "interrupted");
    let interrupted = started.entries[0].turn_id.clone().unwrap();
    store.append(started, at()).unwrap();
    let snapshot = store.snapshot();
    store
        .append(
            audit(
                &snapshot,
                vec![JournalEvent::TurnFinished(TurnFinished {
                    turn_id: interrupted.clone(),
                    outcome: TurnOutcome::Interrupted,
                    code: Some("process_restart".into()),
                })],
            ),
            at(),
        )
        .unwrap();
    accepted(&mut store, &interrupted, ContextMode::Pinned);
    // A turn whose entries are not on the selected ancestry cannot be named.
    navigate_to(&mut store, None);
    rejected(
        &mut store,
        &interrupted,
        ContextMode::Excluded,
        "context_control_not_ancestral",
    );
}

#[test]
fn a_control_on_a_sibling_branch_does_not_apply_to_this_one() {
    let (_temp, _root, mut store) = fresh();
    let ids = turns(&mut store, 1);
    let first = turn_entries(&store.snapshot(), &ids[0]);
    let (user, response) = (first[0].clone(), first[1].clone());
    // Fork a second branch from the shared user entry and complete a turn on it.
    navigate_to(&mut store, Some(user));
    let second = completed_turn(&mut store, "sibling");
    accepted(&mut store, &ids[0], ContextMode::Pinned);
    assert_eq!(mode(&store.snapshot(), &ids[0]), ContextMode::Pinned);
    // The first branch, which forks before that control, does not see it.
    navigate_to(&mut store, Some(response));
    assert_eq!(mode(&store.snapshot(), &ids[0]), ContextMode::Default);
    // The sibling's turn is not on this ancestry at all.
    rejected(
        &mut store,
        &second,
        ContextMode::Pinned,
        "context_control_not_ancestral",
    );
    // And the first branch accepts its own pin, which the sibling cannot see.
    accepted(&mut store, &ids[0], ContextMode::Excluded);
    assert_eq!(mode(&store.snapshot(), &ids[0]), ContextMode::Excluded);
}

#[test]
fn pinning_or_excluding_a_covered_turn_is_refused() {
    let (_temp, root, session_id) = fixture("compaction");
    let mut store = JournalStore::open(root, &session_id).unwrap();
    let snapshot = store.snapshot();
    let covered = snapshot
        .entry(&"019a0000-0000-7000-8000-0000000001f5".parse().unwrap())
        .unwrap()
        .entry
        .turn_id
        .clone()
        .unwrap();
    rejected(
        &mut store,
        &covered,
        ContextMode::Pinned,
        "context_control_covered",
    );
    rejected(
        &mut store,
        &covered,
        ContextMode::Excluded,
        "context_control_covered",
    );
}

#[test]
fn storage_refuses_a_compaction_covering_a_pinned_turn() {
    // Without a control the same coverage is accepted, so the refusal is the
    // pin's and not a property of the span.
    let (_temp, root, mut store) = fresh();
    let ids = turns(&mut store, 4);
    let covered = turn_entries(&store.snapshot(), &ids[0]);
    let commit = compaction(&root, &store.snapshot(), covered);
    store.append(commit, at()).unwrap();

    let (_temp, root, mut store) = fresh();
    let ids = turns(&mut store, 4);
    accepted(&mut store, &ids[0], ContextMode::Pinned);
    let covered = turn_entries(&store.snapshot(), &ids[0]);
    let commit = compaction(&root, &store.snapshot(), covered);
    assert!(matches!(
        store.append(commit, at()),
        Err(StoreError::Invalid("compaction_covers_pinned_turn"))
    ));
    // A pinned turn that is not covered does not block the compaction.
    let (_temp, root, mut store) = fresh();
    let ids = turns(&mut store, 4);
    accepted(&mut store, &ids[3], ContextMode::Pinned);
    let covered = turn_entries(&store.snapshot(), &ids[0]);
    let commit = compaction(&root, &store.snapshot(), covered);
    store.append(commit, at()).unwrap();
}

#[test]
fn coverage_may_skip_a_controlled_turn() {
    // Decision 14 (m6-02): the covered IDs are an ancestral, ordered
    // subsequence whose gap is a whole turn the user excluded here.
    let (_temp, root, mut store) = fresh();
    let ids = turns(&mut store, 6);
    accepted(&mut store, &ids[1], ContextMode::Excluded);
    let snapshot = store.snapshot();
    let covered: Vec<EntryId> = turn_entries(&snapshot, &ids[0])
        .into_iter()
        .chain(turn_entries(&snapshot, &ids[2]))
        .collect();
    let commit = compaction(&root, &snapshot, covered);
    store.append(commit, at()).unwrap();
    let snapshot = store.snapshot();
    let EntryPayload::Compaction(stored) = &snapshot
        .entry(snapshot.head().unwrap())
        .unwrap()
        .entry
        .payload
    else {
        panic!("the compaction is the head")
    };
    assert_eq!(stored.covered_entry_ids.len(), 4, "two whole turns");
    assert!(
        turn_entries(&snapshot, &ids[1])
            .iter()
            .all(|id| !stored.covered_entry_ids.contains(id))
    );
}

#[test]
fn coverage_refuses_a_gap_of_an_uncontrolled_turn() {
    let (_temp, root, mut store) = fresh();
    let ids = turns(&mut store, 6);
    let snapshot = store.snapshot();
    let covered: Vec<EntryId> = turn_entries(&snapshot, &ids[0])
        .into_iter()
        .chain(turn_entries(&snapshot, &ids[2]))
        .collect();
    let commit = compaction(&root, &snapshot, covered);
    assert!(matches!(
        store.append(commit, at()),
        Err(StoreError::Invalid("compaction_not_contiguous"))
    ));
}

#[test]
fn coverage_refuses_a_partially_covered_turn() {
    let (_temp, root, mut store) = fresh();
    let ids = turns(&mut store, 6);
    let snapshot = store.snapshot();
    let mut covered = turn_entries(&snapshot, &ids[0]);
    covered.pop();
    let commit = compaction(&root, &snapshot, covered);
    assert!(matches!(
        store.append(commit, at()),
        Err(StoreError::Invalid("compaction_splits_turn"))
    ));
}

#[test]
fn coverage_refuses_an_excluded_turn() {
    // Without the control the same coverage is accepted, so the refusal is the
    // exclusion's and not a property of the span.
    let (_temp, root, mut store) = fresh();
    let ids = turns(&mut store, 6);
    let snapshot = store.snapshot();
    let covered: Vec<EntryId> = turn_entries(&snapshot, &ids[0])
        .into_iter()
        .chain(turn_entries(&snapshot, &ids[1]))
        .collect();
    let commit = compaction(&root, &snapshot, covered);
    store.append(commit, at()).unwrap();

    let (_temp, root, mut store) = fresh();
    let ids = turns(&mut store, 6);
    accepted(&mut store, &ids[1], ContextMode::Excluded);
    let snapshot = store.snapshot();
    let covered: Vec<EntryId> = turn_entries(&snapshot, &ids[0])
        .into_iter()
        .chain(turn_entries(&snapshot, &ids[1]))
        .collect();
    let commit = compaction(&root, &snapshot, covered);
    assert!(matches!(
        store.append(commit, at()),
        Err(StoreError::Invalid("compaction_covers_excluded_turn"))
    ));
}

#[test]
fn coverage_refuses_a_wrong_order() {
    let (_temp, root, mut store) = fresh();
    let ids = turns(&mut store, 6);
    let snapshot = store.snapshot();
    let mut covered = turn_entries(&snapshot, &ids[1]);
    covered.extend(turn_entries(&snapshot, &ids[0]));
    let commit = compaction(&root, &snapshot, covered);
    assert!(matches!(
        store.append(commit, at()),
        Err(StoreError::Invalid("compaction_not_contiguous"))
    ));
}

#[test]
fn a_v3_compaction_may_omit_its_model() {
    let (_temp, root, mut store) = fresh();
    let ids = turns(&mut store, 4);
    let covered = turn_entries(&store.snapshot(), &ids[0]);
    let mut commit = compaction(&root, &store.snapshot(), covered);
    let EntryPayload::Compaction(compaction) = &mut commit.entries[0].payload else {
        unreachable!()
    };
    compaction.model = None;
    store.append(commit, at()).unwrap();
    let session = store.snapshot().session_id().clone();
    drop(store);
    // The field is absent, never null: absent means no model was used.
    let values = records(&fs::read(journal_path(&root, &session)).unwrap());
    let last = &values[values.len() - 1]["payload"]["entries"][0]["payload"];
    assert_eq!(last["kind"], "compaction");
    assert!(last.get("model").is_none(), "{last}");
    let reopened = JournalStore::open(root, &session).unwrap().snapshot();
    let EntryPayload::Compaction(stored) = &reopened
        .entry(reopened.head().unwrap())
        .unwrap()
        .entry
        .payload
    else {
        panic!("the compaction is the head")
    };
    assert_eq!(stored.model, None);
}

#[test]
fn a_v2_prefix_continues_as_v5() {
    let (_temp, root, mut store) = fresh();
    let ids = turns(&mut store, 2);
    let session = store.snapshot().session_id().clone();
    drop(store);
    // Every record so far is also a valid v2 shape: no v3-only field is used.
    let path = journal_path(&root, &session);
    let mut values = records(&fs::read(&path).unwrap());
    let prefix = values.len() as u64;
    for value in &mut values {
        value["version"] = json!(2);
    }
    fs::write(&path, reframe(&values)).unwrap();
    let mut store = JournalStore::open(root.clone(), &session).unwrap();
    accepted(&mut store, &ids[0], ContextMode::Pinned);
    let mut expected = vec![2; prefix as usize];
    expected.push(4);
    assert_eq!(versions(&root, &session), expected);
    drop(store);
    let inspection = JournalStore::inspect(&root, &session).unwrap();
    assert_eq!(inspection.diagnostic, None);
    assert_eq!(inspection.versions, [(2, prefix), (5, 1)].into());
}

#[test]
fn replay_rebuilds_effective_controls_exactly() {
    let (_temp, root, mut store) = fresh();
    let ids = turns(&mut store, 3);
    accepted(&mut store, &ids[0], ContextMode::Pinned);
    accepted(&mut store, &ids[1], ContextMode::Excluded);
    accepted(&mut store, &ids[1], ContextMode::Pinned);
    accepted(&mut store, &ids[2], ContextMode::Excluded);
    let live = store.snapshot();
    let session = live.session_id().clone();
    drop(store);
    let reopened = JournalStore::open(root, &session).unwrap().snapshot();
    // The test-build oracle already compared every step with the scanning
    // walk; a reopen must rebuild the same facts from history alone.
    assert_eq!(reopened.0.derived(), live.0.derived());
    assert_eq!(mode(&reopened, &ids[0]), ContextMode::Pinned);
    assert_eq!(mode(&reopened, &ids[1]), ContextMode::Pinned);
    assert_eq!(mode(&reopened, &ids[2]), ContextMode::Excluded);
}
