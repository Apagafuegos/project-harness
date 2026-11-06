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
        additional_targets: Vec::new(),
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
    effect: FileEffect,
    file_paths: Vec<FilePathEffect>,
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
                if self.capture.is_none() {
                    self.capture = Some(image(
                        self.path,
                        &Capture {
                            plan: self.plan.clone(),
                            pre: self.pre.clone(),
                        },
                        self.pre.as_ref().map(|_| 0o644),
                        self.post,
                    ));
                }
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
            6 => tool_result_with_paths(
                snapshot,
                self.call.clone().unwrap(),
                self.invocation.clone(),
                ToolStatus::Succeeded,
                Provenance::Executor,
                Some(self.effect),
                self.file_paths.clone(),
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
    tool_result_with_paths(
        snapshot,
        call,
        invocation,
        status,
        provenance,
        effect,
        Vec::new(),
    )
}

fn tool_result_with_paths(
    snapshot: &SessionSnapshot,
    call: CallRef,
    invocation: Option<InvocationId>,
    status: ToolStatus,
    provenance: Provenance,
    effect: Option<FileEffect>,
    file_paths: Vec<FilePathEffect>,
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
                        file_paths,
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
        effect: FileEffect::Applied,
        file_paths: Vec::new(),
    };
    for _ in 0..TOOL_TURN_COMMITS {
        let snapshot = store.snapshot();
        let commit = writes.next(&snapshot);
        store.append(commit, at()).unwrap();
    }
    assert_eq!(store.snapshot().state(), RuntimeState::Idle);
    (temp, root, store)
}

/// A completed newer turn that proposed `calls` `bash` calls and had every one
/// denied, so it is skipped by selection and has no trusted file mutation.
fn denied_bash_turn(store: &mut JournalStore, calls: usize) {
    store
        .append(start_turn(&store.snapshot(), "run it"), at())
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
    let content: Vec<ContentBlock> = (0..calls)
        .map(|_| ContentBlock::ToolCall {
            call_id: CallId::new(),
            name: "bash".into(),
            arguments: serde_json::Map::new(),
            provider_call_id: None,
        })
        .collect();
    let calls: Vec<CallRef> = {
        let commit = response_commit(&store.snapshot(), content, FinishReason::ToolCalls);
        let response = commit.entries[0].id.clone();
        let call_ids: Vec<CallId> = match &commit.entries[0].payload {
            EntryPayload::AssistantResponse(response) => response
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { call_id, .. } => Some(call_id.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        store.append(commit, at()).unwrap();
        call_ids
            .into_iter()
            .map(|call_id| CallRef {
                response_entry_id: response.clone(),
                call_id,
            })
            .collect()
    };
    for call in calls {
        store
            .append(
                tool_result_with_effect(
                    &store.snapshot(),
                    call,
                    None,
                    ToolStatus::Denied,
                    Provenance::Runtime,
                    None,
                ),
                at(),
            )
            .unwrap();
    }
    for (from, to) in [
        (RuntimeState::Processing, RuntimeState::Continuing),
        (RuntimeState::Continuing, RuntimeState::Preparing),
        (RuntimeState::Preparing, RuntimeState::Inferring),
    ] {
        store
            .append(transition(&store.snapshot(), from, to), at())
            .unwrap();
    }
    store
        .append(
            response_commit(
                &store.snapshot(),
                vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                FinishReason::Stop,
            ),
            at(),
        )
        .unwrap();
    let snapshot = store.snapshot();
    let turn = snapshot.active_turn().unwrap().clone();
    store
        .append(
            audit(
                &snapshot,
                vec![JournalEvent::TurnFinished(TurnFinished {
                    turn_id: turn,
                    outcome: TurnOutcome::Completed,
                    code: None,
                })],
            ),
            at(),
        )
        .unwrap();
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
    let omitted: Vec<(CallRef, String)> = intent
        .omitted_calls
        .iter()
        .map(|call| (call.clone(), "bash".to_owned()))
        .collect();
    let text = crate::session::undo::note_text(
        &intent.target_turn_id,
        &intent.undo_id,
        UndoOutcome::Succeeded,
        &intent.targets,
        &rows,
        &omitted,
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
        effect: FileEffect::Applied,
        file_paths: Vec::new(),
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
                        additional_targets: Vec::new(),
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

/// Decision 17: a candidate's report includes the bash proposals of newer
/// unconsumed turns skipped on the way, and replay accepts either that list or
/// the v0.2.0 one, and nothing else.
#[test]
fn a_candidate_reports_skipped_bash_calls_and_accepts_both_recorded_lists() {
    let (_temp, _root, mut store) = written("a", Some(b"before"), b"after");
    denied_bash_turn(&mut store, 1);
    let snapshot = store.snapshot();
    let UndoSelection::Candidate {
        turn_id,
        omitted_calls,
        ..
    } = store.snapshot().select_undo().unwrap()
    else {
        panic!("the write turn is the candidate")
    };
    assert_eq!(omitted_calls.len(), 1, "the skipped bash call is listed");
    let corrected = intent(&store);
    assert_eq!(corrected.target_turn_id, turn_id);
    assert_eq!(corrected.omitted_calls, omitted_calls);
    store
        .append(intent_commit(&snapshot, corrected.clone()), at())
        .unwrap();

    // The v0.2.0 writer recorded only the target turn's own calls. A journal
    // carrying that list still replays: history is never rewritten.
    let (_temp, _root, mut store) = written("a", Some(b"before"), b"after");
    denied_bash_turn(&mut store, 1);
    let snapshot = store.snapshot();
    let mut legacy = intent(&store);
    assert_eq!(legacy.omitted_calls.len(), 1);
    legacy.omitted_calls.clear();
    store
        .append(intent_commit(&snapshot, legacy), at())
        .expect("the v0.2.0 omission still replays");

    // Any other list is a binding failure.
    let (_temp, _root, mut store) = written("a", Some(b"before"), b"after");
    denied_bash_turn(&mut store, 2);
    let snapshot = store.snapshot();
    let mut wrong = intent(&store);
    wrong.omitted_calls.pop();
    assert!(
        matches!(
            store.append(intent_commit(&snapshot, wrong), at()),
            Err(StoreError::Invalid("undo_binding"))
        ),
        "a partial list is not the recorded selection"
    );
    let unknown = intent(&store);
    let mut wrong = unknown;
    wrong.omitted_calls = vec![CallRef {
        response_entry_id: EntryId::new(),
        call_id: CallId::new(),
    }];
    assert!(matches!(
        store.append(intent_commit(&snapshot, wrong), at()),
        Err(StoreError::Invalid("undo_binding"))
    ));
}

/// Consuming the target turn does not hide the turns skipped on the way: the
/// skipped bash call is still reported as not reversed.
#[test]
fn a_skipped_bash_call_is_still_reported_once_the_target_turn_is_consumed() {
    let (_temp, _root, mut store) = written("a", Some(b"before"), b"after");
    denied_bash_turn(&mut store, 1);
    // Consume the older write turn first: its own bash list is empty, so the
    // skipped newer turn stays listed.
    let snapshot = store.snapshot();
    let first = intent(&store);
    store
        .append(intent_commit(&snapshot, first.clone()), at())
        .unwrap();
    let pending = store.snapshot();
    store.append(terminal(&pending, &first), at()).unwrap();
    // The newer bash-only turn is skipped and still reported as not reversed.
    let UndoSelection::Nothing { omitted_calls } = store.snapshot().select_undo().unwrap() else {
        panic!("nothing is undoable now")
    };
    assert_eq!(omitted_calls.len(), 1, "a skipped turn is still reported");
}

/// A terminal commit whose one entry is not a note is invalid, never a panic.
#[test]
fn a_result_with_a_non_note_entry_is_invalid_not_a_panic() {
    let (_temp, _root, mut store) = written("a", Some(b"before"), b"after");
    let snapshot = store.snapshot();
    let exact = intent(&store);
    store
        .append(intent_commit(&snapshot, exact.clone()), at())
        .unwrap();
    let snapshot = store.snapshot();
    let mut bad = terminal(&snapshot, &exact);
    bad.entries[0].payload = EntryPayload::UserMessage(UserMessage {
        content: Vec::new(),
    });
    let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.append(bad, at()).map(|_| ())
    }))
    .expect("storage must not panic on a malformed terminal commit");
    assert!(
        matches!(refused, Err(StoreError::Invalid("undo_commit_shape"))),
        "{refused:?}"
    );
    // The pending intent is untouched, so the honest result still resolves it.
    let snapshot = store.snapshot();
    store
        .append(terminal(&snapshot, &exact), at())
        .expect("the session recovers and can still be resolved");
}

/// The same malformed record, replayed from a journal file: a diagnostic, not a
/// panic and not a silently accepted history.
#[test]
fn a_malformed_terminal_commit_is_a_replay_diagnostic() {
    let (_temp, root, mut store) = written("a", Some(b"before"), b"after");
    let snapshot = store.snapshot();
    let exact = intent(&store);
    store
        .append(intent_commit(&snapshot, exact.clone()), at())
        .unwrap();
    let id = store.snapshot().session_id().clone();
    let snapshot = store.snapshot();
    let mut bad = terminal(&snapshot, &exact);
    bad.entries[0].payload = EntryPayload::UserMessage(UserMessage {
        content: Vec::new(),
    });
    let line = codec::encode(&Record {
        version: codec::CURRENT_VERSION,
        seq: snapshot.sequence() + 1,
        record_id: RecordId::new(),
        session_id: id.clone(),
        at: at(),
        previous_checksum: Some(snapshot.checksum().clone()),
        payload: RecordPayload::Commit(bad),
    })
    .expect("a well-formed v5 record");
    drop(store);
    let path = journal_path(&root, &id);
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&line.0).unwrap();
    file.sync_all().unwrap();
    let inspection = JournalStore::inspect(&root, &id).unwrap();
    let diagnostic = inspection.diagnostic.expect("a diagnostic");
    assert_eq!(diagnostic.kind, DiagnosticKind::Corrupt);
    assert_eq!(diagnostic.code, "undo_commit_shape");
    assert!(matches!(
        JournalStore::open(root, &id),
        Err(StoreError::Journal(_))
    ));
}

/// ADR 0032 §8: the intent and every terminal shape are bounded before a
/// target changes. A maximum note under worst-case escaping, every target and
/// every omitted call are reserved; a record that cannot fit is refused.
#[test]
fn the_record_preflight_bounds_the_worst_case_terminal_commit() {
    let (_temp, _root, store) = written("a", Some(b"before"), b"after");
    let small = intent(&store);
    assert!(crate::session::undo::fits_record_limit(&small, 0));
    // A small undo is nowhere near the limit.
    let mut few = small.clone();
    few.targets = (0..10)
        .map(|n| {
            let path = format!("{n:0>4000}");
            target(&path, None)
        })
        .collect();
    assert!(crate::session::undo::fits_record_limit(&few, 0));
    // The window that matters: an intent that still fits, whose *terminal*
    // commit does not, because the note and the rows name every 4 KiB path.
    // This case fails only if the terminal shapes are bounded (tested by
    // removing that loop: `fits_record_limit` then accepts it).
    let mut many = small.clone();
    many.targets = (0..1200)
        .map(|n| {
            let path = format!("{n:0>4000}");
            target(&path, None)
        })
        .collect();
    assert!(
        !crate::session::undo::fits_record_limit(&many, 0),
        "a terminal commit over 8 MiB must be refused before any change"
    );
    // Omitted calls count too, in the intent and in every note line.
    let mut calls = small.clone();
    calls.omitted_calls = (0..120_000)
        .map(|_| CallRef {
            response_entry_id: EntryId::new(),
            call_id: CallId::new(),
        })
        .collect();
    assert!(!crate::session::undo::fits_record_limit(&calls, 0));
}

fn target(path: &str, pre: Option<ArtifactRef>) -> UndoTarget {
    UndoTarget {
        path: path.to_owned(),
        invocation_ids: vec![InvocationId::new()],
        pre_mode: pre.as_ref().map(|_| 0o644),
        pre_image: pre,
        post_image: Digest::of(path.as_bytes()),
    }
}

/// ADR 0032 §1: a newest turn whose every mutation is proven `not_applied` has
/// no file changes. It is a candidate with no targets, is consumed by an empty
/// successful undo, and the turn before it is then reachable.
#[test]
fn an_all_not_applied_candidate_is_consumed_and_reaches_the_earlier_turn() {
    let (_temp, _root, mut store) = written("a", Some(b"before"), b"after");
    // A second, newer turn whose one write never happened.
    let capture_plan = capture(
        std::sync::Arc::new(DataRoot::open(_root.path(), false).unwrap()).as_ref(),
        "b",
        Some(b"old"),
        b"new",
    );
    let mut writes = Writes {
        call: None,
        invocation: None,
        step: 0,
        plan: capture_plan.plan,
        pre: capture_plan.pre,
        path: "b",
        post: b"new",
        capture: None,
        effect: FileEffect::NotApplied,
        file_paths: Vec::new(),
    };
    for _ in 0..TOOL_TURN_COMMITS {
        let snapshot = store.snapshot();
        let commit = writes.next(&snapshot);
        store.append(commit, at()).unwrap();
    }
    let UndoSelection::Candidate {
        turn_id,
        targets,
        omitted_calls,
    } = store.snapshot().select_undo().unwrap()
    else {
        panic!("the not-applied turn is still a candidate")
    };
    assert!(
        targets.is_empty(),
        "nothing was applied, so nothing restores"
    );
    assert!(omitted_calls.is_empty());
    let empty = UndoIntent {
        undo_id: UndoId::new(),
        target_turn_id: turn_id.clone(),
        anchor_head: store.snapshot().head().cloned(),
        configuration: store.snapshot().configuration().clone(),
        policy_revision: store.snapshot().policy_revision().into(),
        targets: Vec::new(),
        omitted_calls: Vec::new(),
    };
    let snapshot = store.snapshot();
    store
        .append(intent_commit(&snapshot, empty.clone()), at())
        .expect("an empty candidate is a valid intent");
    let pending = store.snapshot();
    store
        .append(terminal(&pending, &empty), at())
        .expect("an empty successful undo commits");
    let note = store
        .snapshot()
        .entries()
        .find_map(|(_, stored)| match &stored.entry.payload {
            EntryPayload::UndoNote(note) => Some(note.text.clone()),
            _ => None,
        })
        .expect("a note");
    assert!(
        note.contains("No trusted file changes required restoration."),
        "{note}"
    );
    // The consumed turn no longer hides the earlier one.
    let UndoSelection::Candidate {
        turn_id: next,
        targets,
        ..
    } = store.snapshot().select_undo().unwrap()
    else {
        panic!("the earlier turn is reachable again")
    };
    assert_ne!(next, turn_id);
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].path, "a");
}

#[test]
fn partially_applied_multi_edit_selects_only_committed_paths_for_undo() {
    let (_temp, root, mut store) = fresh();
    let pre_a = root.put_artifact(b"a-before").unwrap();
    let pre_b = root.put_artifact(b"b-before").unwrap();
    let plan_bytes = serde_json::to_vec(&json!({
        "version": 2,
        "name": "multi_edit",
        "targets": [
            {"path":"a", "pre_image":pre_a.sha256},
            {"path":"b", "pre_image":pre_b.sha256}
        ]
    }))
    .unwrap();
    let plan = root.put_artifact(&plan_bytes).unwrap();
    let multi_image = FileImage {
        path: "a".into(),
        pre_image: Some(pre_a.clone()),
        pre_mode: Some(0o644),
        post_image: Digest::of(b"a-after"),
        additional_targets: vec![FileImageTarget {
            path: "b".into(),
            pre_image: Some(pre_b),
            pre_mode: Some(0o644),
            post_image: Digest::of(b"b-after"),
        }],
    };
    let mut writes = Writes {
        call: None,
        invocation: None,
        step: 0,
        plan,
        pre: Some(pre_a.clone()),
        path: "a",
        post: b"a-after",
        capture: Some(multi_image),
        effect: FileEffect::Applied,
        file_paths: vec![
            FilePathEffect {
                path: "a".into(),
                effect: FileEffect::Applied,
            },
            FilePathEffect {
                path: "b".into(),
                effect: FileEffect::NotApplied,
            },
        ],
    };
    for _ in 0..TOOL_TURN_COMMITS {
        let snapshot = store.snapshot();
        store.append(writes.next(&snapshot), at()).unwrap();
    }
    let UndoSelection::Candidate { targets, .. } = store.snapshot().select_undo().unwrap() else {
        panic!("partially applied batch should have one reversible target");
    };
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].path, "a");
    assert_eq!(targets[0].pre_image, Some(pre_a));
    assert_eq!(targets[0].post_image, Digest::of(b"a-after"));
}

/// Decision 17 compatibility: a committed v4 journal written by a v0.2.0
/// binary records an `undo_intent` with the old `omitted_calls`. It replays
/// byte-identical, the corrected selection would differ, and it still resolves
/// and continues.
#[test]
fn a_committed_v0_2_0_undo_intent_replays_byte_identical_and_continues() {
    let (temp, root) = environment();
    load_artifacts(&root);
    let bytes = source("undo-v0.2.0-omitted");
    let id = install(&root, &bytes);
    let mut store = JournalStore::open(root.clone(), &id).unwrap();
    // Opening reconstructs the pending undo without rewriting a byte.
    assert_eq!(fs::read(journal_path(&root, &id)).unwrap(), bytes);
    let pending = store
        .snapshot()
        .unresolved_undo()
        .expect("the recorded intent")
        .clone();
    assert!(
        pending.intent.omitted_calls.is_empty(),
        "the v0.2.0 writer omitted the skipped turn's bash call"
    );
    // The v0.2.1 selection would list it, which is why replay accepts both.
    let UndoSelection::Candidate {
        turn_id,
        omitted_calls,
        ..
    } = store.snapshot().select_undo().unwrap()
    else {
        panic!("a candidate turn")
    };
    assert_eq!(turn_id, pending.intent.target_turn_id);
    assert_eq!(omitted_calls.len(), 1);
    // The journal continues: its own recorded intent resolves and a new turn
    // follows in the same chain.
    let snapshot = store.snapshot();
    store
        .append(terminal(&snapshot, &pending.intent), at())
        .expect("the v0.2.0 intent resolves");
    let snapshot = store.snapshot();
    store
        .append(start_turn(&snapshot, "after"), at())
        .expect("the session continues");
    assert!(store.snapshot().sequence() > pending.sequence);
    let sequence = store.snapshot().sequence();
    drop(store);
    // The chain the fixture started is still one valid journal.
    assert_eq!(
        JournalStore::open(root, &id).unwrap().snapshot().sequence(),
        sequence
    );
    drop(temp);
}
