//! Regression tests for ADR 0032's pre-rename and pre-intent boundaries.
use super::{
    FsProtectedPaths, FsWorkspace,
    resolve::{Step, errno},
};
use crate::{
    context::ModelLimits,
    model::{
        ProviderEvent,
        scripted::{Script, ScriptedProvider, Step as ProviderStep},
    },
    runtime::*,
    session::{
        io::{Gate, Point},
        *,
    },
    tools::{AuthorizedOperation, CancelToken, ExecutionOutcome, Executor},
};
use serde_json::json;
use std::{fs, future::Future, sync::Arc, time::Duration};

/// A bounded wait. A break-on-purpose build must fail with a named message
/// instead of parking the test binary forever.
async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    match tokio::time::timeout(Duration::from_secs(15), future).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for {what}"),
    }
}

/// Releases an injected durability gate when dropped, so a failing assertion
/// can never leave the storage worker blocked on it.
struct GateGuard(Arc<DataRoot>, std::sync::mpsc::Sender<()>);
impl Drop for GateGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.io.test.lock() {
            state.gate = None;
        }
        let _ = self.1.send(());
    }
}

struct FailTemporary(Arc<FsWorkspace>);
impl Executor for FailTemporary {
    fn backend(&self) -> ExecutionBackend {
        ExecutionBackend::TrustedFs
    }
    fn execute(&self, operation: &AuthorizedOperation, cancel: &CancelToken) -> ExecutionOutcome {
        let fail = operation.image().is_some_and(|image| image.path == "b");
        self.0.execute_hooked(
            operation.plan(),
            cancel,
            &mut |step| {
                if fail && step == Step::WriteTemp {
                    Err(errno(rustix::io::Errno::NOSPC))
                } else {
                    Ok(())
                }
            },
            None,
            operation.image(),
        )
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    workspace: std::path::PathBuf,
    root: Arc<DataRoot>,
    handle: SessionHandle,
    files: Arc<FsWorkspace>,
}
impl Fixture {
    async fn turn(fail_temporary: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        for path in ["a", "b"] {
            fs::write(workspace.join(path), "before").unwrap();
        }
        let root = Arc::new(DataRoot::open(temp.path().join("data"), true).unwrap());
        let model = ModelRef {
            provider_instance: "mock".into(),
            model: "scripted".into(),
        };
        let configuration = root.put_artifact(&serde_json::to_vec(&json!({
            "policy_revision":"p1", "enabled_tools":["write"],
            "workspace_read":"allow", "workspace_mutation":"ask", "process":"deny", "network":"deny",
            "protected_paths":[], "allowances":[]
        })).unwrap()).unwrap();
        let files = Arc::new(FsWorkspace::open(&workspace, &FsProtectedPaths::default()).unwrap());
        let mut config = DriverConfig::default();
        config.runtime.context.catalog.insert(
            model.clone(),
            ModelLimits {
                context_tokens: Some(128_000),
                max_output_tokens: None,
            },
        );
        let executor: Arc<dyn Executor> = if fail_temporary {
            Arc::new(FailTemporary(files.clone()))
        } else {
            files.clone()
        };
        config.tools = ToolHost {
            workspace: files.clone(),
            executors: vec![executor],
            files: Some(files.clone()),
        };
        let mut steps = vec![ProviderStep::Event(ProviderEvent::Started)];
        for (block_index, path) in ["a", "b"].into_iter().enumerate() {
            let block_index = block_index as u32;
            steps.extend([
                ProviderStep::Event(ProviderEvent::ToolCallStarted {
                    block_index,
                    name: "write".into(),
                    provider_call_id: None,
                }),
                ProviderStep::Event(ProviderEvent::ToolArgumentsDelta {
                    block_index,
                    fragment: json!({"path":path,"text":"after","overwrite":true}).to_string(),
                }),
                ProviderStep::Event(ProviderEvent::ToolCallReady { block_index }),
            ]);
        }
        steps.push(ProviderStep::completed(FinishReason::ToolCalls));
        let provider = Arc::new(ScriptedProvider::new([
            Script::new(steps),
            Script::new([
                ProviderStep::text(0, "done"),
                ProviderStep::completed(FinishReason::Stop),
            ]),
        ]));
        let handle = SessionHandle::create(
            root.clone(),
            SessionId::new(),
            SessionHeader {
                workspace: workspace.to_str().unwrap().into(),
                initial_model: model,
                policy_revision: "p1".into(),
                configuration,
            },
            provider,
            config,
        )
        .await
        .unwrap();
        handle
            .command(Command::StartTurn {
                command_id: CommandId::new(),
                content: vec![ContentBlock::Text { text: "go".into() }],
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let view = handle.view().await.unwrap();
                assert_ne!(view.phase, Phase::Faulted, "{view:?}");
                if let Some(ticket) = view.pending_approval {
                    handle
                        .command(Command::SubmitApproval {
                            approval_id: ticket.approval_id,
                            plan_digest: ticket.plan_digest,
                            decision: Decision::Allow,
                            remember: false,
                        })
                        .await
                        .unwrap();
                    continue;
                }
                if view.phase == Phase::Durable(RuntimeState::Idle) && view.active_turn.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for the fixture turn to finish"));
        Self {
            _temp: temp,
            workspace,
            root,
            handle,
            files,
        }
    }
    async fn preview(&self) -> UndoPreview {
        match self.handle.command(Command::PreviewUndo).await.unwrap() {
            Accepted::UndoPreviewed { preview } => *preview,
            other => panic!("{other:?}"),
        }
    }
    fn command(preview: UndoPreview) -> Command {
        Command::Undo {
            command_id: CommandId::new(),
            expected_sequence: preview.sequence,
            expected_head: preview.head,
            expected_configuration: preview.configuration,
            target_turn_id: preview.target_turn_id,
        }
    }
    fn temporaries(&self) -> usize {
        fs::read_dir(&self.workspace)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".harness-undo-")
            })
            .count()
    }
}

#[tokio::test]
async fn temporary_write_failure_is_not_applied_and_other_edits_remain_undoable() {
    let fixture = Fixture::turn(true).await;
    let view = fixture.handle.view().await.unwrap();
    let effects: Vec<_> = view
        .snapshot
        .entries()
        .filter_map(|(_, entry)| match &entry.entry.payload {
            EntryPayload::ToolResult(result) => result.result.metadata.file_effect,
            _ => None,
        })
        .collect();
    assert_eq!(effects, [FileEffect::Applied, FileEffect::NotApplied]);
    let preview = fixture.preview().await;
    assert_eq!(preview.targets.len(), 1);
    assert_eq!(preview.targets[0].path, "a");
    assert!(matches!(
        fixture
            .handle
            .command(Fixture::command(preview))
            .await
            .unwrap(),
        Accepted::UndoCompleted {
            outcome: UndoOutcome::Succeeded,
            ..
        }
    ));
    assert_eq!(fs::read(fixture.workspace.join("a")).unwrap(), b"before");
    assert_eq!(fs::read(fixture.workspace.join("b")).unwrap(), b"before");
    assert_eq!(fixture.temporaries(), 0);
    within("the session to shut down", fixture.handle.shutdown()).await;
}

#[tokio::test]
async fn staging_precedes_commit_a_and_a_conflict_before_ack_is_not_applied() {
    let fixture = Fixture::turn(false).await;
    let preview = fixture.preview().await;
    let (entered, wait) = tokio::sync::oneshot::channel();
    let (release, resume) = std::sync::mpsc::channel();
    // Honours the gate on drop even if an assertion below fails.
    let _guard = GateGuard(fixture.root.clone(), release.clone());
    fixture.root.io.test.lock().unwrap().gate = Some(Gate {
        point: Point::Written,
        entered,
        release: resume,
    });
    let handle = fixture.handle.clone();
    let mut command = tokio::spawn(async move { handle.command(Fixture::command(preview)).await });
    within("commit A to reach Point::Written", async {
        wait.await.unwrap_or_else(|_| {
            panic!("the undo finished without writing commit A, so staging did not precede it")
        })
    })
    .await;
    // Hook: commit A is written but not acknowledged. Both replacements must
    // already be staged, and neither target has changed yet.
    assert_eq!(
        fixture.temporaries(),
        2,
        "both replacements must be staged before commit A is written"
    );
    assert_eq!(fs::read(fixture.workspace.join("a")).unwrap(), b"after");
    fs::write(fixture.workspace.join("b"), "user edit").unwrap();
    // Let commit A be acknowledged; the post-intent recheck now sees the change.
    release.send(()).unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::select! {
            joined = &mut command => Some(joined.unwrap()),
            _ = async {
                loop {
                    if fixture.handle.view().await.unwrap().phase == Phase::AwaitingResolution {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            } => None,
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for the undo command or an uncertainty record"));
    let Some(reply) = reply else {
        panic!(
            "a conflict found after staging must be refused as undo_conflict, \
             not recorded as durable uncertainty"
        )
    };
    assert!(
        matches!(
            reply,
            Err(Rejection::Undo {
                code: "undo_conflict",
                ..
            })
        ),
        "a conflict after staging must be refused as undo_conflict, got {reply:?}"
    );
    let view = fixture.handle.view().await.unwrap();
    let notes: Vec<_> = view
        .snapshot
        .entries()
        .filter_map(|(_, entry)| match &entry.entry.payload {
            EntryPayload::UndoNote(note) => Some(note.outcome),
            _ => None,
        })
        .collect();
    assert_eq!(notes, [UndoOutcome::NotApplied]);
    assert_eq!(fixture.temporaries(), 0);
    assert_eq!(fs::read(fixture.workspace.join("a")).unwrap(), b"after");
    assert_eq!(fs::read(fixture.workspace.join("b")).unwrap(), b"user edit");
    within("the session to shut down", fixture.handle.shutdown()).await;
}

#[tokio::test]
async fn failed_commit_a_discards_executor_staging() {
    let fixture = Fixture::turn(false).await;
    let preview = fixture.preview().await;
    fixture.root.io.test.lock().unwrap().fail = Some(Point::BeforeWrite);
    assert!(
        within(
            "the undo command to be refused after a failed commit A",
            fixture.handle.command(Fixture::command(preview)),
        )
        .await
        .is_err()
    );
    within("executor staging to be discarded", async {
        while fixture.temporaries() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    for path in ["a", "b"] {
        assert_eq!(fs::read(fixture.workspace.join(path)).unwrap(), b"after");
    }
    within("the session to shut down", fixture.handle.shutdown()).await;
}

impl Fixture {
    /// The exact target set the runtime would restore, prepared as if commit A
    /// had already been acknowledged.
    async fn prepared(&self) -> (crate::tools::fs::PreparedUndo, Vec<UndoTarget>) {
        let snapshot = self.handle.view().await.unwrap().snapshot;
        let UndoSelection::Candidate { targets, .. } = snapshot.select_undo().unwrap() else {
            panic!("a candidate turn")
        };
        let identity = crate::tools::WorkspaceIdentity(self.workspace.to_string_lossy().into());
        let prepared = self
            .files
            .prepare_undo(&identity, &targets, &self.root)
            .unwrap_or_else(|conflicts| panic!("preflight refused: {conflicts:?}"));
        (prepared, targets)
    }
}

/// ADR 0032 step 5, both sides: a recheck refusal that happens before any
/// mutation is `not_applied`; once a rename or unlink is attempted it stays
/// uncertain.
#[tokio::test]
async fn a_first_target_recheck_refusal_is_not_applied_and_a_later_one_is_uncertain() {
    let fixture = Fixture::turn(false).await;
    let (prepared, targets) = fixture.prepared().await;
    let identity = crate::tools::WorkspaceIdentity(fixture.workspace.to_string_lossy().into());
    // Side A: the first target changes after the whole-set recheck and before
    // its own recheck. Nothing was mutated, so the undo is refused as
    // not_applied and no file is claimed unknown.
    let path = fixture.workspace.join("a");
    let mut fired = false;
    let outcome = fixture.files.apply_undo_hooked(
        prepared,
        &identity,
        &targets,
        &fixture.root,
        &mut |target, step| {
            if target == "a" && step == Step::Resolve && !fired {
                fired = true;
                fs::write(&path, "user edit").unwrap();
            }
            Ok(())
        },
    );
    let crate::tools::UndoExecution::Refused(conflicts) = outcome else {
        panic!("a recheck refusal before any mutation is not_applied, got {outcome:?}")
    };
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].path, "a");
    assert_eq!(conflicts[0].reason, "changed");
    assert_eq!(fs::read(&path).unwrap(), b"user edit");
    assert_eq!(fs::read(fixture.workspace.join("b")).unwrap(), b"after");

    // Side B: the first target is rechecked and then the rename attempt fails.
    // A rename may have happened, so this stays uncertain.
    fs::write(&path, "after").unwrap();
    let (prepared, targets) = fixture.prepared().await;
    let outcome = fixture.files.apply_undo_hooked(
        prepared,
        &identity,
        &targets,
        &fixture.root,
        &mut |target, step| {
            if target == "a" && step == Step::Rename {
                return Err(crate::tools::Refusal::new(
                    crate::session::ResultCode::InfrastructureFailure,
                    "injected rename failure",
                ));
            }
            Ok(())
        },
    );
    let crate::tools::UndoExecution::Done { files, complete } = outcome else {
        panic!("a failed mutation is uncertainty, got {outcome:?}")
    };
    assert!(!complete);
    assert_eq!(files[0].path, "a");
    assert_eq!(files[0].outcome, FileOutcome::Unknown);
    assert!(
        files[1..]
            .iter()
            .all(|row| row.outcome == FileOutcome::Unknown)
    );
    assert_eq!(fixture.temporaries(), 0, "staging is never left behind");
    within("the session to shut down", fixture.handle.shutdown()).await;
}
