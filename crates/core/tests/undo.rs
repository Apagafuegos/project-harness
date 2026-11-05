//! ADR 0032 end to end: capture before dispatch, durable undo, preflight
//! refusal, large and binary pre-images, and no automatic retry.
use harness_core::{
    context::ModelLimits,
    model::{
        ProviderEvent,
        scripted::{Script, ScriptedProvider, Step},
    },
    runtime::*,
    session::*,
};
use serde_json::{Value, json};
use std::{
    fs,
    future::Future,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

fn model() -> ModelRef {
    ModelRef {
        provider_instance: "mock".into(),
        model: "scripted".into(),
    }
}
fn config() -> DriverConfig {
    let mut config = DriverConfig::default();
    config.runtime.context.catalog.insert(
        model(),
        ModelLimits {
            context_tokens: Some(128_000),
            max_output_tokens: None,
        },
    );
    config
}
fn policy() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "policy_revision": "p1",
        "enabled_tools": ["read", "write", "edit"],
        "workspace_read": "allow",
        "workspace_mutation": "ask",
        "process": "deny",
        "network": "deny",
        "protected_paths": ["private"],
        "allowances": []
    }))
    .unwrap()
}
fn proposal(calls: &[(&str, Value)]) -> Script {
    let mut steps = vec![Step::Event(ProviderEvent::Started)];
    for (index, (name, arguments)) in calls.iter().enumerate() {
        let index = index as u32;
        steps.push(Step::Event(ProviderEvent::ToolCallStarted {
            block_index: index,
            name: (*name).into(),
            provider_call_id: None,
        }));
        steps.push(Step::Event(ProviderEvent::ToolArgumentsDelta {
            block_index: index,
            fragment: arguments.to_string(),
        }));
        steps.push(Step::Event(ProviderEvent::ToolCallReady {
            block_index: index,
        }));
    }
    steps.push(Step::completed(FinishReason::ToolCalls));
    Script::new(steps)
}
fn reply() -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, "done"),
        Step::completed(FinishReason::Stop),
    ])
}
async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), future)
        .await
        .expect("undo test deadline")
}
async fn wait(handle: &SessionHandle, predicate: impl Fn(&RuntimeView) -> bool) -> RuntimeView {
    bounded(async {
        loop {
            let view = handle.view().await.unwrap();
            assert_ne!(view.phase, Phase::Faulted, "unexpected fault: {view:?}");
            if predicate(&view) {
                return view;
            }
            if view.phase == Phase::AwaitingResolution {
                panic!("awaiting resolution: {view:?}");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
}
fn idle(view: &RuntimeView) -> bool {
    view.phase == Phase::Durable(RuntimeState::Idle) && view.active_turn.is_none()
}
async fn start(handle: &SessionHandle) {
    bounded(handle.command(Command::StartTurn {
        command_id: CommandId::new(),
        content: vec![ContentBlock::Text { text: "go".into() }],
    }))
    .await
    .unwrap();
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: Arc<DataRoot>,
    host: ToolHostConfig,
    session: SessionId,
    header: SessionHeader,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("workspace")).unwrap();
        fs::create_dir(temp.path().join("workspace/private")).unwrap();
        let root = Arc::new(DataRoot::open(temp.path().join("data"), true).unwrap());
        let host = ToolHostConfig {
            workspace_root: temp.path().join("workspace"),
            data_root: root.path().into(),
            sandbox: None,
        };
        let header = SessionHeader {
            workspace: host.workspace_root.to_str().unwrap().into(),
            initial_model: model(),
            policy_revision: "p1".into(),
            configuration: root.put_artifact(&policy()).unwrap(),
        };
        Self {
            _temp: temp,
            root,
            host,
            session: SessionId::new(),
            header,
        }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.host.workspace_root.join(name)
    }
    async fn create(&self, provider: Arc<ScriptedProvider>) -> SessionHandle {
        bounded(SessionHandle::create_with_tools(
            self.root.clone(),
            self.session.clone(),
            self.header.clone(),
            provider,
            config(),
            &self.host,
        ))
        .await
        .unwrap()
    }
    async fn open(&self, provider: Arc<ScriptedProvider>) -> SessionHandle {
        bounded(SessionHandle::open_with_tools(
            self.root.clone(),
            self.session.clone(),
            provider,
            config(),
            &self.host,
        ))
        .await
        .unwrap()
    }
    /// One completed turn with the given calls, each approved.
    async fn turn(&self, calls: &[(&str, Value)]) -> SessionHandle {
        let handle = self
            .create(Arc::new(ScriptedProvider::new([proposal(calls), reply()])))
            .await;
        start(&handle).await;
        // Approve every ticket the turn asks for; a read-only call asks for
        // none under this policy.
        for _ in 0..calls.len() + 1 {
            let Some(ticket) = wait(&handle, |view| {
                view.pending_approval.is_some() || idle(view)
            })
            .await
            .pending_approval
            else {
                break;
            };
            bounded(handle.command(Command::SubmitApproval {
                approval_id: ticket.approval_id,
                plan_digest: ticket.plan_digest,
                decision: Decision::Allow,
                remember: false,
            }))
            .await
            .unwrap();
        }
        wait(&handle, idle).await;
        handle
    }
    fn journal(&self) -> Vec<Value> {
        let path = self
            .root
            .path()
            .join("sessions")
            .join(format!("{}.jsonl", self.session));
        fs::read(path)
            .unwrap()
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap()["record"].clone())
            .collect()
    }
}

fn entry<'a>(records: &'a [Value], kind: &str) -> Option<&'a Value> {
    records.iter().find_map(|record| {
        record["payload"]["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|entry| &entry["payload"])
            .find(|payload| payload["kind"] == kind)
    })
}
fn event<'a>(records: &'a [Value], kind: &str) -> Option<&'a Value> {
    records.iter().find_map(|record| {
        record["payload"]["events"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|event| event["kind"] == kind)
    })
}

async fn preview(handle: &SessionHandle) -> UndoPreview {
    match bounded(handle.command(Command::PreviewUndo)).await {
        Ok(Accepted::UndoPreviewed { preview }) => *preview,
        other => panic!("no preview: {other:?}"),
    }
}
async fn undo(handle: &SessionHandle, preview: &UndoPreview) -> Result<Accepted, Rejection> {
    bounded(handle.command(Command::Undo {
        command_id: CommandId::new(),
        expected_sequence: preview.sequence,
        expected_head: preview.head.clone(),
        expected_configuration: preview.configuration.clone(),
        target_turn_id: preview.target_turn_id.clone(),
    }))
    .await
}

#[tokio::test]
async fn undo_restores_an_overwritten_file_and_removes_a_created_one() {
    let fixture = Fixture::new();
    fs::write(fixture.path("a"), "before").unwrap();
    // A private mode is part of the captured pre-image and comes back with it;
    // special bits are never restored.
    fs::set_permissions(fixture.path("a"), fs::Permissions::from_mode(0o600)).unwrap();
    let handle = fixture
        .turn(&[
            ("write", json!({"path":"a","text":"after","overwrite":true})),
            ("write", json!({"path":"b","text":"created"})),
        ])
        .await;
    assert_eq!(fs::read_to_string(fixture.path("a")).unwrap(), "after");
    assert_eq!(fs::read_to_string(fixture.path("b")).unwrap(), "created");
    let records_before = fixture.journal().len();

    let preview = preview(&handle).await;
    assert_eq!(preview.targets.len(), 2);
    assert!(preview.targets[0].restores, "a had a pre-image");
    assert!(!preview.targets[1].restores, "b was created");
    let result = undo(&handle, &preview).await;
    let Accepted::UndoCompleted { outcome, files, .. } = result.unwrap() else {
        panic!("unexpected reply")
    };
    assert_eq!(outcome, UndoOutcome::Succeeded);
    assert_eq!(
        files.iter().map(|row| row.outcome).collect::<Vec<_>>(),
        [FileOutcome::Restored, FileOutcome::Removed]
    );
    wait(&handle, idle).await;
    assert_eq!(fs::read_to_string(fixture.path("a")).unwrap(), "before");
    assert_eq!(
        fs::metadata(fixture.path("a")).unwrap().mode() & 0o777,
        0o600,
        "the captured ordinary mode is restored",
    );
    assert!(!fixture.path("b").exists(), "a created file is removed");

    // History is append-only: the intent, the note and the result are new
    // records, and every earlier record is byte-identical.
    let records = fixture.journal();
    assert!(records.len() > records_before);
    let note = entry(&records, "undo_note").expect("a recorded note");
    assert_eq!(note["outcome"], "succeeded");
    let text = note["text"].as_str().unwrap();
    assert!(
        text.starts_with(&format!(
            "The user reverted the agent's trusted file edits from turn {}",
            preview.target_turn_id
        )),
        "{text}"
    );
    assert!(text.contains(r#"Restored: "a""#), "{text}");
    assert!(
        text.contains(r#"Removed (created by the agent): "b""#),
        "{text}"
    );
    assert!(
        text.ends_with("Do not assume the reverted edits are present.\n"),
        "{text}"
    );
    let result = event(&records, "undo_result").expect("a result");
    assert_eq!(result["file_outcomes"][0]["outcome"], "restored");
    // The note is projected as one user block carrying its own entry id, and
    // its text carries no pre-image digest or artifact reference.
    let report = handle.inspect_context(None, None).await.unwrap();
    assert_eq!(report.undos.len(), 1);
    assert_eq!(report.undos[0].outcome, UndoOutcome::Succeeded);
    assert_eq!(report.undos[0].paths, ["a", "b"]);
    let note_entry = report.undos[0].note_entry_id.clone();
    let projected = report
        .included
        .iter()
        .find(|item| item.entry_id.as_ref() == Some(&note_entry))
        .expect("the note is an included item");
    assert_eq!(projected.kind, harness_core::context::ItemKind::User);
    assert!(
        projected.estimated_tokens > 0,
        "the note is counted in user tokens"
    );
    assert!(
        report.totals.estimated_user >= projected.estimated_tokens,
        "note text is part of the user token total"
    );
    // The export renders the note and never the pre-image bytes.
    let snapshot = handle.view().await.unwrap().snapshot;
    let markdown = harness_core::session::markdown_export(&snapshot, false);
    assert!(
        markdown.contains("The user reverted the agent's trusted file edits"),
        "{markdown}"
    );
    assert!(!markdown.contains("pre_image"), "{markdown}");
    assert!(!markdown.contains("before"), "pre-image bytes never appear");
    handle.shutdown().await;
    // The next preview offers the turn before this one, or nothing at all.
    let reopened = fixture.open(Arc::new(ScriptedProvider::new([]))).await;
    match bounded(reopened.command(Command::PreviewUndo)).await {
        Ok(Accepted::UndoPreviewed { preview }) => {
            assert_ne!(preview.target_turn_id, records_target(&records))
        }
        Err(Rejection::Undo { code, .. }) => assert_eq!(code, "undo_nothing"),
        other => panic!("unexpected reply: {other:?}"),
    }
    reopened.shutdown().await;
}

fn records_target(records: &[Value]) -> TurnId {
    let note = entry(records, "undo_note").expect("note");
    serde_json::from_value(note["target_turn_id"].clone()).unwrap()
}

#[tokio::test]
async fn a_preflight_conflict_changes_nothing_and_consumes_nothing() {
    let fixture = Fixture::new();
    fs::write(fixture.path("a"), "before").unwrap();
    let handle = fixture
        .turn(&[("write", json!({"path":"a","text":"after","overwrite":true}))])
        .await;
    // Someone else edits the file after the agent did, and their directory is
    // not writable: the whole-set preflight must name the content conflict
    // before any replacement temporary is even staged.
    fs::write(fixture.path("a"), "user edit").unwrap();
    fs::set_permissions(
        &fixture.host.workspace_root,
        fs::Permissions::from_mode(0o555),
    )
    .unwrap();
    let first = preview(&handle).await;
    let before = fixture.journal();
    let refusal = undo(&handle, &first).await.unwrap_err();
    let Rejection::Undo {
        code, conflicts, ..
    } = refusal
    else {
        panic!("unexpected refusal")
    };
    assert_eq!(code, "undo_conflict");
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].path, "a");
    assert_eq!(
        conflicts[0].reason, "changed",
        "the content preflight runs before staging"
    );
    fs::set_permissions(
        &fixture.host.workspace_root,
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    // Nothing changed, and the turn is still the next candidate.
    assert_eq!(fs::read_to_string(fixture.path("a")).unwrap(), "user edit");
    assert_eq!(
        fixture.journal(),
        before,
        "preflight refusal appends nothing"
    );
    assert!(
        !fs::read_dir(&fixture.host.workspace_root)
            .unwrap()
            .any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".harness-undo-")
            }),
        "preflight refusal leaks no staging"
    );
    let again = self::preview(&handle).await;
    assert_eq!(again.target_turn_id, first.target_turn_id);
    assert_eq!(again.sequence, first.sequence);
    handle.shutdown().await;
}

#[tokio::test]
async fn a_large_and_a_binary_pre_image_are_streamed_and_restored() {
    let fixture = Fixture::new();
    // Larger than the 1 MiB write limit: only the old target is large.
    let large: Vec<u8> = (0..2 * 1024 * 1024 + 7).map(|n| (n % 251) as u8).collect();
    fs::write(fixture.path("big"), &large).unwrap();
    let handle = fixture
        .turn(&[
            (
                "write",
                json!({"path":"big","text":"small","overwrite":true}),
            ),
            ("write", json!({"path":"tiny","text":"third"})),
        ])
        .await;
    assert_eq!(fs::read_to_string(fixture.path("big")).unwrap(), "small");
    // The pre-image was captured before the mutation was dispatched.
    let records = fixture.journal();
    let intent = event(&records, "execution_intent").expect("an intent");
    let image = &intent["file_image"];
    assert_eq!(image["path"], "big");
    assert_eq!(image["pre_mode"], 0o644);
    assert_eq!(image["pre_image"]["bytes"], large.len() as u64);
    assert_eq!(
        image["pre_image"]["sha256"],
        Digest::of(&large).as_str(),
        "the captured bytes are the old file"
    );
    let preview = preview(&handle).await;
    let Accepted::UndoCompleted { files, .. } = undo(&handle, &preview).await.unwrap() else {
        panic!("unexpected reply")
    };
    assert_eq!(files[0].outcome, FileOutcome::Restored);
    assert_eq!(fs::read(fixture.path("big")).unwrap(), large);
}

#[tokio::test]
async fn a_binary_old_target_is_a_valid_pre_image() {
    let fixture = Fixture::new();
    let bytes: Vec<u8> = vec![0x00, 0xff, 0x80, b'a', 0x0a, 0x00];
    fs::write(fixture.path("bin"), &bytes).unwrap();
    let handle = fixture
        .turn(&[(
            "write",
            json!({"path":"bin","text":"text now","overwrite":true}),
        )])
        .await;
    let preview = preview(&handle).await;
    let Accepted::UndoCompleted { outcome, .. } = undo(&handle, &preview).await.unwrap() else {
        panic!("unexpected reply")
    };
    assert_eq!(outcome, UndoOutcome::Succeeded);
    assert_eq!(fs::read(fixture.path("bin")).unwrap(), bytes);
}

#[tokio::test]
async fn a_turn_with_no_trusted_file_edit_reports_nothing_and_lists_bash() {
    let fixture = Fixture::new();
    fs::write(fixture.path("a"), "hello world").unwrap();
    let handle = fixture.turn(&[("read", json!({"path":"a"}))]).await;
    match bounded(handle.command(Command::PreviewUndo)).await {
        Err(Rejection::Undo {
            code,
            detail,
            omitted,
            ..
        }) => {
            assert_eq!(code, "undo_nothing");
            assert!(
                detail.contains("no unconsumed trusted file-edit turn"),
                "{detail}"
            );
            assert!(omitted.is_empty(), "a read is not an omitted effect");
        }
        other => panic!("unexpected reply: {other:?}"),
    }
}

#[tokio::test]
async fn a_stale_preview_never_reselects_a_different_turn() {
    let fixture = Fixture::new();
    fs::write(fixture.path("a"), "before").unwrap();
    let handle = fixture
        .turn(&[("write", json!({"path":"a","text":"after","overwrite":true}))])
        .await;
    let preview = preview(&handle).await;
    // Another recorded change moves the head; the confirmation is stale.
    bounded(handle.command(Command::SelectHead {
        head: preview.head.clone(),
    }))
    .await
    .ok();
    let stale = bounded(handle.command(Command::Undo {
        command_id: CommandId::new(),
        expected_sequence: preview.sequence.saturating_sub(1),
        expected_head: preview.head.clone(),
        expected_configuration: preview.configuration.clone(),
        target_turn_id: preview.target_turn_id.clone(),
    }))
    .await
    .unwrap_err();
    match stale {
        Rejection::Undo { code, .. } => assert_eq!(code, "undo_stale_preview"),
        other => panic!("unexpected refusal: {other:?}"),
    }
    assert_eq!(fs::read_to_string(fixture.path("a")).unwrap(), "after");
}

#[tokio::test]
async fn a_capture_that_stales_refuses_without_dispatching() {
    let fixture = Fixture::new();
    fs::write(fixture.path("a"), "before").unwrap();
    // The plan is built against `before`; the file changes before the
    // executor may rename, so the mutation must refuse and change nothing.
    let handle = fixture
        .create(Arc::new(ScriptedProvider::new([
            proposal(&[("write", json!({"path":"a","text":"after","overwrite":true}))]),
            reply(),
        ])))
        .await;
    start(&handle).await;
    let pending = wait(&handle, |view| view.pending_approval.is_some())
        .await
        .pending_approval
        .unwrap();
    fs::write(fixture.path("a"), "changed meanwhile").unwrap();
    bounded(handle.command(Command::SubmitApproval {
        approval_id: pending.approval_id,
        plan_digest: pending.plan_digest,
        decision: Decision::Allow,
        remember: false,
    }))
    .await
    .unwrap();
    let view = wait(&handle, idle).await;
    assert_eq!(
        fs::read_to_string(fixture.path("a")).unwrap(),
        "changed meanwhile"
    );
    let results: Vec<_> = view
        .snapshot
        .entries()
        .filter_map(|(_, entry)| match &entry.entry.payload {
            EntryPayload::ToolResult(result) => Some(&result.result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert_ne!(results[0].status, ToolStatus::Succeeded);
    // No intent was ever recorded: capture refuses before dispatch.
    assert!(event(&fixture.journal(), "execution_intent").is_none());
}

#[tokio::test]
async fn a_pending_undo_blocks_new_work_until_it_is_resolved() {
    let fixture = Fixture::new();
    fs::write(fixture.path("a"), "before").unwrap();
    let handle = fixture
        .turn(&[("write", json!({"path":"a","text":"after","overwrite":true}))])
        .await;
    // Reopen, then hand-write the state a crash between intent and result
    // leaves behind: the intent is durable, its result never was.
    handle.shutdown().await;
    fixture.inject_pending_undo();
    let reopened = fixture.open(Arc::new(ScriptedProvider::new([]))).await;
    let view = wait(&reopened, |view| {
        view.phase == Phase::AwaitingResolution && view.undo.is_some()
    })
    .await;
    let undo = view.undo.clone().unwrap();
    assert_eq!(
        view.snapshot.unresolved_undo().unwrap().intent.undo_id,
        undo.undo_id
    );
    // New work and a configuration change are refused while it waits.
    assert_eq!(
        bounded(reopened.command(Command::StartTurn {
            command_id: CommandId::new(),
            content: vec![ContentBlock::Text {
                text: "again".into()
            }],
        }))
        .await
        .unwrap_err(),
        Rejection::ResolutionRequired
    );
    assert_eq!(
        bounded(reopened.command(Command::PreviewUndo))
            .await
            .unwrap_err(),
        Rejection::Undo {
            code: "undo_uncertain_pending",
            detail: format!("resolve pending undo {} before undo", undo.undo_id),
            conflicts: Vec::new(),
            omitted: Vec::new(),
        }
    );
    // The user's statement about the whole set resolves it.
    let targets = view
        .snapshot
        .unresolved_undo()
        .unwrap()
        .intent
        .targets
        .clone();
    bounded(
        reopened.command(Command::ResolveUndo {
            undo_id: undo.undo_id.clone(),
            resolution: UndoResolution::Observed {
                files: targets
                    .iter()
                    .map(|target| UndoFileOutcome {
                        path: target.path.clone(),
                        outcome: FileOutcome::ObservedRestored,
                    })
                    .collect(),
                note: "I put the file back by hand".into(),
            },
        }),
    )
    .await
    .unwrap();
    wait(&reopened, idle).await;
    let records = fixture.journal();
    let note = entry(&records, "undo_note").expect("resolved note");
    assert_eq!(note["outcome"], "observed");
    let text = note["text"].as_str().unwrap();
    assert!(
        text.starts_with(&format!(
            "The user resolved interrupted undo {} of turn {}",
            undo.undo_id, undo.target_turn_id
        )),
        "{text}"
    );
    assert!(
        text.contains("User observed observed_restored: \"a\""),
        "{text}"
    );
    assert!(
        text.contains(r#"User resolution note: "I put the file back by hand""#),
        "{text}"
    );
    assert!(
        text.ends_with(
            "Do not assume either the original edits or their restoration are present.\n"
        ),
        "{text}"
    );
    // After resolution a new turn is accepted again.
    start(&reopened).await;
    reopened.shutdown().await;
}

impl Fixture {
    /// Append a durable `undo_intent` with no result, as a crash leaves it.
    fn inject_pending_undo(&self) {
        let store = JournalStore::open(self.root.clone(), &self.session).unwrap();
        let snapshot = store.snapshot();
        let selection = snapshot.select_undo().unwrap();
        let UndoSelection::Candidate {
            turn_id,
            targets,
            omitted_calls,
        } = selection
        else {
            panic!("a candidate turn")
        };
        drop(store);
        let mut store = JournalStore::open(self.root.clone(), &self.session).unwrap();
        let commit = Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: snapshot.head().cloned(),
                to: snapshot.head().cloned(),
                reason: HeadReason::Retain,
            },
            entries: Vec::new(),
            events: vec![JournalEvent::UndoIntent(UndoIntent {
                undo_id: UndoId::new(),
                target_turn_id: turn_id,
                anchor_head: snapshot.head().cloned(),
                configuration: snapshot.configuration().clone(),
                policy_revision: snapshot.policy_revision().into(),
                targets,
                omitted_calls,
            })],
        };
        store.append(commit, Timestamp::now()).unwrap();
    }
}

/// A protected path is refused by the plan, so its file is never captured.
#[tokio::test]
async fn a_protected_target_never_reaches_capture_or_undo() {
    let fixture = Fixture::new();
    fs::write(fixture.path("private/secret"), "secret").unwrap();
    let handle = fixture
        .create(Arc::new(ScriptedProvider::new([
            proposal(&[(
                "write",
                json!({"path":"private/secret","text":"leak","overwrite":true}),
            )]),
            reply(),
        ])))
        .await;
    start(&handle).await;
    let view = wait(&handle, idle).await;
    let results: Vec<_> = view
        .snapshot
        .entries()
        .filter_map(|(_, entry)| match &entry.entry.payload {
            EntryPayload::ToolResult(result) => Some(&result.result),
            _ => None,
        })
        .collect();
    assert_eq!(results[0].status, ToolStatus::Denied);
    assert_eq!(
        fs::read_to_string(fixture.path("private/secret")).unwrap(),
        "secret"
    );
    assert!(event(&fixture.journal(), "execution_intent").is_none());
}

/// A write turn that created a file inside `d`, then `d` made read-only: the
/// unlink cannot happen, so the undo stops after changing nothing.
async fn partial_restore(fixture: &Fixture) -> (SessionHandle, UndoPreview) {
    fs::create_dir(fixture.path("d")).unwrap();
    let handle = fixture
        .turn(&[("write", json!({"path":"d/new.txt","text":"x"}))])
        .await;
    let shown = self::preview(&handle).await;
    fs::set_permissions(fixture.path("d"), fs::Permissions::from_mode(0o555)).unwrap();
    (handle, shown)
}

async fn wait_awaiting_undo(handle: &SessionHandle) -> RuntimeView {
    bounded(async {
        loop {
            let view = handle.view().await.unwrap();
            assert_ne!(view.phase, Phase::Faulted, "a refusal faulted the session");
            if view.phase == Phase::AwaitingResolution && view.undo.is_some() {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
}

/// v0.2.1: a partial restoration always answers its command, with a stable
/// code that names the undo and says where it is resolved.
#[tokio::test]
async fn a_partial_restore_answers_the_undo_command() {
    let fixture = Fixture::new();
    let (handle, shown) = partial_restore(&fixture).await;
    let reply = tokio::time::timeout(Duration::from_secs(20), undo(&handle, &shown))
        .await
        .expect("the undo command must be answered, not hang");
    let view = wait_awaiting_undo(&handle).await;
    let pending = view
        .snapshot
        .unresolved_undo()
        .expect("the interrupted undo");
    match reply {
        Err(Rejection::Undo { code, detail, .. }) => {
            assert_eq!(code, "undo_interrupted");
            assert!(
                detail.contains(pending.intent.undo_id.as_str()),
                "the refusal names the undo: {detail}"
            );
            assert!(detail.contains("TUI"), "{detail}");
        }
        other => panic!("unexpected reply: {other:?}"),
    }
    // Nothing was restored, and the intent is explicitly uncertain.
    assert!(pending.uncertain);
    assert!(event(&fixture.journal(), "undo_result").is_none());
    assert_eq!(fs::read(fixture.path("d/new.txt")).unwrap(), b"x");
    fs::set_permissions(fixture.path("d"), fs::Permissions::from_mode(0o755)).unwrap();
    handle.shutdown().await;
}

/// v0.2.1: a note over the storage limit is refused before a commit is built;
/// the session stays resolvable and the dialog's answer is never lost to a
/// fault.
#[tokio::test]
async fn an_over_long_resolution_note_is_refused_and_the_undo_stays_resolvable() {
    let fixture = Fixture::new();
    let (handle, shown) = partial_restore(&fixture).await;
    let _ = tokio::time::timeout(Duration::from_secs(20), undo(&handle, &shown)).await;
    let view = wait_awaiting_undo(&handle).await;
    let undo_id = view.undo.as_ref().unwrap().undo_id.clone();
    let refusal = bounded(handle.command(Command::ResolveUndo {
        undo_id: undo_id.clone(),
        resolution: UndoResolution::AcknowledgedUnknown {
            note: "n".repeat(100 * 1024),
        },
    }))
    .await
    .expect_err("a note over 64 KiB is refused");
    assert_eq!(refusal, Rejection::Invalid("resolution_note_too_large"));
    // Still awaiting resolution: no fault, no lost intent.
    let view = wait_awaiting_undo(&handle).await;
    assert_eq!(view.undo.as_ref().unwrap().undo_id, undo_id);
    // The same undo resolves with a note inside the limit.
    bounded(handle.command(Command::ResolveUndo {
        undo_id: undo_id.clone(),
        resolution: UndoResolution::AcknowledgedUnknown {
            note: "I checked by hand".into(),
        },
    }))
    .await
    .unwrap();
    wait(&handle, idle).await;
    let records = fixture.journal();
    let note = entry(&records, "undo_note").expect("a recorded note");
    assert_eq!(note["outcome"], "acknowledged_unknown");
    fs::set_permissions(fixture.path("d"), fs::Permissions::from_mode(0o755)).unwrap();
    handle.shutdown().await;
}

/// v0.2.1: a row outcome storage would reject is refused before the commit,
/// instead of reaching storage and faulting the session.
#[tokio::test]
async fn an_invalid_observed_row_is_refused_and_the_undo_stays_resolvable() {
    let fixture = Fixture::new();
    let (handle, shown) = partial_restore(&fixture).await;
    let _ = tokio::time::timeout(Duration::from_secs(20), undo(&handle, &shown)).await;
    let view = wait_awaiting_undo(&handle).await;
    let undo_id = view.undo.as_ref().unwrap().undo_id.clone();
    let targets = view
        .snapshot
        .unresolved_undo()
        .unwrap()
        .intent
        .targets
        .clone();
    // `restored` is only valid for a succeeded undo; an observed row must be
    // an observation.
    let refusal = bounded(
        handle.command(Command::ResolveUndo {
            undo_id: undo_id.clone(),
            resolution: UndoResolution::Observed {
                files: targets
                    .iter()
                    .map(|target| UndoFileOutcome {
                        path: target.path.clone(),
                        outcome: FileOutcome::Restored,
                    })
                    .collect(),
                note: "I put it back".into(),
            },
        }),
    )
    .await
    .expect_err("an invalid row is refused");
    assert_eq!(refusal, Rejection::Invalid("undo_rows_mismatch"));
    let view = wait_awaiting_undo(&handle).await;
    assert_eq!(view.undo.as_ref().unwrap().undo_id, undo_id);
    bounded(
        handle.command(Command::ResolveUndo {
            undo_id: undo_id.clone(),
            resolution: UndoResolution::Observed {
                files: targets
                    .iter()
                    .map(|target| UndoFileOutcome {
                        path: target.path.clone(),
                        outcome: FileOutcome::ObservedRemoved,
                    })
                    .collect(),
                note: "I removed it".into(),
            },
        }),
    )
    .await
    .unwrap();
    wait(&handle, idle).await;
    fs::set_permissions(fixture.path("d"), fs::Permissions::from_mode(0o755)).unwrap();
    handle.shutdown().await;
}

/// Decision 17: undoing an earlier turn reports the bash calls of the newer
/// turns skipped on the way, in the preview, the note and the result.
#[tokio::test]
async fn a_skipped_newer_bash_call_is_reported_as_not_reversed() {
    let fixture = Fixture::new();
    fs::write(fixture.path("a"), "before").unwrap();
    let handle = fixture
        .create(Arc::new(ScriptedProvider::new([
            proposal(&[("write", json!({"path":"a","text":"after","overwrite":true}))]),
            reply(),
            proposal(&[("bash", json!({"command":"true"}))]),
            reply(),
        ])))
        .await;
    for _ in 0..2 {
        start(&handle).await;
        for _ in 0..3 {
            let Some(ticket) = wait(&handle, |v| v.pending_approval.is_some() || idle(v))
                .await
                .pending_approval
            else {
                break;
            };
            bounded(handle.command(Command::SubmitApproval {
                approval_id: ticket.approval_id,
                plan_digest: ticket.plan_digest,
                decision: Decision::Allow,
                remember: false,
            }))
            .await
            .unwrap();
        }
        wait(&handle, idle).await;
    }
    let shown = preview(&handle).await;
    let tools: Vec<_> = shown.omitted.iter().map(|call| call.tool.clone()).collect();
    assert_eq!(tools, ["bash"], "the skipped bash call is listed once");
    let bash_call = shown.omitted[0].call.clone();
    let Accepted::UndoCompleted { outcome, .. } = undo(&handle, &shown).await.unwrap() else {
        panic!("unexpected reply")
    };
    assert_eq!(outcome, UndoOutcome::Succeeded);
    let records = fixture.journal();
    let note = entry(&records, "undo_note").expect("a recorded note");
    let text = note["text"].as_str().unwrap();
    assert!(
        text.contains(&format!("Not reversed: bash call {}", bash_call.call_id)),
        "{text}"
    );
    // The undo consumed the write turn; the bash-only turn stays "nothing to
    // undo" and is still reported there.
    match bounded(handle.command(Command::PreviewUndo)).await {
        Err(Rejection::Undo { code, omitted, .. }) => {
            assert_eq!(code, "undo_nothing");
            assert_eq!(
                omitted.len(),
                1,
                "the skipped turn's bash call stays visible"
            );
        }
        other => panic!("unexpected reply: {other:?}"),
    }
    handle.shutdown().await;
}
