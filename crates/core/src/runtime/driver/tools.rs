//! Dedicated synchronous planning/execution boundary. No runtime decisions here.
use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, mpsc as std_mpsc},
    thread,
};

use tokio::sync::mpsc;

use super::*;
use crate::{
    session::{CallRef, DataRoot, ExecutionBackend, ResultCode, UndoId, UndoTarget},
    tools::{
        AuthorizedOperation, CancelToken, Executor, FileFacts, Plan, PolicySnapshot, Refusal,
        Registry, ToolSet, ValidatedOperation, WorkspaceIdentity, WorkspacePath, WorkspaceView,
    },
};

/// Trusted workspace and fixed executor backends for one session.
#[derive(Clone)]
pub struct ToolHost {
    /// Workspace facts used only on the executor thread.
    pub workspace: Arc<dyn WorkspaceView + Send + Sync>,
    /// At most one executor per backend; duplicates fail before journal I/O.
    pub executors: Vec<Arc<dyn Executor>>,
    /// The trusted descriptor-rooted workspace, when one is installed. Capture
    /// and undo restoration are capabilities of that concrete backend, not of
    /// the generic planning view.
    pub files: Option<Arc<crate::tools::FsWorkspace>>,
}
impl ToolHost {
    /// No available tools and no executor thread (the default).
    pub fn none() -> Self {
        Self {
            workspace: Arc::new(NoWorkspace(WorkspaceIdentity(String::new()))),
            executors: Vec::new(),
            files: None,
        }
    }

    /// Capture a trusted write/edit pre-image, or refuse without dispatch.
    pub(super) fn capture(
        &self,
        plan: &Plan,
        artifacts: &DataRoot,
    ) -> Result<crate::session::FileImage, Refusal> {
        self.workspace.capture(plan, artifacts)
    }

    /// Prepare one undo without journal or target mutations.
    pub(super) fn prepare_undo(
        &self,
        workspace: &str,
        targets: &[UndoTarget],
        artifacts: &DataRoot,
    ) -> Result<crate::tools::fs::PreparedUndo, Vec<crate::session::UndoConflict>> {
        match &self.files {
            Some(files) => {
                files.prepare_undo(&WorkspaceIdentity(workspace.to_owned()), targets, artifacts)
            }
            None => Err(targets
                .iter()
                .map(|target| crate::session::UndoConflict {
                    path: target.path.clone(),
                    reason: "workspace_unavailable",
                })
                .collect()),
        }
    }

    /// Ephemeral per-file evidence for an uncertain undo.
    pub(super) fn undo_hint(&self, target: &UndoTarget) -> crate::tools::RecoveryHint {
        match &self.files {
            Some(files) => files.undo_hint(target),
            None => crate::tools::RecoveryHint::unavailable("tools_unavailable"),
        }
    }
    pub(super) fn tool_set(&self) -> ToolSet {
        ToolSet::new(self.executors.iter().map(|e| e.backend()))
    }
    pub(super) fn validate(&self) -> Result<(), PersistError> {
        let mut backends = Vec::new();
        for executor in &self.executors {
            let backend = executor.backend();
            if backends.contains(&backend) {
                return Err(PersistError::Rejected {
                    code: "duplicate_executor_backend".into(),
                });
            }
            backends.push(backend);
        }
        Ok(())
    }
}
impl Default for ToolHost {
    fn default() -> Self {
        Self::none()
    }
}
impl fmt::Debug for ToolHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolHost")
            .field(
                "backends",
                &self
                    .executors
                    .iter()
                    .map(|e| e.backend())
                    .collect::<Vec<ExecutionBackend>>(),
            )
            .finish()
    }
}
struct NoWorkspace(WorkspaceIdentity);
impl WorkspaceView for NoWorkspace {
    fn identity(&self) -> &WorkspaceIdentity {
        &self.0
    }
    fn inspect(&self, _: &WorkspacePath) -> Result<FileFacts, Refusal> {
        Err(unavailable())
    }
    fn read_text(&self, _: &WorkspacePath, _: u64) -> Result<String, Refusal> {
        Err(unavailable())
    }
}
fn unavailable() -> Refusal {
    Refusal::new(ResultCode::InfrastructureFailure, "workspace unavailable")
}

pub(super) enum Job {
    /// Drop executor-owned staging when its runtime is invalidated.
    UndoDiscard,
    /// Verify and stage before commit A.
    UndoPrepare {
        epoch: RuntimeEpoch,
        token: UndoToken,
        workspace: String,
        targets: Vec<UndoTarget>,
        artifacts: Arc<DataRoot>,
    },
    /// Capture a trusted write/edit pre-image through the workspace.
    Capture {
        epoch: RuntimeEpoch,
        token: CaptureToken,
        plan: Box<Plan>,
        artifacts: Arc<DataRoot>,
    },
    /// Restore one undo's targets on this sequential worker.
    UndoRestore {
        epoch: RuntimeEpoch,
        token: UndoToken,
        workspace: String,
        targets: Vec<UndoTarget>,
        artifacts: Arc<DataRoot>,
    },
    /// Compare an uncertain undo's targets with the current workspace.
    UndoHints {
        epoch: RuntimeEpoch,
        token: HintToken,
        undo_id: UndoId,
        targets: Vec<UndoTarget>,
    },
    Hints {
        epoch: RuntimeEpoch,
        token: HintToken,
        plans: Vec<(crate::session::InvocationId, Result<Vec<u8>, String>)>,
    },
    Plan {
        epoch: RuntimeEpoch,
        token: PlanToken,
        call: CallRef,
        tool: String,
        operation: ValidatedOperation,
        policy: PolicySnapshot,
    },
    Execute {
        epoch: RuntimeEpoch,
        token: ExecutionToken,
        operation: AuthorizedOperation,
        cancel: CancelToken,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum JobToken {
    Plan(PlanToken),
    Execute(ExecutionToken),
    Hints(HintToken),
    Capture(CaptureToken),
    Undo(UndoToken),
}
pub(super) struct Answer {
    pub epoch: RuntimeEpoch,
    pub token: JobToken,
    /// None means an execution outcome is unknown. Never fabricate a result.
    pub input: Option<Input>,
}
pub(super) struct PendingJob {
    pub epoch: RuntimeEpoch,
    pub token: JobToken,
    pub cancel: Option<CancelToken>,
}
pub(super) struct ToolWorker {
    pub send: std_mpsc::Sender<Job>,
    pub thread: thread::JoinHandle<()>,
}
impl ToolWorker {
    pub fn start(
        host: ToolHost,
        answers: mpsc::UnboundedSender<Answer>,
    ) -> Result<Option<Self>, PersistError> {
        if host.executors.is_empty() {
            return Ok(None);
        }
        let (send, jobs) = std_mpsc::channel();
        let thread = thread::Builder::new()
            .name("harness-tools".into())
            .spawn(move || {
                let registry = Registry::builtin();
                let mut staged = None;
                while let Ok(job) = jobs.recv() {
                    let answer = match job {
                        Job::UndoDiscard => {
                            staged = None;
                            continue;
                        }
                        Job::UndoPrepare {
                            epoch,
                            token,
                            workspace,
                            targets,
                            artifacts,
                        } => {
                            // One staging owner, keyed by both epoch and undo token.
                            staged = None;
                            let result = catch_unwind(AssertUnwindSafe(|| {
                                host.prepare_undo(&workspace, &targets, &artifacts)
                            }))
                            .unwrap_or_else(|_| {
                                Err(targets
                                    .iter()
                                    .map(|target| crate::session::UndoConflict {
                                        path: target.path.clone(),
                                        reason: "staging_failed",
                                    })
                                    .collect())
                            });
                            let result = result.map(|prepared| {
                                staged = Some((epoch.clone(), token, prepared));
                            });
                            Answer {
                                epoch,
                                token: JobToken::Undo(token),
                                input: Some(Input::UndoPrepared { token, result }),
                            }
                        }
                        Job::Capture {
                            epoch,
                            token,
                            plan,
                            artifacts,
                        } => {
                            let result = catch_unwind(AssertUnwindSafe(|| {
                                host.capture(plan.as_ref(), artifacts.as_ref())
                            }))
                            .unwrap_or_else(|_| {
                                Err(Refusal::new(
                                    ResultCode::InfrastructureFailure,
                                    "workspace capture panicked",
                                ))
                            });
                            Answer {
                                epoch,
                                token: JobToken::Capture(token),
                                input: Some(Input::Captured { token, result }),
                            }
                        }
                        Job::UndoRestore {
                            epoch,
                            token,
                            workspace,
                            targets,
                            artifacts,
                        } => {
                            let prepared =
                                staged.take().filter(|(e, t, _)| e == &epoch && *t == token);
                            let result =
                                catch_unwind(AssertUnwindSafe(|| match (prepared, &host.files) {
                                    (Some((_, _, prepared)), Some(files)) => files.apply_undo(
                                        prepared,
                                        &WorkspaceIdentity(workspace),
                                        &targets,
                                        &artifacts,
                                    ),
                                    _ => crate::tools::UndoExecution::Done {
                                        files: Vec::new(),
                                        complete: false,
                                    },
                                }));
                            Answer {
                                epoch,
                                token: JobToken::Undo(token),
                                input: result
                                    .ok()
                                    .map(|result| Input::UndoFinished { token, result }),
                            }
                        }
                        Job::UndoHints {
                            epoch,
                            token,
                            undo_id,
                            targets,
                        } => {
                            let hints = targets
                                .iter()
                                .map(|target| (target.path.clone(), host.undo_hint(target)))
                                .collect();
                            Answer {
                                epoch,
                                token: JobToken::Hints(token),
                                input: Some(Input::UndoHints {
                                    token,
                                    undo_id,
                                    hints,
                                }),
                            }
                        }
                        Job::Hints {
                            epoch,
                            token,
                            plans,
                        } => {
                            let hints = plans
                                .into_iter()
                                .map(|(id, bytes)| {
                                    let hint = match bytes {
                                        Err(reason) => {
                                            crate::tools::RecoveryHint::unavailable(reason)
                                        }
                                        Ok(bytes) => catch_unwind(AssertUnwindSafe(|| {
                                            crate::tools::recovery_hint(
                                                &bytes,
                                                host.workspace.as_ref(),
                                            )
                                        }))
                                        .unwrap_or_else(|_| {
                                            crate::tools::RecoveryHint::unavailable(
                                                "infrastructure_failure",
                                            )
                                        }),
                                    };
                                    (id, hint)
                                })
                                .collect();
                            Answer {
                                epoch,
                                token: JobToken::Hints(token),
                                input: Some(Input::Hints { token, hints }),
                            }
                        }
                        Job::Plan {
                            epoch,
                            token,
                            call,
                            tool,
                            operation,
                            policy,
                        } => {
                            let result = catch_unwind(AssertUnwindSafe(|| {
                                registry.resolve(&tool, &policy)?.plan(
                                    &operation,
                                    &call,
                                    host.workspace.as_ref(),
                                    &policy,
                                )
                            }))
                            .unwrap_or_else(|_| {
                                Err(Refusal::new(
                                    ResultCode::InfrastructureFailure,
                                    "workspace planning panicked",
                                ))
                            });
                            Answer {
                                epoch,
                                token: JobToken::Plan(token),
                                input: Some(Input::Planned { token, result }),
                            }
                        }
                        Job::Execute {
                            epoch,
                            token,
                            operation,
                            cancel,
                        } => {
                            let outcome = catch_unwind(AssertUnwindSafe(|| {
                                host.executors
                                    .iter()
                                    .find(|e| e.backend() == operation.plan().backend())
                                    .map(|e| e.execute(&operation, &cancel))
                            }))
                            .ok()
                            .flatten();
                            Answer {
                                epoch,
                                token: JobToken::Execute(token),
                                input: outcome
                                    .map(|outcome| Input::ToolFinished { token, outcome }),
                            }
                        }
                    };
                    if answers.send(answer).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| PersistError::Io {
                message: e.to_string(),
            })?;
        Ok(Some(Self { send, thread }))
    }
    pub async fn shutdown(self) {
        drop(self.send);
        let _ = tokio::task::spawn_blocking(move || self.thread.join()).await;
    }
}
