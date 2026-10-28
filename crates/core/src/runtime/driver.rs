//! Async plumbing. All decisions still belong to `Runtime::step`.

use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};

use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

use super::{
    worker::{Opening, StorageWorker, WorkerOptions},
    *,
};
use crate::{
    context::ContextReport,
    model::{ModelEventStream, ModelProvider, ModelRequest, OperationControl},
    session::{ArtifactRef, DataRoot, Digest, EntryId, ModelRef, SessionHeader, SessionId},
};

mod tools;
use crate::tools::{CancelToken, PolicySnapshot, WorkspacePath};
pub use tools::ToolHost;
use tools::{Answer, Job, JobToken, PendingJob, ToolWorker};

type CommandResult = Result<Accepted, Rejection>;
type PendingStorage = Pin<Box<dyn Future<Output = Input> + Send>>;
/// Timer fires carry their runtime's epoch: `TimerId`s restart in a new runtime.
type TimerFire = (RuntimeEpoch, TimerId);

/// Channel and artifact bounds for one driver, separate from core limits.
#[derive(Clone, Debug)]
pub struct DriverConfig {
    /// Limits interpreted exclusively by the pure core.
    pub runtime: RuntimeConfig,
    /// Trusted planning workspace and executor backends (none by default).
    pub tools: ToolHost,
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
            tools: ToolHost::none(),
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
    InspectContext(
        Option<EntryId>,
        Option<ModelRef>,
        oneshot::Sender<Result<ContextReport, InspectError>>,
    ),
    PutArtifact(Vec<u8>, oneshot::Sender<Result<ArtifactRef, PersistError>>),
    Reopen(oneshot::Sender<Result<RuntimeView, PersistError>>),
    RepairTail(
        Digest,
        u64,
        oneshot::Sender<Result<RuntimeView, RepairError>>,
    ),
    Shutdown(oneshot::Sender<()>),
    /// Test-only access to the real timer channel, to inject stale fires.
    #[cfg(test)]
    TimerSender(oneshot::Sender<mpsc::UnboundedSender<TimerFire>>),
    #[cfg(test)]
    Inject(Input, oneshot::Sender<()>),
    #[cfg(test)]
    ToolAnswer(Answer, oneshot::Sender<()>),
    #[cfg(test)]
    InstallConfiguration(crate::context::SessionConfiguration, oneshot::Sender<()>),
    #[cfg(test)]
    Offered(oneshot::Sender<Vec<Vec<String>>>),
}

/// Cheaply cloned access to the one task owning a session runtime.
#[derive(Clone)]
pub struct SessionHandle {
    commands: mpsc::Sender<Message>,
    events: broadcast::WeakSender<RuntimeEvent>,
    max_artifact_bytes: usize,
    availability: Option<ToolAvailability>,
}

impl SessionHandle {
    /// Open using the caller's Tokio runtime. No runtime is created here.
    /// Every inference of this session streams from `provider`. A session
    /// interrupted by a crash is recovered at once: see `Input::Start`. A
    /// torn tail fails with `PersistError::TornTail` and is never repaired
    /// implicitly; see [`SessionHandle::repair_tail`].
    pub async fn open(
        root: Arc<DataRoot>,
        session: SessionId,
        provider: Arc<dyn ModelProvider>,
        config: DriverConfig,
    ) -> Result<Self, PersistError> {
        Self::open_with_env(root, session, provider, config, SystemEnv).await
    }

    /// Open storage first, then assemble real tools from its current policy.
    /// A preinstalled `config.tools` is rejected, never merged. Invalid policy
    /// installs nothing and fails the first turn with `policy_invalid`.
    pub async fn open_with_tools(
        root: Arc<DataRoot>,
        session: SessionId,
        provider: Arc<dyn ModelProvider>,
        config: DriverConfig,
        host: &ToolHostConfig,
    ) -> Result<Self, PersistError> {
        Self::launch_with_host(
            root,
            session,
            Opening::Open,
            config,
            SystemEnv,
            provider,
            WorkerOptions::default(),
            Some(host.clone()),
        )
        .await
    }

    /// Create storage, then assemble real tools from the header's policy.
    pub async fn create_with_tools(
        root: Arc<DataRoot>,
        session: SessionId,
        header: SessionHeader,
        provider: Arc<dyn ModelProvider>,
        config: DriverConfig,
        host: &ToolHostConfig,
    ) -> Result<Self, PersistError> {
        Self::launch_with_host(
            root,
            session,
            Opening::Create(header, SystemEnv.now()),
            config,
            SystemEnv,
            provider,
            WorkerOptions::default(),
            Some(host.clone()),
        )
        .await
    }

    /// Startup report for handles opened/created with real tools. Legacy
    /// caller-supplied hosts have no probed availability report.
    pub fn tool_availability(&self) -> Option<&ToolAvailability> {
        self.availability.as_ref()
    }

    /// Repair the torn tail the user inspected, then open the session as
    /// `open` does. `expected_fingerprint` and `expected_size` are the whole
    /// file's SHA-256 and length from that inspection (`TornTail` or
    /// `JournalStore::inspect`). On the worker thread, under the writer lock,
    /// storage rescans the file, refuses a changed one, quarantines the tail
    /// into an artifact, truncates and syncs, and commits `tail_repaired`.
    pub async fn repair_tail(
        root: Arc<DataRoot>,
        session: SessionId,
        expected_fingerprint: Digest,
        expected_size: u64,
        provider: Arc<dyn ModelProvider>,
        config: DriverConfig,
    ) -> Result<Self, RepairError> {
        let at = SystemEnv.now();
        Self::launch(
            root,
            session,
            Opening::Repair {
                fingerprint: expected_fingerprint,
                size: expected_size,
                at,
            },
            config,
            SystemEnv,
            provider,
            WorkerOptions::default(),
        )
        .await
        .map_err(repair_error)
    }

    /// Create and sync a new session on its dedicated worker thread.
    pub async fn create(
        root: Arc<DataRoot>,
        session: SessionId,
        header: SessionHeader,
        provider: Arc<dyn ModelProvider>,
        config: DriverConfig,
    ) -> Result<Self, PersistError> {
        Self::create_with_env(root, session, header, provider, config, SystemEnv).await
    }

    /// `open`, with time and identities from `env` instead of the wall clock
    /// and random UUIDs. Meant for deterministic tests, examples, and replay;
    /// applications should use `open`. Give each process that writes a
    /// journal its own identity stream.
    pub async fn open_with_env<E: Env + Send + 'static>(
        root: Arc<DataRoot>,
        session: SessionId,
        provider: Arc<dyn ModelProvider>,
        config: DriverConfig,
        env: E,
    ) -> Result<Self, PersistError> {
        Self::launch(
            root,
            session,
            Opening::Open,
            config,
            env,
            provider,
            WorkerOptions::default(),
        )
        .await
    }

    /// `create`, with the header time and every later identity from `env`.
    /// Meant for deterministic tests, examples, and replay; applications
    /// should use `create`.
    pub async fn create_with_env<E: Env + Send + 'static>(
        root: Arc<DataRoot>,
        session: SessionId,
        header: SessionHeader,
        provider: Arc<dyn ModelProvider>,
        config: DriverConfig,
        mut env: E,
    ) -> Result<Self, PersistError> {
        let at = env.now();
        Self::launch(
            root,
            session,
            Opening::Create(header, at),
            config,
            env,
            provider,
            WorkerOptions::default(),
        )
        .await
    }

    pub(super) async fn launch<E: Env + Send + 'static>(
        root: Arc<DataRoot>,
        session: SessionId,
        opening: Opening,
        config: DriverConfig,
        env: E,
        provider: Arc<dyn ModelProvider>,
        options: WorkerOptions,
    ) -> Result<Self, PersistError> {
        Self::launch_with_host(root, session, opening, config, env, provider, options, None).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn launch_with_host<E: Env + Send + 'static>(
        root: Arc<DataRoot>,
        session: SessionId,
        opening: Opening,
        mut config: DriverConfig,
        mut env: E,
        provider: Arc<dyn ModelProvider>,
        options: WorkerOptions,
        host: Option<ToolHostConfig>,
    ) -> Result<Self, PersistError> {
        config.tools.validate()?;
        if let Some(host) = &host {
            let code = if !config.tools.executors.is_empty()
                || !config.tools.workspace.identity().0.is_empty()
            {
                Some("tool_host_conflict")
            } else if !host.data_root.is_absolute() || !host.workspace_root.is_absolute() {
                Some("absolute_path_required")
            } else if super::host::location(&host.data_root).is_none()
                || super::host::location(root.path()).is_none()
            {
                Some("root_resolution_failed")
            } else if super::host::location(&host.data_root) != super::host::location(root.path()) {
                Some("tool_host_data_root_mismatch")
            } else {
                None
            };
            if let Some(code) = code {
                return Err(PersistError::Rejected { code: code.into() });
            }
        }
        let (worker, snapshot, configuration) = StorageWorker::start(
            root.clone(),
            session.clone(),
            opening,
            config.worker_capacity,
            options.clone(),
        )
        .await?;
        let mut protected = None;
        let require_tool_policy = host.is_some();
        let availability = if let Some(host) = host {
            if super::host::location(&host.workspace_root)
                != super::host::location(std::path::Path::new(&snapshot.header().workspace))
            {
                worker.shutdown().await;
                return Err(PersistError::Rejected {
                    code: "tool_host_workspace_mismatch".into(),
                });
            }
            let policy =
                PolicySnapshot::parse(configuration.artifact().clone(), configuration.bytes())
                    .ok()
                    .filter(|policy| policy.revision() == snapshot.policy_revision());
            if let Some(policy) = policy {
                protected = Some(policy.protected_paths().to_vec());
                let assembled =
                    tokio::task::spawn_blocking(move || ToolHost::assemble(&host, &policy)).await;
                match assembled {
                    Ok((tools, availability)) => {
                        config.tools = tools;
                        Some(availability)
                    }
                    Err(_) => {
                        worker.shutdown().await;
                        return Err(PersistError::WorkerLost);
                    }
                }
            } else {
                Some(ToolAvailability::unavailable("policy_invalid"))
            }
        } else {
            None
        };
        let mut runtime = Runtime::with_tools(
            snapshot,
            configuration,
            config.runtime.clone(),
            config.tools.tool_set(),
            &mut env,
        );
        runtime.require_tool_policy = require_tool_policy;
        let (tool_send, tool_receive) = mpsc::unbounded_channel();
        let tool_worker = match ToolWorker::start(config.tools.clone(), tool_send) {
            Ok(worker) => worker,
            Err(error) => {
                worker.shutdown().await;
                return Err(error);
            }
        };
        let (commands, receive) = mpsc::channel(config.command_capacity.max(1));
        let (events, _) = broadcast::channel(config.event_capacity.max(1));
        let handle = Self {
            commands,
            events: events.downgrade(),
            max_artifact_bytes: config.max_artifact_bytes,
            availability,
        };
        let mut driver = Driver::new(
            root,
            session,
            worker,
            runtime,
            config,
            env,
            provider,
            options,
            events,
            receive,
            tool_worker,
            tool_receive,
        );
        driver.host_protected = protected;
        driver.require_tool_policy = require_tool_policy;
        tokio::spawn(driver.run());
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

    #[cfg(test)]
    async fn timer_sender(&self) -> mpsc::UnboundedSender<TimerFire> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Message::TimerSender(reply))
            .await
            .unwrap();
        receive.await.unwrap()
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

    /// The context report for `head` (`None` means the current head), from
    /// the snapshot published when the driver answers, between core steps.
    /// Any entry can be the head, so a client can inspect another branch
    /// without navigating to it. With `model_override` the report is a
    /// preview for that model. Nothing is persisted and the head never moves.
    pub async fn inspect_context(
        &self,
        head: Option<EntryId>,
        model_override: Option<ModelRef>,
    ) -> Result<ContextReport, InspectError> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Message::InspectContext(head, model_override, reply))
            .await
            .map_err(|_| InspectError::Closed)?;
        receive.await.unwrap_or(Err(InspectError::Closed))
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

    /// Replace a faulted runtime after shutting down its worker, then recover
    /// as `open` does. A torn tail is returned as `PersistError::TornTail`
    /// with its inspection; this never performs implicit journal repair.
    pub async fn reopen(&self) -> Result<RuntimeView, PersistError> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Message::Reopen(reply))
            .await
            .map_err(|_| PersistError::WorkerLost)?;
        receive.await.unwrap_or(Err(PersistError::WorkerLost))
    }

    /// Repair a faulted handle's torn tail in place, bound to the inspected
    /// fingerprint and size, then reopen with a new epoch and recover. The
    /// refusals are those of [`SessionHandle::repair_tail`]; a handle that is
    /// not faulted is refused with `NotFaulted`.
    pub async fn reopen_repairing_tail(
        &self,
        expected_fingerprint: Digest,
        expected_size: u64,
    ) -> Result<RuntimeView, RepairError> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Message::RepairTail(
                expected_fingerprint,
                expected_size,
                reply,
            ))
            .await
            .map_err(|_| PersistError::WorkerLost)?;
        receive
            .await
            .unwrap_or(Err(PersistError::WorkerLost.into()))
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

/// Storage refusals of a repair become typed; anything else stays a failure.
fn repair_error(error: PersistError) -> RepairError {
    match &error {
        PersistError::Rejected { code } => match code.as_str() {
            "stale_repair" => RepairError::StaleRepair,
            "tail_not_repairable" => RepairError::NotRepairable,
            "session_locked" => RepairError::Locked,
            _ => RepairError::Persist(error),
        },
        _ => RepairError::Persist(error),
    }
}

async fn request_view(commands: &mpsc::Sender<Message>) -> Option<RuntimeView> {
    let (reply, receive) = oneshot::channel();
    commands.send(Message::View(reply)).await.ok()?;
    receive.await.ok()
}

/// The current stream and its token. Dropping it cancels the token first, so
/// an adapter's own tasks stop even if dropping the stream alone would not.
struct Live {
    id: InferenceId,
    stream: ModelEventStream,
    cancel: CancellationToken,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct Driver<E> {
    root: Arc<DataRoot>,
    session: SessionId,
    worker: Option<StorageWorker>,
    runtime: Runtime,
    config: DriverConfig,
    env: E,
    provider: Arc<dyn ModelProvider>,
    options: WorkerOptions,
    events: broadcast::Sender<RuntimeEvent>,
    commands: mpsc::Receiver<Message>,
    replies: BTreeMap<RequestId, oneshot::Sender<CommandResult>>,
    next_request: u64,
    storage: Option<PendingStorage>,
    inference: Option<Live>,
    timers: BTreeMap<TimerId, JoinHandle<()>>,
    timer_send: mpsc::UnboundedSender<TimerFire>,
    timer_receive: mpsc::UnboundedReceiver<TimerFire>,
    artifacts: JoinSet<Result<(), PersistError>>,
    shutdown: Vec<oneshot::Sender<()>>,
    closing: bool,
    /// Every sender is gone; `recv` would return `None` at once.
    mailbox_closed: bool,
    cancel_sent: bool,
    tool_worker: Option<ToolWorker>,
    tool_receive: mpsc::UnboundedReceiver<Answer>,
    tool_pending: Option<PendingJob>,
    tool_closed: bool,
    hint_load: Option<Pin<Box<dyn Future<Output = Job> + Send>>>,
    hint_ready: Option<Job>,
    deferred_tool: Option<(Job, PendingJob)>,
    host_protected: Option<Vec<WorkspacePath>>,
    require_tool_policy: bool,
    #[cfg(test)]
    offered: Vec<Vec<String>>,
}

impl<E: Env + Send + 'static> Driver<E> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        root: Arc<DataRoot>,
        session: SessionId,
        worker: StorageWorker,
        runtime: Runtime,
        config: DriverConfig,
        env: E,
        provider: Arc<dyn ModelProvider>,
        options: WorkerOptions,
        events: broadcast::Sender<RuntimeEvent>,
        commands: mpsc::Receiver<Message>,
        tool_worker: Option<ToolWorker>,
        tool_receive: mpsc::UnboundedReceiver<Answer>,
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
            mailbox_closed: false,
            cancel_sent: false,
            tool_worker,
            tool_receive,
            tool_pending: None,
            tool_closed: false,
            hint_load: None,
            hint_ready: None,
            deferred_tool: None,
            host_protected: None,
            require_tool_policy: false,
            #[cfg(test)]
            offered: Vec::new(),
        }
    }

    async fn run(mut self) {
        // Before any message: no view is taken between construction and Start.
        self.step(Input::Start);
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
                    && self.tool_pending.is_none()
                    && self.deferred_tool.is_none()
                    && (self.runtime.view().active_turn.is_none()
                        || matches!(
                            self.runtime.phase(),
                            Phase::Faulted | Phase::RecoveryRequired | Phase::AwaitingResolution
                        ))
                {
                    break;
                }
            }
            self.dispatch_hints();
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
                answer = self.tool_receive.recv(), if self.tool_worker.is_some() && !self.tool_closed => {
                    self.tool_answer(answer);
                }
                job = async { self.hint_load.as_mut().expect("guarded").await }, if self.hint_load.is_some() => {
                    self.hint_load = None;
                    self.hint_ready = Some(job);
                }
                Some((epoch, timer)) = self.timer_receive.recv() => {
                    // A fire from a replaced runtime could alias a new `TimerId`.
                    if epoch == *self.runtime.epoch() {
                        self.timers.remove(&timer);
                        self.step(Input::TimerFired { timer });
                    }
                }
                result = self.artifacts.join_next(), if !self.artifacts.is_empty() => {
                    match result {
                        Some(Ok(Err(error))) => self.step(Input::StorageFailed { error }),
                        Some(Err(_)) => self.step(Input::StorageFailed { error: PersistError::WorkerLost }),
                        _ => {},
                    }
                },
                // Still read while draining, so late commands get `ShuttingDown`.
                message = self.commands.recv(), if !self.mailbox_closed => {
                    match message {
                        Some(message) => self.message(message).await,
                        None => {
                            self.mailbox_closed = true;
                            self.closing = true;
                        }
                    }
                }
                // Last, so timers and commands (a cancel) are never starved by
                // a stream that is always ready. Pull-driven: it produces only
                // when polled here, and each event spends Tokio's coop budget,
                // so the task yields even if every other branch is idle.
                event = std::future::poll_fn(|cx| {
                    let coop = std::task::ready!(tokio::task::coop::poll_proceed(cx));
                    let event = std::task::ready!(
                        self.inference.as_mut().expect("guarded").stream.as_mut().poll_next(cx)
                    );
                    coop.made_progress();
                    std::task::Poll::Ready(event)
                }), if self.inference.is_some() => {
                    let inference = self.inference.as_ref().expect("guarded").id;
                    match event {
                        Some(event) => self.step(Input::Provider { inference, event }),
                        None => { self.inference = None; self.step(Input::ProviderEnded { inference }); }
                    }
                }
            }
        }
        self.inference = None;
        self.hint_load = None;
        self.hint_ready = None;
        for (_, timer) in std::mem::take(&mut self.timers) {
            timer.abort();
        }
        while self.artifacts.join_next().await.is_some() {}
        if let Some(worker) = self.tool_worker.take() {
            worker.shutdown().await;
        }
        if let Some(worker) = self.worker.take() {
            worker.shutdown().await;
        }
        // Close only after exit: every concurrent shutdown waits for this point.
        // Messages queued since the loop ended are refused while still closing.
        self.commands.close();
        while let Ok(message) = self.commands.try_recv() {
            self.message(message).await;
        }
        for reply in self.shutdown {
            let _ = reply.send(());
        }
    }

    fn step(&mut self, input: Input) {
        #[allow(unused_mut)]
        let mut effects = self.runtime.step(input, &mut self.env);
        // Check immediately after an applied configuration, before dispatching
        // any of its effects (especially Execute following a remembered grant).
        if self.host_is_stale() && self.runtime.phase() != Phase::Faulted {
            // These events already updated the core's client view. Publish
            // them in sequence, but suppress every side effect before faulting.
            effects = effects
                .into_iter()
                .filter_map(|effect| match effect {
                    Effect::Emit(_) => Some(effect),
                    Effect::Reply { request, .. } => Some(Effect::Reply {
                        request,
                        result: Err(Rejection::Faulted),
                    }),
                    _ => None,
                })
                .collect();
            effects.extend(self.runtime.step(
                Input::DriverInvariant {
                    code: "tool_host_stale",
                },
                &mut self.env,
            ));
        }
        #[cfg(test)]
        if let Some(at) = effects.iter().position(|effect| {
            matches!(
                effect,
                Effect::Persist { .. } | Effect::VerifyArtifacts { .. }
            )
        }) && self
            .options
            .duplicate_storage_next
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            let duplicate = effects[at].clone();
            effects.insert(at + 1, duplicate);
        }
        // The core keeps at most one storage operation outstanding. If it ever
        // asks for a second, keep the first (it may already be written), skip
        // the second, and let the core fault rather than drop an answer.
        let mut overlap = false;
        let mut executor_error = None;
        for effect in effects {
            match effect {
                Effect::Persist {
                    token,
                    commit,
                    command,
                    artifacts,
                    at,
                } => {
                    if self.storage.is_some() {
                        overlap = true;
                        continue;
                    }
                    let sender = self.worker.as_ref().map(|worker| worker.sender.clone());
                    self.storage = Some(Box::pin(async move {
                        let result = match sender {
                            Some(sender) => sender.append(commit, command, artifacts, at).await,
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
                    if self.storage.is_some() {
                        overlap = true;
                        continue;
                    }
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
                // The core built the projection in `Preparing`; the driver
                // only hands it to the provider with the response identity.
                Effect::StartInference {
                    inference,
                    input,
                    response_id,
                    tools: _definitions,
                    ..
                } => {
                    #[cfg(test)]
                    self.offered
                        .push(_definitions.into_iter().map(|d| d.name).collect());
                    let cancel = CancellationToken::new();
                    let request = ModelRequest { response_id, input };
                    let control = OperationControl {
                        cancel: cancel.clone(),
                    };
                    self.inference = Some(Live {
                        id: inference,
                        stream: self.provider.stream(request, control),
                        cancel,
                    });
                }
                // Keep polling: the core waits for the stream to confirm.
                Effect::CancelInference { inference } => {
                    if let Some(live) = &self.inference
                        && live.id == inference
                    {
                        live.cancel.cancel();
                    }
                }
                Effect::ReleaseInference { inference } => {
                    if self
                        .inference
                        .as_ref()
                        .is_some_and(|live| live.id == inference)
                    {
                        self.inference = None;
                    }
                }
                Effect::SetTimer { timer, duration } => {
                    let send = self.timer_send.clone();
                    let epoch = self.runtime.epoch().clone();
                    let deadline = tokio::time::Instant::now() + duration;
                    let task = tokio::spawn(async move {
                        tokio::time::sleep_until(deadline).await;
                        let _ = send.send((epoch, timer));
                    });
                    self.timers.insert(timer, task);
                }
                Effect::CancelTimer { timer } => {
                    if let Some(task) = self.timers.remove(&timer) {
                        task.abort();
                    }
                }
                Effect::InspectUncertain { token, invocations } => {
                    let epoch = self.runtime.epoch().clone();
                    let sender = self.worker.as_ref().map(|w| w.sender.clone());
                    let limit = self.config.max_artifact_bytes;
                    self.hint_load = Some(Box::pin(async move {
                        let mut plans = Vec::new();
                        for (id, reference) in invocations {
                            let bytes = match &sender {
                                Some(sender) => sender
                                    .read_artifact(reference, limit)
                                    .await
                                    .map_err(|_| "artifact_unavailable".to_owned()),
                                None => Err("artifact_unavailable".into()),
                            };
                            plans.push((id, bytes));
                        }
                        Job::Hints {
                            epoch,
                            token,
                            plans,
                        }
                    }));
                }
                Effect::PlanCall {
                    token,
                    call,
                    tool,
                    operation,
                } => {
                    let epoch = self.runtime.epoch().clone();
                    let policy = self.runtime.policy().ok().cloned();
                    if self
                        .tool_pending
                        .as_ref()
                        .is_some_and(|p| !matches!(p.token, JobToken::Hints(_)))
                    {
                        executor_error = Some("executor_overlap");
                    } else if let Some(policy) = policy {
                        let pending = PendingJob {
                            epoch: epoch.clone(),
                            token: JobToken::Plan(token),
                            cancel: None,
                        };
                        if self
                            .send_tool(
                                Job::Plan {
                                    epoch,
                                    token,
                                    call,
                                    tool,
                                    operation,
                                    policy,
                                },
                                pending,
                            )
                            .is_err()
                        {
                            executor_error = Some("executor_lost");
                        }
                    } else {
                        executor_error = Some("executor_lost");
                    }
                }
                Effect::Execute { token, operation } => {
                    let epoch = self.runtime.epoch().clone();
                    let cancel = CancelToken::new();
                    if self
                        .tool_pending
                        .as_ref()
                        .is_some_and(|p| !matches!(p.token, JobToken::Hints(_)))
                    {
                        executor_error = Some("executor_overlap");
                    } else {
                        let pending = PendingJob {
                            epoch: epoch.clone(),
                            token: JobToken::Execute(token),
                            cancel: Some(cancel.clone()),
                        };
                        if self
                            .send_tool(
                                Job::Execute {
                                    epoch,
                                    token,
                                    operation,
                                    cancel,
                                },
                                pending,
                            )
                            .is_err()
                        {
                            executor_error = Some("executor_lost");
                        }
                    }
                }
                Effect::CancelExecution { token } => {
                    if self.tool_worker.is_none() {
                        executor_error = Some("executor_lost");
                    }
                    if let Some(pending) = &self.tool_pending
                        && pending.token == JobToken::Execute(token)
                        && let Some(cancel) = &pending.cancel
                    {
                        cancel.cancel();
                    }
                    if let Some((_, pending)) = &self.deferred_tool
                        && pending.token == JobToken::Execute(token)
                        && let Some(cancel) = &pending.cancel
                    {
                        cancel.cancel();
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
        if let Some(code) = executor_error {
            self.step(Input::DriverInvariant { code });
        }
        if overlap {
            self.step(Input::DriverInvariant {
                code: "storage_overlap",
            });
        }
        // A terminal core fault also disposes of an already-ended transport whose
        // sender failed to close, before local inference IDs can be reused.
        if self.runtime.phase() == Phase::Faulted {
            self.inference = None;
            self.deferred_tool = None;
            self.hint_load = None;
            self.hint_ready = None;
        }
    }

    fn host_is_stale(&self) -> bool {
        self.host_protected.as_ref().is_some_and(|protected| {
            self.runtime
                .policy()
                .is_ok_and(|policy| policy.protected_paths() != protected)
        })
    }

    fn dispatch_hints(&mut self) {
        if self.tool_pending.is_some() {
            return;
        }
        if let Some((job, pending)) = self.deferred_tool.take() {
            if self.send_tool(job, pending).is_err() {
                self.step(Input::DriverInvariant {
                    code: "executor_lost",
                });
            }
            return;
        }
        let Some(Job::Hints {
            epoch,
            token,
            plans,
        }) = self.hint_ready.take()
        else {
            return;
        };
        if epoch != *self.runtime.epoch() {
            return;
        }
        if self.tool_worker.is_none() || self.tool_closed {
            let hints = plans
                .into_iter()
                .map(|(id, _)| (id, RecoveryHint::unavailable("tools_unavailable")))
                .collect();
            self.step(Input::Hints { token, hints });
            return;
        }
        let fallback: Vec<_> = plans
            .iter()
            .map(|(id, _)| {
                (
                    id.clone(),
                    RecoveryHint::unavailable("infrastructure_failure"),
                )
            })
            .collect();
        if self
            .send_tool(
                Job::Hints {
                    epoch: epoch.clone(),
                    token,
                    plans,
                },
                PendingJob {
                    epoch,
                    token: JobToken::Hints(token),
                    cancel: None,
                },
            )
            .is_err()
        {
            self.step(Input::Hints {
                token,
                hints: fallback,
            });
        }
    }

    fn send_tool(&mut self, job: Job, pending: PendingJob) -> Result<(), ()> {
        if self
            .tool_pending
            .as_ref()
            .is_some_and(|p| matches!(p.token, JobToken::Hints(_)))
        {
            if self.deferred_tool.is_some() {
                return Err(());
            }
            self.deferred_tool = Some((job, pending));
            return Ok(());
        }
        let worker = self.tool_worker.as_ref().ok_or(())?;
        if self.tool_closed {
            return Err(());
        }
        worker.send.send(job).map_err(|_| ())?;
        self.tool_pending = Some(pending);
        Ok(())
    }

    fn tool_answer(&mut self, answer: Option<Answer>) {
        let Some(answer) = answer else {
            self.tool_closed = true;
            self.tool_pending = None;
            self.step(Input::DriverInvariant {
                code: "executor_lost",
            });
            return;
        };
        if answer.epoch != *self.runtime.epoch() {
            return;
        }
        if self
            .tool_pending
            .as_ref()
            .is_some_and(|p| p.epoch == answer.epoch && p.token == answer.token)
        {
            self.tool_pending = None;
        }
        match answer.input {
            Some(input) => self.step(input),
            None => self.step(Input::DriverInvariant {
                code: "executor_lost",
            }),
        }
    }

    /// Stop the faulted runtime's worker and attach a new runtime over the
    /// journal `opening` yields, with a new epoch, then let it recover.
    async fn replace(&mut self, opening: Opening) -> Result<RuntimeView, PersistError> {
        // Drop advisory worker senders before draining the storage worker.
        self.hint_load = None;
        self.hint_ready = None;
        // A fault may have requested cancellation without receiving cleanup.
        // Await the thread's answer before storage can label the intent uncertain.
        while self.tool_pending.is_some() {
            let answer = self.tool_receive.recv().await;
            self.tool_answer(answer);
        }
        // An uncertain commit is never cancelled or repeated.
        if let Some(storage) = self.storage.take() {
            let _ = storage.await;
        }
        while self.artifacts.join_next().await.is_some() {}
        if let Some(worker) = self.worker.take() {
            worker.shutdown().await;
        }
        // The fault cancelled the old runtime's timer; drop any left.
        for (_, timer) in std::mem::take(&mut self.timers) {
            timer.abort();
        }
        let (worker, snapshot, configuration) = StorageWorker::start(
            self.root.clone(),
            self.session.clone(),
            opening,
            self.config.worker_capacity,
            self.options.clone(),
        )
        .await?;
        self.worker = Some(worker);
        self.runtime = Runtime::with_tools(
            snapshot,
            configuration,
            self.config.runtime.clone(),
            self.config.tools.tool_set(),
            &mut self.env,
        );
        self.runtime.require_tool_policy = self.require_tool_policy;
        self.step(Input::PublishView);
        self.step(Input::Start);
        Ok(self.runtime.view())
    }

    async fn message(&mut self, message: Message) {
        let shutting_down = || PersistError::Rejected {
            code: "shutting_down".into(),
        };
        match message {
            Message::Command(_, reply) if self.closing => {
                let _ = reply.send(Err(Rejection::ShuttingDown));
            }
            Message::PutArtifact(_, reply) if self.closing => {
                let _ = reply.send(Err(shutting_down()));
            }
            Message::Reopen(reply) if self.closing => {
                let _ = reply.send(Err(shutting_down()));
            }
            Message::RepairTail(_, _, reply) if self.closing => {
                let _ = reply.send(Err(shutting_down().into()));
            }
            Message::Command(command, reply) => {
                self.next_request += 1;
                let request = RequestId(self.next_request);
                self.replies.insert(request, reply);
                self.step(Input::Command { request, command });
            }
            Message::View(reply) => {
                let _ = reply.send(self.runtime.view());
            }
            Message::InspectContext(head, model, reply) => {
                let _ = reply.send(
                    self.runtime
                        .inspect_context(head, model)
                        .map_err(InspectError::from),
                );
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
                    self.replace(Opening::Open).await
                };
                let _ = reply.send(result);
            }
            Message::RepairTail(fingerprint, size, reply) => {
                let result = if self.runtime.phase() != Phase::Faulted {
                    Err(RepairError::NotFaulted)
                } else {
                    let at = self.env.now();
                    self.replace(Opening::Repair {
                        fingerprint,
                        size,
                        at,
                    })
                    .await
                    .map_err(repair_error)
                };
                let _ = reply.send(result);
            }
            Message::Shutdown(reply) => {
                self.shutdown.push(reply);
                self.closing = true;
            }
            #[cfg(test)]
            Message::InstallConfiguration(configuration, reply) => {
                self.runtime.install_configuration_for_test(configuration);
                self.step(Input::PublishView);
                let _ = reply.send(());
            }
            #[cfg(test)]
            Message::Offered(reply) => {
                let _ = reply.send(self.offered.clone());
            }
            #[cfg(test)]
            Message::Inject(input, reply) => {
                self.step(input);
                let _ = reply.send(());
            }
            #[cfg(test)]
            Message::ToolAnswer(answer, reply) => {
                self.tool_answer(Some(answer));
                let _ = reply.send(());
            }
            #[cfg(test)]
            Message::TimerSender(reply) => {
                let _ = reply.send(self.timer_send.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests;
