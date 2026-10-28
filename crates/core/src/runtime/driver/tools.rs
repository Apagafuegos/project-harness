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
    session::{CallRef, ExecutionBackend, ResultCode},
    tools::{
        AuthorizedOperation, CancelToken, Executor, FileFacts, PolicySnapshot, Refusal, Registry,
        ToolSet, ValidatedOperation, WorkspaceIdentity, WorkspacePath, WorkspaceView,
    },
};

/// Trusted workspace and fixed executor backends for one session.
#[derive(Clone)]
pub struct ToolHost {
    /// Workspace facts used only on the executor thread.
    pub workspace: Arc<dyn WorkspaceView + Send + Sync>,
    /// At most one executor per backend; duplicates fail before journal I/O.
    pub executors: Vec<Arc<dyn Executor>>,
}
impl ToolHost {
    /// No available tools and no executor thread (the default).
    pub fn none() -> Self {
        Self {
            workspace: Arc::new(NoWorkspace(WorkspaceIdentity(String::new()))),
            executors: Vec::new(),
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
                while let Ok(job) = jobs.recv() {
                    let answer = match job {
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
