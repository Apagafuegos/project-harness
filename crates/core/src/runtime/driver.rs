//! Async plumbing. All decisions still belong to `Runtime::step`.

use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};

use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::{JoinHandle, JoinSet},
};

use super::{
    worker::{StorageWorker, WorkerOptions},
    *,
};
use crate::session::{ArtifactRef, DataRoot, SessionHeader, SessionId};

type CommandResult = Result<Accepted, Rejection>;
type PendingStorage = Pin<Box<dyn Future<Output = Input> + Send>>;

/// Channel and artifact bounds for one driver, separate from core limits.
#[derive(Clone, Debug)]
pub struct DriverConfig {
    /// Limits interpreted exclusively by the pure core.
    pub runtime: RuntimeConfig,
    /// Mailbox capacity. `command` rejects immediately when full.
    pub command_capacity: usize,
    /// Broadcast capacity; lagging subscribers automatically reset.
    pub event_capacity: usize,
    /// Bounded worker mailbox and maximum concurrent artifact requests.
    pub worker_capacity: usize,
    /// Maximum artifact upload size, checked before any I/O.
    pub max_artifact_bytes: usize,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            runtime: RuntimeConfig::default(),
            command_capacity: 16,
            event_capacity: 64,
            worker_capacity: 8,
            max_artifact_bytes: 16 * 1024 * 1024,
        }
    }
}

enum Message {
    Command(Command, oneshot::Sender<CommandResult>),
    View(oneshot::Sender<RuntimeView>),
    PutArtifact(Vec<u8>, oneshot::Sender<Result<ArtifactRef, PersistError>>),
    Reopen(oneshot::Sender<Result<RuntimeView, PersistError>>),
    Shutdown(oneshot::Sender<()>),
}

/// Cheaply cloned access to the one task owning a session runtime.
#[derive(Clone)]
pub struct SessionHandle {
    commands: mpsc::Sender<Message>,
    events: broadcast::WeakSender<RuntimeEvent>,
    max_artifact_bytes: usize,
}

impl SessionHandle {
    /// Open using the caller's Tokio runtime. No runtime is created here.
    /// The private stand-in reports `provider_unavailable` until step 4.
    pub async fn open(
        root: Arc<DataRoot>,
        session: SessionId,
        config: DriverConfig,
    ) -> Result<Self, PersistError> {
        Self::launch(
            root,
            session,
            None,
            config,
            SystemEnv,
            UnavailableProvider,
            WorkerOptions::default(),
        )
        .await
    }

    /// Create and sync a new session on its dedicated worker thread.
    pub async fn create(
        root: Arc<DataRoot>,
        session: SessionId,
        header: SessionHeader,
        config: DriverConfig,
    ) -> Result<Self, PersistError> {
        let mut env = SystemEnv;
        let at = env.now();
        Self::launch(
            root,
            session,
            Some((header, at)),
            config,
            env,
            UnavailableProvider,
            WorkerOptions::default(),
        )
        .await
    }

    pub(super) async fn launch<E: Env + Send + 'static, P: Provider + Send + 'static>(
        root: Arc<DataRoot>,
        session: SessionId,
        create: Option<(SessionHeader, crate::session::Timestamp)>,
        config: DriverConfig,
        mut env: E,
        provider: P,
        options: WorkerOptions,
    ) -> Result<Self, PersistError> {
        let (worker, snapshot) = StorageWorker::start(
            root.clone(),
            session.clone(),
            create,
            config.worker_capacity,
            options.clone(),
        )
        .await?;
        let runtime = Runtime::new(snapshot, config.runtime.clone(), &mut env);
        let (commands, receive) = mpsc::channel(config.command_capacity.max(1));
        let (events, _) = broadcast::channel(config.event_capacity.max(1));
        let handle = Self {
            commands,
            events: events.downgrade(),
            max_artifact_bytes: config.max_artifact_bytes,
        };
        tokio::spawn(
            Driver::new(
                root, session, worker, runtime, config, env, provider, options, events, receive,
            )
            .run(),
        );
        Ok(handle)
    }

    /// Submit without waiting for mailbox space. Acceptance follows durability.
    pub async fn command(&self, command: Command) -> CommandResult {
        let (reply, receive) = oneshot::channel();
        self.commands
            .try_send(Message::Command(command, reply))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Rejection::Overloaded,
                mpsc::error::TrySendError::Closed(_) => Rejection::Faulted,
            })?;
        receive.await.unwrap_or(Err(Rejection::Faulted))
    }

    /// Begin the gap-free subscription protocol immediately, before taking a view.
    pub fn subscribe(&self) -> Subscription {
        Subscription {
            commands: self.commands.downgrade(),
            events: self.events.upgrade().map(|events| events.subscribe()),
            client: None,
        }
    }

    /// Take a coherent view between core steps.
    pub async fn view(&self) -> Result<RuntimeView, Rejection> {
        request_view(&self.commands).await.ok_or(Rejection::Faulted)
    }

    /// Publish an artifact, replying only after file and directory sync.
    pub async fn put_artifact(&self, bytes: Vec<u8>) -> Result<ArtifactRef, PersistError> {
        if bytes.len() > self.max_artifact_bytes {
            return Err(PersistError::Rejected {
                code: "artifact_too_large".into(),
            });
        }
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Message::PutArtifact(bytes, reply))
            .await
            .map_err(|_| PersistError::WorkerLost)?;
        receive.await.unwrap_or(Err(PersistError::WorkerLost))
    }

    /// Replace a faulted runtime after shutting down its worker. A torn tail is
    /// reported unchanged; this never performs implicit journal repair.
    pub async fn reopen(&self) -> Result<RuntimeView, PersistError> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Message::Reopen(reply))
            .await
            .map_err(|_| PersistError::WorkerLost)?;
        receive.await.unwrap_or(Err(PersistError::WorkerLost))
    }

    /// Cancel inference, finish pending durability work, release the journal
    /// lock and await worker exit. Concurrent shutdown callers all await exit.
    pub async fn shutdown(&self) {
        let (reply, receive) = oneshot::channel();
        if self.commands.send(Message::Shutdown(reply)).await.is_ok() {
            let _ = receive.await;
        } else {
            self.commands.closed().await;
        }
    }
}

/// One gap-free observation. A reset replaces the entire client view.
#[derive(Clone, Debug)]
pub enum SubscriptionItem {
    /// Initial view, or resync after lag, an epoch change or a sequence gap.
    Reset(ClientView),
    /// Next event in the current runtime incarnation.
    Event(RuntimeEvent),
}

/// Maintains its own reducer to detect gaps and runtime replacement.
pub struct Subscription {
    commands: mpsc::WeakSender<Message>,
    events: Option<broadcast::Receiver<RuntimeEvent>>,
    client: Option<ClientView>,
}

impl Subscription {
    /// Yield a reset or the next contiguous event; `None` means driver shutdown.
    pub async fn next(&mut self) -> Option<SubscriptionItem> {
        loop {
            let events = self.events.as_mut()?;
            if self.client.is_none() {
                let commands = self.commands.upgrade()?;
                let view = request_view(&commands).await?;
                let client = ClientView::from(view);
                self.client = Some(client.clone());
                return Some(SubscriptionItem::Reset(client));
            }
            match events.recv().await {
                Ok(event) => match self.client.as_mut()?.apply(&event) {
                    ApplyOutcome::Applied => return Some(SubscriptionItem::Event(event)),
                    ApplyOutcome::Duplicate => {}
                    ApplyOutcome::NeedsResync => {
                        self.events = Some(events.resubscribe());
                        self.client = None;
                    }
                },
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.events = Some(events.resubscribe());
                    self.client = None;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

async fn request_view(commands: &mpsc::Sender<Message>) -> Option<RuntimeView> {
    let (reply, receive) = oneshot::channel();
    commands.send(Message::View(reply)).await.ok()?;
    receive.await.ok()
}

/// Minimal internal seam. Step 4 replaces this with the provider Stream trait.
pub(super) trait Provider {
    fn start(&mut self, request: InferenceRequest) -> mpsc::Receiver<ProviderEvent>;
}

/// Exactly the data carried by StartInference, with no context decisions.
#[derive(Clone, Debug)]
pub(super) struct InferenceRequest {
    pub inference: InferenceId,
    pub turn_id: crate::session::TurnId,
    pub head: crate::session::EntryId,
    pub model: crate::session::ModelRef,
    pub response_id: crate::session::ResponseId,
}

struct UnavailableProvider;
impl Provider for UnavailableProvider {
    fn start(&mut self, request: InferenceRequest) -> mpsc::Receiver<ProviderEvent> {
        let InferenceRequest {
            inference,
            turn_id,
            head,
            model,
            response_id,
        } = request;
        let _ = (inference, turn_id, head, model, response_id);
        let (send, receive) = mpsc::channel(1);
        let _ = send.try_send(ProviderEvent::Finished {
            outcome: ProviderOutcome::Failed,
            finish_reason: None,
            error: Some(crate::session::ModelError {
                code: crate::session::ModelErrorCode::ProviderFailure,
                message: "provider_unavailable: step 4 is not implemented".into(),
                provider_code: None,
                retry_after_ms: None,
            }),
        });
        receive
    }
}

struct Driver<E, P> {
    root: Arc<DataRoot>,
    session: SessionId,
    worker: Option<StorageWorker>,
    runtime: Runtime,
    config: DriverConfig,
    env: E,
    provider: P,
    options: WorkerOptions,
    events: broadcast::Sender<RuntimeEvent>,
    commands: mpsc::Receiver<Message>,
    replies: BTreeMap<RequestId, oneshot::Sender<CommandResult>>,
    next_request: u64,
    storage: Option<PendingStorage>,
    inference: Option<(InferenceId, mpsc::Receiver<ProviderEvent>)>,
    timers: BTreeMap<TimerId, JoinHandle<()>>,
    timer_send: mpsc::UnboundedSender<TimerId>,
    timer_receive: mpsc::UnboundedReceiver<TimerId>,
    artifacts: JoinSet<Result<(), PersistError>>,
    shutdown: Vec<oneshot::Sender<()>>,
    closing: bool,
    cancel_sent: bool,
}

impl<E: Env + Send + 'static, P: Provider + Send + 'static> Driver<E, P> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        root: Arc<DataRoot>,
        session: SessionId,
        worker: StorageWorker,
        runtime: Runtime,
        config: DriverConfig,
        env: E,
        provider: P,
        options: WorkerOptions,
        events: broadcast::Sender<RuntimeEvent>,
        commands: mpsc::Receiver<Message>,
    ) -> Self {
        let (timer_send, timer_receive) = mpsc::unbounded_channel();
        Self {
            root,
            session,
            worker: Some(worker),
            runtime,
            config,
            env,
            provider,
            options,
            events,
            commands,
            replies: BTreeMap::new(),
            next_request: 0,
            storage: None,
            inference: None,
            timers: BTreeMap::new(),
            timer_send,
            timer_receive,
            artifacts: JoinSet::new(),
            shutdown: Vec::new(),
            closing: false,
            cancel_sent: false,
        }
    }

    async fn run(mut self) {
        loop {
            if self.closing {
                if !self.cancel_sent
                    && let Some(turn_id) = self.runtime.view().active_turn
                    && matches!(self.runtime.phase(), Phase::Durable(_))
                {
                    self.cancel_sent = true;
                    self.next_request += 1;
                    self.step(Input::Command {
                        request: RequestId(self.next_request),
                        command: Command::Cancel { turn_id },
                    });
                }
                if self.storage.is_none()
                    && (self.runtime.view().active_turn.is_none()
                        || matches!(
                            self.runtime.phase(),
                            Phase::Faulted | Phase::RecoveryRequired
                        ))
                {
                    break;
                }
            }
            tokio::select! {
                biased;
                input = async { self.storage.as_mut().expect("guarded").await }, if self.storage.is_some() => {
                    self.storage = None;
                    self.step(input);
                }
                _ = async { (&mut self.worker.as_mut().expect("guarded").exited).await }, if self.worker.is_some() => {
                    self.worker = None;
                    self.step(Input::StorageFailed { error: PersistError::WorkerLost });
                }
                event = async { self.inference.as_mut().expect("guarded").1.recv().await }, if self.inference.is_some() => {
                    let inference = self.inference.as_ref().expect("guarded").0;
                    match event {
                        Some(event) => self.step(Input::Provider { inference, event }),
                        None => { self.inference = None; self.step(Input::ProviderEnded { inference }); }
                    }
                }
                Some(timer) = self.timer_receive.recv() => {
                    self.timers.remove(&timer);
                    self.step(Input::TimerFired { timer });
                }
                result = self.artifacts.join_next(), if !self.artifacts.is_empty() => {
                    match result {
                        Some(Ok(Err(error))) => self.step(Input::StorageFailed { error }),
                        Some(Err(_)) => self.step(Input::StorageFailed { error: PersistError::WorkerLost }),
                        _ => {},
                    }
                },
                message = self.commands.recv(), if !self.closing => {
                    match message {
                        Some(message) => self.message(message).await,
                        None => self.closing = true,
                    }
                }
            }
        }
        self.inference = None;
        for (_, timer) in std::mem::take(&mut self.timers) {
            timer.abort();
        }
        while self.artifacts.join_next().await.is_some() {}
        if let Some(worker) = self.worker.take() {
            worker.shutdown().await;
        }
        // Close only after exit: every concurrent shutdown waits for this point.
        self.commands.close();
        while let Ok(message) = self.commands.try_recv() {
            if let Message::Shutdown(reply) = message {
                self.shutdown.push(reply);
            }
        }
        for reply in self.shutdown {
            let _ = reply.send(());
        }
    }

    fn step(&mut self, input: Input) {
        for effect in self.runtime.step(input, &mut self.env) {
            match effect {
                Effect::Persist {
                    token,
                    commit,
                    command,
                    at,
                } => {
                    let sender = self.worker.as_ref().map(|worker| worker.sender.clone());
                    self.storage = Some(Box::pin(async move {
                        let result = match sender {
                            Some(sender) => sender.append(commit, command, at).await,
                            None => Err(PersistError::WorkerLost),
                        };
                        match result {
                            Ok((ack, snapshot)) => Input::Persisted {
                                token,
                                ack,
                                snapshot,
                            },
                            Err(error) => Input::PersistFailed { token, error },
                        }
                    }));
                }
                Effect::VerifyArtifacts { token, refs } => {
                    let sender = self.worker.as_ref().map(|worker| worker.sender.clone());
                    self.storage = Some(Box::pin(async move {
                        let result = match sender {
                            Some(sender) => sender.verify(refs).await,
                            None => Err(PersistError::WorkerLost),
                        };
                        match result {
                            Ok(missing) => Input::ArtifactsVerified { token, missing },
                            Err(error) => Input::PersistFailed { token, error },
                        }
                    }));
                }
                Effect::StartInference {
                    inference,
                    turn_id,
                    head,
                    model,
                    response_id,
                } => {
                    self.inference = Some((
                        inference,
                        self.provider.start(InferenceRequest {
                            inference,
                            turn_id,
                            head,
                            model,
                            response_id,
                        }),
                    ));
                }
                Effect::CancelInference { inference } => {
                    if self
                        .inference
                        .as_ref()
                        .is_some_and(|(id, _)| *id == inference)
                    {
                        self.inference = None;
                    }
                }
                Effect::SetTimer { timer, duration } => {
                    let send = self.timer_send.clone();
                    let deadline = tokio::time::Instant::now() + duration;
                    let task = tokio::spawn(async move {
                        tokio::time::sleep_until(deadline).await;
                        let _ = send.send(timer);
                    });
                    self.timers.insert(timer, task);
                }
                Effect::CancelTimer { timer } => {
                    if let Some(task) = self.timers.remove(&timer) {
                        task.abort();
                    }
                }
                Effect::Emit(event) => {
                    let _ = self.events.send(event);
                }
                Effect::Reply { request, result } => {
                    if let Some(reply) = self.replies.remove(&request) {
                        let _ = reply.send(result);
                    }
                }
            }
        }
        // A terminal core fault also disposes of an already-ended transport whose
        // sender failed to close, before local inference IDs can be reused.
        if self.runtime.phase() == Phase::Faulted {
            self.inference = None;
        }
    }

    async fn message(&mut self, message: Message) {
        match message {
            Message::Command(command, reply) => {
                self.next_request += 1;
                let request = RequestId(self.next_request);
                self.replies.insert(request, reply);
                self.step(Input::Command { request, command });
            }
            Message::View(reply) => {
                let _ = reply.send(self.runtime.view());
            }
            Message::PutArtifact(bytes, reply) => {
                if self.runtime.phase() == Phase::Faulted {
                    let _ = reply.send(Err(PersistError::Rejected {
                        code: "runtime_faulted".into(),
                    }));
                } else if self.artifacts.len() >= self.config.worker_capacity.max(1) {
                    let _ = reply.send(Err(PersistError::Rejected {
                        code: "overloaded".into(),
                    }));
                } else if let Some(worker) = &self.worker {
                    let sender = worker.sender.clone();
                    self.artifacts.spawn(async move {
                        let result = sender.put_artifact(bytes).await;
                        let failure = match &result {
                            Err(
                                error @ (PersistError::Io { .. }
                                | PersistError::Poisoned
                                | PersistError::WorkerLost),
                            ) => Err(error.clone()),
                            _ => Ok(()),
                        };
                        let _ = reply.send(result);
                        failure
                    });
                } else {
                    let _ = reply.send(Err(PersistError::WorkerLost));
                }
            }
            Message::Reopen(reply) => {
                let result = if self.runtime.phase() != Phase::Faulted {
                    Err(PersistError::Rejected {
                        code: "not_faulted".into(),
                    })
                } else {
                    // An uncertain commit is never cancelled or repeated.
                    if let Some(storage) = self.storage.take() {
                        let _ = storage.await;
                    }
                    while self.artifacts.join_next().await.is_some() {}
                    if let Some(worker) = self.worker.take() {
                        worker.shutdown().await;
                    }
                    match StorageWorker::start(
                        self.root.clone(),
                        self.session.clone(),
                        None,
                        self.config.worker_capacity,
                        self.options.clone(),
                    )
                    .await
                    {
                        Ok((worker, snapshot)) => {
                            self.worker = Some(worker);
                            self.runtime =
                                Runtime::new(snapshot, self.config.runtime.clone(), &mut self.env);
                            self.step(Input::PublishView);
                            Ok(self.runtime.view())
                        }
                        Err(error) => Err(error),
                    }
                };
                let _ = reply.send(result);
            }
            Message::Shutdown(reply) => {
                self.shutdown.push(reply);
                self.closing = true;
            }
        }
    }
}

#[cfg(test)]
mod tests;
