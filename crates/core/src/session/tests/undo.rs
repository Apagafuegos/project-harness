//! ADR 0032 storage rules: the exact undo commit shapes, the state table,
//! branch-derived consumption, the compaction barrier and artifact retention.

use super::*;

/// A plan artifact that agrees with one captured write, and its pre-image.
struct Capture {
    plan: ArtifactRef,
    pre: Option<ArtifactRef>,
}

fn capture(root: &DataRoot, path: &str, pre: Option<&[u8]>, post: &[u8]) -> Capture {
    let pre = pre.map(|bytes| root.put_artifact(bytes).unwrap());
    let bytes = serde_json::to_vec(&json!({
        "version": 2,
        "name": "write",
        "workspace": "/workspace",
        "target": {
            "path": path,
            "pre_image": match &pre {
                Some(reference) => json!(reference.sha256),
                None => json!("absent"),
            },
            "content_sha256": Digest::of(post),
        },
    }))
    .unwrap();
    Capture {
        plan: root.put_artifact(&bytes).unwrap(),
        pre,
    }
}

fn image(path: &str, capture: &Capture, pre_mode: Option<u32>, post: &[u8]) -> FileImage {
    FileImage {
        path: path.into(),
        pre_image: capture.pre.clone(),
        pre_mode,
        post_image: Digest::of(post),
    }
}

/// One identity and the commit that records it, so a test can chain commits.
struct Writes {
    call: Option<CallRef>,
    invocation: Option<InvocationId>,
    step: usize,
    plan: ArtifactRef,
    pre: Option<ArtifactRef>,
    path: &'static str,
    post: &'static [u8],
    capture: Option<FileImage>,
}
impl Writes {
    /// A completed turn with one captured write. `commits` covers the twelve
    /// commits of the turn, exactly as `Workload::ToolTurns` does for a read.
    fn next(&mut self, snapshot: &SessionSnapshot) -> Commit {
        use RuntimeState::*;
        let step = self.step;
        self.step = (step + 1) % TOOL_TURN_COMMITS;
        match step {
            0 => start_turn(snapshot, "write it"),
            1 | 9 => transition(snapshot, Preparing, Inferring),
            2 => {
                let call_id = CallId::new();
                let commit = response_commit(
                    snapshot,
                    vec![ContentBlock::ToolCall {
                        call_id: call_id.clone(),
                        name: "write".into(),
                        arguments: serde_json::Map::new(),
                        provider_call_id: None,
                    }],
                    FinishReason::ToolCalls,
                );
                self.call = Some(CallRef {
                    response_entry_id: commit.entries[0].id.clone(),
                    call_id,
                });
                commit
            }
            3 => transition(snapshot, Processing, Authorizing),
            4 => {
                let turn_id = snapshot.active_turn().unwrap().clone();
                let call = self.call.clone().unwrap();
                let decision_id = DecisionId::new();
                let invocation_id = InvocationId::new();
                self.invocation = Some(invocation_id.clone());
                self.capture = Some(image(
                    self.path,
                    &Capture {
                        plan: self.plan.clone(),
                        pre: self.pre.clone(),
                    },
                    self.pre.as_ref().map(|_| 0o644),
                    self.post,
                ));
                audit(
                    snapshot,
                    vec![
                        JournalEvent::AuthorizationDecided(AuthorizationDecided {
                            turn_id: turn_id.clone(),
                            decision_id: decision_id.clone(),
                            call: call.clone(),
                            approval_id: None,
                            plan_digest: self.plan.sha256.clone(),
                            plan: self.plan.clone(),
                            policy_revision: snapshot.policy_revision().into(),
                            decision: Decision::Allow,
                            reason: "test allow".into(),
                        }),
                        JournalEvent::ExecutionIntent(ExecutionIntent {
                            turn_id,
                            invocation_id,
                            call,
                            decision_id,
                            plan_digest: self.plan.sha256.clone(),
                            backend: ExecutionBackend::TrustedFs,
                            timeout_ms: 30_000,
                            effects: EffectSet::new([EffectClass::FilesystemMutation]),
                            file_image: self.capture.clone(),
                        }),
                    ],
                )
            }
            5 => transition(snapshot, Authorizing, Executing),
            6 => tool_result_with_effect(
                snapshot,
                self.call.clone().unwrap(),
                self.invocation.clone(),
                ToolStatus::Succeeded,
                Provenance::Executor,
                Some(FileEffect::Applied),
            ),
            7 => transition(snapshot, Executing, Continuing),
            8 => transition(snapshot, Continuing, Preparing),
            10 => response_commit(
                snapshot,
                vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                FinishReason::Stop,
            ),
            _ => audit(
                snapshot,
                vec![JournalEvent::TurnFinished(TurnFinished {
                    turn_id: snapshot.active_turn().unwrap().clone(),
                    outcome: TurnOutcome::Completed,
                    code: None,
                })],
            ),
        }
    }
}

/// A tool result whose v4 effect classification the caller chooses.
fn tool_result_with_effect(
    snapshot: &SessionSnapshot,
    call: CallRef,
    invocation: Option<InvocationId>,
    status: ToolStatus,
    provenance: Provenance,
    effect: Option<FileEffect>,
) -> Commit {
    let turn = snapshot.active_turn().unwrap().clone();
    let entry = EntryId::new();
    let code = if status == ToolStatus::Succeeded
        || matches!(
            provenance,
            Provenance::UserObserved | Provenance::UserAcknowledgedUnknown
        ) {
        None
    } else {
        Some("infrastructure_failure".to_owned())
    };
    Commit {
        command_id: None,
        command_fingerprint: None,
        head: HeadChange {
            from: snapshot.head().cloned(),
            to: Some(entry.clone()),
            reason: HeadReason::Advance,
        },
        entries: vec![Entry {
            id: entry,
            parent_id: snapshot.head().cloned(),
            turn_id: Some(turn),
            payload: EntryPayload::ToolResult(ToolResultEntry {
                result: ToolResult {
                    call,
                    invocation_id: invocation,
                    status,
                    content: vec![ContentBlock::Text {
                        text: "sha256=written".into(),
                    }],
                    metadata: ToolMetadata {
                        exit_code: None,
                        duration_ms: Some(1),
                        output_truncated: false,
                        provenance,
                        artifacts: Vec::new(),
                        code,
                        file_effect: effect,
                    },
                },
            }),
        }],
        events: Vec::new(),
    }
}

/// A session whose only turn is one completed, captured write of `path`.
fn written(
    path: &'static str,
    pre: Option<&[u8]>,
    post: &'static [u8],
) -> (tempfile::TempDir, Arc<DataRoot>, JournalStore) {
    let (temp, root, mut store) = fresh();
    let capture = capture(&root, path, pre, post);
    let mut writes = Writes {
        call: None,
        invocation: None,
        step: 0,
        plan: capture.plan,
        pre: capture.pre,
        path,
        post,
        capture: None,
    };
    for _ in 0..TOOL_TURN_COMMITS {
        let snapshot = store.snapshot();
        let commit = writes.next(&snapshot);
        store.append(commit, at()).unwrap();
    }
    assert_eq!(store.snapshot().state(), RuntimeState::Idle);
    (temp, root, store)
}

/// The exact intent storage derives for the selected turn, as JSON.
fn intent(store: &JournalStore) -> UndoIntent {
    let snapshot = store.snapshot();
    let UndoSelection::Candidate {
        turn_id,
        targets,
        omitted_calls,
    } = snapshot.select_undo().unwrap()
    else {
        panic!("a candidate turn")
    };
    UndoIntent {
        undo_id: UndoId::new(),
        target_turn_id: turn_id,
        anchor_head: snapshot.head().cloned(),
        configuration: snapshot.configuration().clone(),
        policy_revision: snapshot.policy_revision().into(),
        targets,
        omitted_calls,
    }
}

fn intent_commit(snapshot: &SessionSnapshot, intent: UndoIntent) -> Commit {
    audit(snapshot, vec![JournalEvent::UndoIntent(intent)])
}

/// The terminal commit for `intent` with the exact recorded text.
fn terminal(snapshot: &SessionSnapshot, intent: &UndoIntent) -> Commit {
    let note = EntryId::new();
    let rows: Vec<UndoFileOutcome> = intent
        .targets
        .iter()
        .map(|target| UndoFileOutcome {
            path: target.path.clone(),
            outcome: if target.pre_image.is_some() {
                FileOutcome::Restored
            } else {
                FileOutcome::Removed
            },
        })
        .collect();
    let text = crate::session::undo::note_text(
        &intent.target_turn_id,
        &intent.undo_id,
        UndoOutcome::Succeeded,
        &intent.targets,
        &rows,
        &[],
        None,
    );
    Commit {
        command_id: None,
        command_fingerprint: None,
        head: HeadChange {
            from: snapshot.head().cloned(),
            to: Some(note.clone()),
            reason: HeadReason::Advance,
        },
        entries: vec![Entry {
            id: note.clone(),
            parent_id: intent.anchor_head.clone(),
            turn_id: None,
            payload: EntryPayload::UndoNote(UndoNote {
                undo_id: intent.undo_id.clone(),
                target_turn_id: intent.target_turn_id.clone(),
                outcome: UndoOutcome::Succeeded,
                text,
            }),
        }],
        events: vec![JournalEvent::UndoResult(UndoResult {
            undo_id: intent.undo_id.clone(),
            outcome: UndoOutcome::Succeeded,
            file_outcomes: rows,
            note_entry_id: note,
            resolution_note: None,
        })],
    }
}

#[test]
fn an_undo_intent_must_be_exactly_the_selection_storage_derives() {
    let (_temp, _root, store) = written("a", Some(b"before"), b"after");
    let snapshot = store.snapshot();
    let exact = intent(&store);

    // The honest intent is accepted as a retained, intent-only commit.
    let mut store = store;
    store
        .append(intent_commit(&snapshot, exact.clone()), at())
        .unwrap();
    let pending = store.snapshot();
    assert_eq!(
        pending.unresolved_undo().unwrap().intent.undo_id,
        exact.undo_id
    );
    assert!(
        pending.requires_recovery(),
        "an unresolved undo is unfinished work"
    );
    let id = pending.session_id().clone();
    drop(store);
    // Reopening reconstructs the same pending undo without appending anything.
    let reopened = JournalStore::open(DataRoot::open(_root.path(), false).unwrap().into(), &id)
        .unwrap()
        .snapshot();
    assert_eq!(reopened.sequence(), pending.sequence());
    assert_eq!(
        reopened.unresolved_undo().unwrap().intent.undo_id,
        exact.undo_id
    );

    for (name, mutate) in [
        (
            "wrong turn",
            Box::new(|intent: &mut UndoIntent| intent.target_turn_id = TurnId::new())
                as Box<dyn Fn(&mut UndoIntent)>,
        ),
        (
            "wrong configuration",
            Box::new(|intent: &mut UndoIntent| intent.configuration.sha256 = Digest::of(b"other")),
        ),
        (
            "wrong policy revision",
            Box::new(|intent: &mut UndoIntent| intent.policy_revision = "other".into()),
        ),
        (
            "extra omitted call",
            Box::new(|intent: &mut UndoIntent| {
                intent.omitted_calls.push(CallRef {
                    response_entry_id: EntryId::new(),
                    call_id: CallId::new(),
                })
            }),
        ),
        (
            "wrong anchor",
            Box::new(|intent: &mut UndoIntent| intent.anchor_head = None),
        ),
        (
            "no targets",
            Box::new(|intent: &mut UndoIntent| intent.targets.clear()),
        ),
    ] {
        let (_temp, _root, mut store) = written("a", Some(b"before"), b"after");
        let snapshot = store.snapshot();
        let mut attempt = intent(&store);
        mutate(&mut attempt);
        assert!(
            matches!(
                store.append(intent_commit(&snapshot, attempt), at()),
                Err(StoreError::Invalid("undo_binding"))
            ),
            "{name}"
        );
    }
}

#[test]
fn an_intent_whose_targets_disagree_with_the_recorded_images_is_refused() {
    let (_temp, _root, mut store) = written("a", Some(b"before"), b"after");
    let snapshot = store.snapshot();
    // The selection derives the post-image from the recorded result.
    let mut attempt = intent(&store);
    assert_eq!(attempt.targets[0].post_image, Digest::of(b"after"));
    attempt.targets[0].post_image = Digest::of(b"something else");
    assert!(matches!(
        store.append(intent_commit(&snapshot, attempt), at()),
        Err(StoreError::Invalid("undo_binding"))
    ));
    // The pre-image must be the recorded capture, not another artifact.
    let other = _root.put_artifact(b"other bytes").unwrap();
    let mut attempt = intent(&store);
    attempt.targets[0].pre_image = Some(other);
    assert!(matches!(
        store.append(intent_commit(&snapshot, attempt), at()),
        Err(StoreError::Invalid("undo_binding"))
    ));
}

#[test]
fn the_terminal_note_and_result_are_one_atomic_exact_pair() {
    let (_temp, _root, mut store) = written("a", Some(b"before"), b"after");
    // A note with no intent is not evidence.
    let snapshot = store.snapshot();
    let orphan = terminal(
        &snapshot,
        &UndoIntent {
            undo_id: UndoId::new(),
            target_turn_id: TurnId::new(),
            anchor_head: snapshot.head().cloned(),
            configuration: snapshot.configuration().clone(),
            policy_revision: snapshot.policy_revision().into(),
            targets: Vec::new(),
            omitted_calls: Vec::new(),
        },
    );
    assert!(matches!(
        store.append(orphan, at()),
        Err(StoreError::Invalid("undo_result_binding"))
    ));

    let snapshot = store.snapshot();
    let exact = intent(&store);
    store
        .append(intent_commit(&snapshot, exact.clone()), at())
        .unwrap();

    // A result without its note is a mixed commit.
    let snapshot = store.snapshot();
    let mut broken = terminal(&snapshot, &exact);
    broken.entries.pop().unwrap();
    broken.head.to = snapshot.head().cloned();
    broken.head.reason = HeadReason::Retain;
    assert!(matches!(
        store.append(broken, at()),
        Err(StoreError::Invalid("undo_commit_shape"))
    ));

    // A note committed without its result event is a mixed shape.
    let snapshot = store.snapshot();
    let mut note_only = terminal(&snapshot, &exact);
    note_only.events.clear();
    assert!(matches!(
        store.append(note_only, at()),
        Err(StoreError::Invalid("undo_commit_shape"))
    ));

    // A note whose text is not the deterministic text is refused.
    let snapshot = store.snapshot();
    let mut wrong = terminal(&snapshot, &exact);
    if let EntryPayload::UndoNote(note) = &mut wrong.entries[0].payload {
        note.text.push_str("extra\n");
    }
    assert!(matches!(
        store.append(wrong, at()),
        Err(StoreError::Invalid("undo_result_binding"))
    ));

    // An advanced note must continue the head: a note anywhere else is not a
    // commit shape this storage accepts.
    let snapshot = store.snapshot();
    let mut wrong = terminal(&snapshot, &exact);
    wrong.entries[0].parent_id = None;
    assert!(matches!(
        store.append(wrong, at()),
        Err(StoreError::Invalid("nonlinear_advance"))
    ));
}

#[test]
fn an_unresolved_undo_blocks_turns_configuration_and_overlapping_work() {
    let (_temp, _root, mut store) = written("a", Some(b"before"), b"after");
    let snapshot = store.snapshot();
    let exact = intent(&store);
    store
        .append(intent_commit(&snapshot, exact.clone()), at())
        .unwrap();
    let blocked = store.snapshot();

    // Both the opening commit's entries and its `turn_started` require idle
    // work; a pending undo is unfinished work, so the first check refuses.
    let refused = store.append(start_turn(&blocked, "next"), at());
    assert!(
        matches!(refused, Err(StoreError::Invalid("session_not_idle"))),
        "{refused:?}"
    );
    assert!(matches!(
        store.append(
            audit(
                &blocked,
                vec![JournalEvent::ConfigurationChanged(ConfigurationChanged {
                    configuration: blocked.configuration().clone(),
                    policy_revision: "other".into(),
                })],
            ),
            at(),
        ),
        Err(StoreError::Invalid("configuration_inside_turn"))
    ));
    assert!(matches!(
        store.append(navigate(&blocked, None), at()),
        Err(StoreError::Invalid("session_not_idle"))
    ));
    // A second intent cannot overlap the first.
    let mut second = exact.clone();
    second.undo_id = UndoId::new();
    assert!(matches!(
        store.append(intent_commit(&blocked, second), at()),
        Err(StoreError::Invalid("session_not_idle"))
    ));
}

#[test]
fn a_note_consumes_its_turn_on_its_branch_only() {
    let (_temp, _root, mut store) = written("a", Some(b"before"), b"after");
    let snapshot = store.snapshot();
    let candidate = intent(&store);
    let sibling = snapshot.head().cloned();
    store
        .append(intent_commit(&snapshot, candidate.clone()), at())
        .unwrap();
    let pending = store.snapshot();
    store.append(terminal(&pending, &candidate), at()).unwrap();
    let done = store.snapshot();
    // The consumed turn is no longer offered on this ancestry.
    assert!(matches!(
        done.select_undo().unwrap(),
        UndoSelection::Nothing { .. }
    ));
    // Navigating back before the note removes the consumption: the note is no
    // longer ancestral, and the turn is offered again.
    let back = store.snapshot();
    store
        .append(navigate(&back, sibling.clone()), at())
        .unwrap();
    assert!(matches!(
        store.snapshot().select_undo().unwrap(),
        UndoSelection::Candidate { .. }
    ));
}

#[test]
fn a_compaction_may_not_cover_an_undo_note_or_anything_before_it() {
    let (_temp, _root, mut store) = written("a", Some(b"before"), b"after");
    let snapshot = store.snapshot();
    let candidate = intent(&store);
    store
        .append(intent_commit(&snapshot, candidate.clone()), at())
        .unwrap();
    let pending = store.snapshot();
    store.append(terminal(&pending, &candidate), at()).unwrap();
    // A second turn follows so a compaction has eligible dialogue.
    let mut writes = Writes {
        call: None,
        invocation: None,
        step: 0,
        plan: capture(
            std::sync::Arc::new(DataRoot::open(_root.path(), false).unwrap()).as_ref(),
            "b",
            None,
            b"created",
        )
        .plan,
        pre: None,
        path: "b",
        post: b"created",
        capture: None,
    };
    for _ in 0..TOOL_TURN_COMMITS {
        let snapshot = store.snapshot();
        let commit = writes.next(&snapshot);
        store.append(commit, at()).unwrap();
    }
    let snapshot = store.snapshot();
    let ancestry = snapshot.ancestry(snapshot.head()).unwrap();
    // Everything up to and including the note.
    let through_note: Vec<EntryId> = ancestry
        .iter()
        .take_while(|stored| !matches!(stored.entry.payload, EntryPayload::UndoNote(_)))
        .map(|stored| stored.entry.id.clone())
        .chain(
            ancestry
                .iter()
                .find(|stored| matches!(stored.entry.payload, EntryPayload::UndoNote(_)))
                .map(|stored| stored.entry.id.clone()),
        )
        .collect();
    let compaction = |covered: Vec<EntryId>| {
        let manifest = crate::session::compaction_manifest(
            &snapshot,
            &covered,
            &[],
            &snapshot.configuration().sha256,
        )
        .unwrap();
        let reference = _root.put_artifact(&manifest).unwrap();
        Compaction {
            strategy: "rolling_summary".into(),
            strategy_version: "1".into(),
            covered_entry_ids: covered,
            source_fingerprint: reference.sha256.clone(),
            source_manifest: reference,
            supersedes: Vec::new(),
            summary: "summary".into(),
            model: None,
            configuration_fingerprint: snapshot.configuration().sha256.clone(),
        }
    };
    let entry = |compaction: Compaction| Entry {
        id: EntryId::new(),
        parent_id: snapshot.head().cloned(),
        turn_id: None,
        payload: EntryPayload::Compaction(compaction),
    };
    let entry_id = entry(compaction(through_note.clone())).id.clone();
    let covered = entry(compaction(through_note));
    store
        .append(
            Commit {
                command_id: None,
                command_fingerprint: None,
                head: HeadChange {
                    from: snapshot.head().cloned(),
                    to: Some(entry_id),
                    reason: HeadReason::Advance,
                },
                entries: vec![covered],
                events: Vec::new(),
            },
            at(),
        )
        .expect_err("a compaction may not cover an undo note");
}

#[test]
fn a_missing_pre_image_artifact_blocks_replay() {
    let (_temp, root, mut store) = written("a", Some(b"before"), b"after");
    let snapshot = store.snapshot();
    let exact = intent(&store);
    store
        .append(intent_commit(&snapshot, exact.clone()), at())
        .unwrap();
    let pending = store.snapshot();
    store.append(terminal(&pending, &exact), at()).unwrap();
    let id = store.snapshot().session_id().clone();
    drop(store);
    // Remove the pre-image bytes; replay must fail closed, not read as absent.
    let pre = exact.targets[0].pre_image.as_ref().unwrap();
    fs::remove_file(root.path().join("artifacts").join(pre.sha256.as_str())).unwrap();
    let inspection = JournalStore::inspect(&root, &id).unwrap();
    let diagnostic = inspection.diagnostic.expect("a diagnostic");
    assert_eq!(diagnostic.kind, DiagnosticKind::Artifact);
    assert!(matches!(
        JournalStore::open(root, &id),
        Err(StoreError::Journal(_))
    ));
}

#[test]
fn a_plan_that_disagrees_with_the_captured_image_is_rejected() {
    let (_temp, root, mut store) = fresh();
    store
        .append(start_turn(&store.snapshot(), "write it"), at())
        .unwrap();
    store
        .append(
            transition(
                &store.snapshot(),
                RuntimeState::Preparing,
                RuntimeState::Inferring,
            ),
            at(),
        )
        .unwrap();
    let call_id = CallId::new();
    let response = response_commit(
        &store.snapshot(),
        vec![ContentBlock::ToolCall {
            call_id: call_id.clone(),
            name: "write".into(),
            arguments: serde_json::Map::new(),
            provider_call_id: None,
        }],
        FinishReason::ToolCalls,
    );
    let call = CallRef {
        response_entry_id: response.entries[0].id.clone(),
        call_id,
    };
    store.append(response, at()).unwrap();
    store
        .append(
            transition(
                &store.snapshot(),
                RuntimeState::Processing,
                RuntimeState::Authorizing,
            ),
            at(),
        )
        .unwrap();

    let snapshot = store.snapshot();
    let turn_id = snapshot.active_turn().unwrap().clone();
    // The plan names other pre-image bytes than the image captured.
    let mismatched = capture(&root, "a", Some(b"different"), b"after");
    let agreed = capture(&root, "a", Some(b"before"), b"after");
    let decision_id = DecisionId::new();
    let commit = |plan: ArtifactRef, pre: Option<ArtifactRef>| {
        audit(
            &snapshot,
            vec![
                JournalEvent::AuthorizationDecided(AuthorizationDecided {
                    turn_id: turn_id.clone(),
                    decision_id: decision_id.clone(),
                    call: call.clone(),
                    approval_id: None,
                    plan_digest: plan.sha256.clone(),
                    plan: plan.clone(),
                    policy_revision: snapshot.policy_revision().into(),
                    decision: Decision::Allow,
                    reason: "test allow".into(),
                }),
                JournalEvent::ExecutionIntent(ExecutionIntent {
                    turn_id: turn_id.clone(),
                    invocation_id: InvocationId::new(),
                    call: call.clone(),
                    decision_id: decision_id.clone(),
                    plan_digest: plan.sha256.clone(),
                    backend: ExecutionBackend::TrustedFs,
                    timeout_ms: 1_000,
                    effects: EffectSet::new([EffectClass::FilesystemMutation]),
                    file_image: Some(FileImage {
                        path: "a".into(),
                        pre_image: pre,
                        pre_mode: Some(0o644),
                        post_image: Digest::of(b"after"),
                    }),
                }),
            ],
        )
    };
    let refused = store.append(commit(mismatched.plan, agreed.pre.clone()), at());
    assert!(
        matches!(refused, Err(StoreError::Invalid("undo_image_binding"))),
        "{refused:?}"
    );
    // The same intent over its own captured bytes is accepted.
    store.append(commit(agreed.plan, agreed.pre), at()).unwrap();
}

#[test]
fn pre_images_survive_an_undo_and_a_collection() {
    let (_temp, root, mut store) = written("a", Some(b"before"), b"after");
    let snapshot = store.snapshot();
    let exact = intent(&store);
    let pre = exact.targets[0].pre_image.clone().unwrap();
    store
        .append(intent_commit(&snapshot, exact.clone()), at())
        .unwrap();
    let pending = store.snapshot();
    store.append(terminal(&pending, &exact), at()).unwrap();
    let id = store.snapshot().session_id().clone();
    drop(store);
    // An undo never releases the pre-image: the journal still references it.
    let report = root
        .collect_artifacts(&GcOptions {
            dry_run: false,
            grace: std::time::Duration::ZERO,
        })
        .unwrap();
    assert!(
        !report.candidates.iter().any(|c| c.sha256 == pre.sha256),
        "a referenced pre-image is never collected"
    );
    root.verify_artifact(&pre).unwrap();
    // Deleting the journal leaves the bytes for another branch or session.
    root.delete_session(&id).unwrap();
    root.verify_artifact(&pre).unwrap();
}
