//! A dedicated thread owns the writer, including opening and dropping it.

use std::{sync::Arc, thread};

use tokio::sync::{mpsc, oneshot};

use crate::context::SessionConfiguration;
use crate::session::{
    ArtifactRef, CommandId, Commit, CommitAck, DataRoot, DiagnosticKind, Digest, JournalStore,
    SessionHeader, SessionId, SessionSnapshot, StoreError, Timestamp,
};

use super::{PersistError, TornTail};

type AppendResult = Result<(CommitAck, SessionSnapshot), PersistError>;
type Opened = Result<(SessionSnapshot, SessionConfiguration), PersistError>;

enum Request {
    Append {
        commit: Box<Commit>,
        command: Option<(CommandId, serde_json::Value)>,
        artifacts: Vec<Vec<u8>>,
        at: Timestamp,
        reply: oneshot::Sender<AppendResult>,
    },
    PutArtifact {
        bytes: Vec<u8>,
        reply: oneshot::Sender<Result<ArtifactRef, PersistError>>,
    },
    VerifyArtifacts {
        refs: Vec<ArtifactRef>,
        reply: oneshot::Sender<Result<Vec<ArtifactRef>, PersistError>>,
    },
}

/// How the worker obtains its journal.
pub(super) enum Opening {
    /// Open an existing journal. A torn tail fails with its inspection.
    Open,
    /// Create and sync a new session.
    Create(SessionHeader, Timestamp),
    /// Repair the inspected torn tail, then open the repaired journal.
    Repair {
        fingerprint: Digest,
        size: u64,
        at: Timestamp,
    },
}

/// The request half of the worker. Only driver-owned work retains clones.
#[derive(Clone)]
pub(super) struct WorkerSender(mpsc::Sender<Request>);

impl WorkerSender {
    pub(super) async fn append(
        &self,
        commit: Commit,
        command: Option<(CommandId, serde_json::Value)>,
        artifacts: Vec<Vec<u8>>,
        at: Timestamp,
    ) -> AppendResult {
        let (reply, receive) = oneshot::channel();
        self.0
            .send(Request::Append {
                commit: Box::new(commit),
                command,
                artifacts,
                at,
                reply,
            })
            .await
            .map_err(|_| PersistError::WorkerLost)?;
        receive.await.unwrap_or(Err(PersistError::WorkerLost))
    }

    pub(super) async fn put_artifact(&self, bytes: Vec<u8>) -> Result<ArtifactRef, PersistError> {
        let (reply, receive) = oneshot::channel();
        self.0
            .send(Request::PutArtifact { bytes, reply })
            .await
            .map_err(|_| PersistError::WorkerLost)?;
        receive.await.unwrap_or(Err(PersistError::WorkerLost))
    }

    pub(super) async fn verify(
        &self,
        refs: Vec<ArtifactRef>,
    ) -> Result<Vec<ArtifactRef>, PersistError> {
        let (reply, receive) = oneshot::channel();
        self.0
            .send(Request::VerifyArtifacts { refs, reply })
            .await
            .map_err(|_| PersistError::WorkerLost)?;
        receive.await.unwrap_or(Err(PersistError::WorkerLost))
    }
}

/// One named OS thread owns the journal for its entire lifetime.
/// Dropping every request sender drains queued requests and releases the lock.
pub struct StorageWorker {
    pub(super) sender: WorkerSender,
    pub(super) exited: oneshot::Receiver<()>,
}

/// Test-only fault switches. Worker switches are applied on the worker, never
/// on Tokio; `duplicate_storage_next` is applied by the driver.
#[derive(Clone, Default)]
pub(super) struct WorkerOptions {
    #[cfg(test)]
    pub without_oracle: bool,
    #[cfg(test)]
    pub panic_next: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    pub lost_reply_next: Arc<std::sync::atomic::AtomicBool>,
    /// Repeat the next storage effect, as if the core had asked for two.
    #[cfg(test)]
    pub duplicate_storage_next: Arc<std::sync::atomic::AtomicBool>,
}

impl StorageWorker {
    /// Open on the worker thread itself. Failure leaves no live writer/thread.
    /// The session's configuration artifact must decode; see
    /// [`SessionConfiguration::parse`].
    pub async fn open(
        root: Arc<DataRoot>,
        session_id: SessionId,
    ) -> Result<(Self, SessionSnapshot), PersistError> {
        Self::start(root, session_id, Opening::Open, 8, WorkerOptions::default())
            .await
            .map(|(worker, snapshot, _)| (worker, snapshot))
    }

    /// Create and sync the header on the worker thread itself.
    pub async fn create(
        root: Arc<DataRoot>,
        session_id: SessionId,
        header: SessionHeader,
        at: Timestamp,
    ) -> Result<(Self, SessionSnapshot), PersistError> {
        Self::start(
            root,
            session_id,
            Opening::Create(header, at),
            8,
            WorkerOptions::default(),
        )
        .await
        .map(|(worker, snapshot, _)| (worker, snapshot))
    }

    pub(super) async fn start(
        root: Arc<DataRoot>,
        session_id: SessionId,
        opening: Opening,
        capacity: usize,
        options: WorkerOptions,
    ) -> Result<(Self, SessionSnapshot, SessionConfiguration), PersistError> {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        let (opened, receive) = oneshot::channel();
        let (exit, exited) = oneshot::channel();
        thread::Builder::new()
            .name(format!("harness-storage-{session_id}"))
            .spawn(move || {
                // The receiver, store and file all drop before the exit notification,
                // including when unwinding. No panic crosses this thread boundary.
                let run = || run(root, session_id, opening, receiver, opened, &options);
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    #[cfg(test)]
                    if options.without_oracle {
                        return crate::session::state::without_oracle(run);
                    }
                    run();
                }));
                let _ = exit.send(());
            })
            .map_err(|error| PersistError::Io {
                message: error.to_string(),
            })?;
        let worker = Self {
            sender: WorkerSender(sender),
            exited,
        };
        match receive.await.unwrap_or(Err(PersistError::WorkerLost)) {
            Ok((snapshot, configuration)) => Ok((worker, snapshot, configuration)),
            Err(error) => {
                worker.shutdown().await;
                Err(error)
            }
        }
    }

    /// Drain queued operations, drop the writer and await its exit signal.
    pub async fn shutdown(self) {
        drop(self.sender);
        let _ = self.exited.await;
    }
}

fn run(
    root: Arc<DataRoot>,
    session_id: SessionId,
    opening: Opening,
    mut requests: mpsc::Receiver<Request>,
    opened: oneshot::Sender<Opened>,
    options: &WorkerOptions,
) {
    let result = match opening {
        Opening::Create(header, at) => {
            JournalStore::create(root.clone(), session_id.clone(), header, at)
        }
        Opening::Open => JournalStore::open(root.clone(), &session_id),
        Opening::Repair {
            fingerprint,
            size,
            at,
        } => JournalStore::repair_inspected_tail(root.clone(), &session_id, &fingerprint, size, at),
    };
    let mut store = match result {
        Ok(store) => store,
        Err(error) => {
            let error = match error {
                StoreError::Journal(diagnostic) if diagnostic.kind == DiagnosticKind::TornTail => {
                    torn_tail(&root, &session_id, diagnostic)
                }
                error => PersistError::from_store(&error),
            };
            let _ = opened.send(Err(error));
            return;
        }
    };
    // The context builder reads the configuration artifact's instructions;
    // the pure core cannot, so it is decoded once here, under the lock.
    let snapshot = store.snapshot();
    let configuration = match SessionConfiguration::load(&root, snapshot.configuration()) {
        Ok(configuration) => configuration,
        Err(error) => {
            let _ = opened.send(Err(PersistError::Rejected {
                code: error.code().into(),
            }));
            return;
        }
    };
    if opened.send(Ok((snapshot, configuration))).is_err() {
        return;
    }
    let mut poisoned = false;
    while let Some(request) = requests.blocking_recv() {
        #[cfg(not(test))]
        let _ = options;
        #[cfg(test)]
        if options
            .panic_next
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            panic!("injected storage worker panic");
        }
        match request {
            Request::Append {
                mut commit,
                command,
                artifacts,
                at,
                reply,
            } => {
                let result = (|| {
                    if poisoned {
                        return Err(StoreError::Poisoned);
                    }
                    for bytes in artifacts {
                        root.put_artifact(&bytes)?;
                    }
                    if let Some((id, command)) = command {
                        store.bind_command(&mut commit, id, &command)?;
                    }
                    let ack = store.append(*commit, at)?;
                    Ok((ack, store.snapshot()))
                })();
                if matches!(result, Err(StoreError::Io(_) | StoreError::Poisoned)) {
                    poisoned = true;
                }
                #[cfg(test)]
                if options
                    .lost_reply_next
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    drop(reply);
                    continue;
                }
                let _ = reply.send(result.map_err(|error| PersistError::from_store(&error)));
            }
            Request::PutArtifact { bytes, reply } => {
                let result = if poisoned {
                    Err(StoreError::Poisoned)
                } else {
                    root.put_artifact(&bytes)
                };
                if matches!(result, Err(StoreError::Io(_))) {
                    poisoned = true;
                }
                let _ = reply.send(result.map_err(|error| PersistError::from_store(&error)));
            }
            Request::VerifyArtifacts { refs, reply } => {
                let result = (|| {
                    if poisoned {
                        return Err(StoreError::Poisoned);
                    }
                    let mut missing = Vec::new();
                    for reference in refs {
                        match root.verify_artifact(&reference) {
                            Ok(()) => {}
                            Err(error @ StoreError::Io(_)) => return Err(error),
                            Err(_) => missing.push(reference),
                        }
                    }
                    Ok(missing)
                })();
                if matches!(result, Err(StoreError::Io(_) | StoreError::Poisoned)) {
                    poisoned = true;
                }
                let _ = reply.send(result.map_err(|error| PersistError::from_store(&error)));
            }
        }
    }
}

/// Inspect a journal whose open failed on a torn tail, so the caller can show
/// the user what repair would quarantine. Opening released its lock; if the
/// file changed meanwhile, the newer finding is reported instead.
fn torn_tail(
    root: &DataRoot,
    session_id: &SessionId,
    diagnostic: crate::session::Diagnostic,
) -> PersistError {
    match JournalStore::inspect(root, session_id) {
        Ok(inspection)
            if inspection
                .diagnostic
                .as_ref()
                .is_some_and(|d| d.kind == DiagnosticKind::TornTail) =>
        {
            PersistError::TornTail(TornTail(Box::new(inspection)))
        }
        Ok(_) => PersistError::from_store(&StoreError::Journal(diagnostic)),
        Err(error) => PersistError::from_store(&error),
    }
}
