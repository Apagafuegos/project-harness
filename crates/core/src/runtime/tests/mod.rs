//! Runtime tests against real storage. Every commit the core emits is appended
//! to a `JournalStore` with the test-build oracle on; any rejection fails.

mod compaction;
mod controls;
mod model;
pub(crate) mod pipeline;
mod random;
mod recovery;
mod scenarios;
mod tool_gates;
mod tool_recovery;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use crate::context::{ModelLimits, SessionConfiguration};
use crate::runtime::*;
use crate::session::{io::Point, *};
use crate::tools::tests::FakeWorkspace;
use crate::tools::{
    AuthorizedOperation, CancelToken, ExecutionOutcome, Executor, ToolSet, ValidatedOperation,
};
use pipeline::{StubExecutor, fake_workspace};

/// The first `stream` used by `DeterministicEnv`; reopening uses the next one.
const FIRST_STREAM: u16 = 1;
/// Stream of the test client's command IDs, disjoint from every runtime's.
const CLIENT_STREAM: u64 = 0xfff;

/// A client command identity that no runtime stream can produce.
fn command_id(n: u64) -> CommandId {
    let value = (0x019a_0000_0000_u128 << 80)
        | (0x7 << 76)
        | (u128::from(CLIENT_STREAM) << 64)
        | (0b10 << 62)
        | u128::from(n);
    uuid::Uuid::from_u128(value).to_string().parse().unwrap()
}

fn text(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text { text: text.into() }]
}

/// Inputs and outputs of one `Runtime`, from construction to the end or a reopen.
struct Segment {
    stream: u16,
    configuration: SessionConfiguration,
    config: RuntimeConfig,
    tools: ToolSet,
    initial: SessionSnapshot,
    inputs: Vec<Input>,
    times: Vec<Timestamp>,
    effects: Vec<Effect>,
    /// The view before the first input and after each one.
    views: Vec<RuntimeView>,
}

/// How a simulated process dies with a commit in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Crash {
    /// The commit was never written.
    Lost,
    /// The commit is durable, but its acknowledgement never arrived.
    Durable,
    /// Half of the commit was written: a torn tail.
    Torn,
}

/// The commit handed to the storage worker and not yet answered.
struct Pending {
    token: CommitToken,
    commit: Commit,
    command: Option<(CommandId, serde_json::Value)>,
    at: Timestamp,
    artifacts: Vec<Vec<u8>>,
}

/// A synchronous stand-in for the step-3 driver: it owns the runtime, a real
/// store in a temporary directory and the deterministic environment. Provider
/// events, timers and failures are fed in by the test; nothing reads a clock.
struct Driver {
    _temp: tempfile::TempDir,
    root: Arc<DataRoot>,
    store: Option<JournalStore>,
    session: SessionId,
    runtime: Runtime,
    env: DeterministicEnv,
    segment: Segment,
    done: Vec<Segment>,
    pending: Option<Pending>,
    verification: Option<(CommitToken, Vec<ArtifactRef>)>,
    /// Failure to inject into the next completed persist.
    fail: Option<Point>,
    /// A commit whose persist failed with I/O; it may still be durable.
    failed: Option<Commit>,
    requests: BTreeSet<RequestId>,
    replies: BTreeMap<RequestId, Result<Accepted, Rejection>>,
    next_request: u64,
    next_command: u64,
    last_seq: u64,
    /// Latest started inference of the current runtime.
    inference: Option<InferenceId>,
    started: Vec<InferenceId>,
    /// Started and not yet released, in the current runtime.
    listening: BTreeSet<InferenceId>,
    timers: BTreeSet<TimerId>,
    /// Torn tails repaired after a crash.
    repairs: u64,
    /// Simulated process crashes.
    crashes: u64,
    /// Identities for records tests write directly, as a tool runtime would.
    injector: DeterministicEnv,
    workspace: FakeWorkspace,
    planning: Option<(PlanToken, CallRef, String, ValidatedOperation)>,
    executing: Option<(ExecutionToken, AuthorizedOperation, CancelToken)>,
    executor: StubExecutor,
    compaction: Option<(CompactionToken, crate::context::compaction::CompactionInput)>,
}

/// The default configuration with a generous limit for both test models, so
/// a turn's budget is known.
pub(super) fn known() -> RuntimeConfig {
    let mut config = RuntimeConfig::default();
    for model in ["scripted", "other"] {
        config.context.catalog.insert(
            ModelRef {
                provider_instance: "mock".into(),
                model: model.into(),
            },
            ModelLimits {
                context_tokens: Some(1_000_000),
                max_output_tokens: None,
            },
        );
    }
    config
}

impl Driver {
    fn new() -> Self {
        Self::with_config(known())
    }

    fn with_config(config: RuntimeConfig) -> Self {
        Self::with_configuration(config, br#"{"policy":"runtime-test"}"#)
    }

    /// A new session whose configuration artifact holds `configuration`.
    fn with_configuration(config: RuntimeConfig, configuration: &[u8]) -> Self {
        Self::with_tools(config, configuration, ToolSet::default())
    }

    fn with_tools(config: RuntimeConfig, configuration: &[u8], tools: ToolSet) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
        let configuration = root.put_artifact(configuration).unwrap();
        let decoded = SessionConfiguration::load(&root, &configuration).unwrap();
        let header = SessionHeader {
            workspace: "/workspace".into(),
            initial_model: ModelRef {
                provider_instance: "mock".into(),
                model: "scripted".into(),
            },
            policy_revision: "initial".into(),
            configuration,
        };
        let at: Timestamp = DeterministicEnv::BASE.parse().unwrap();
        let store = JournalStore::create(root.clone(), SessionId::new(), header, at).unwrap();
        let snapshot = store.snapshot();
        let mut env = DeterministicEnv::new(FIRST_STREAM);
        Self {
            session: snapshot.session_id().clone(),
            runtime: Runtime::with_tools(
                snapshot.clone(),
                decoded.clone(),
                config.clone(),
                tools.clone(),
                &mut env,
            ),
            env,
            segment: Segment::new(FIRST_STREAM, decoded, config, snapshot, tools),
            done: Vec::new(),
            _temp: temp,
            root,
            store: Some(store),
            pending: None,
            verification: None,
            fail: None,
            failed: None,
            requests: BTreeSet::new(),
            replies: BTreeMap::new(),
            next_request: 0,
            next_command: 0,
            last_seq: 0,
            inference: None,
            started: Vec::new(),
            listening: BTreeSet::new(),
            timers: BTreeSet::new(),
            repairs: 0,
            crashes: 0,
            injector: DeterministicEnv::new(INJECTOR_STREAM),
            workspace: fake_workspace(),
            planning: None,
            executing: None,
            executor: StubExecutor::default(),
            compaction: None,
        }
    }

    /// A runtime over an existing journal in `root`, opened and started as a
    /// process would open it.
    fn open_existing(
        temp: tempfile::TempDir,
        root: Arc<DataRoot>,
        session: SessionId,
        config: RuntimeConfig,
    ) -> Self {
        let tools = ToolSet::default();
        let store = JournalStore::open(root.clone(), &session).unwrap();
        let snapshot = store.snapshot();
        let decoded = SessionConfiguration::load(&root, snapshot.configuration()).unwrap();
        let mut env = DeterministicEnv::new(FIRST_STREAM);
        let mut driver = Self {
            session,
            runtime: Runtime::with_tools(
                snapshot.clone(),
                decoded.clone(),
                config.clone(),
                tools.clone(),
                &mut env,
            ),
            env,
            segment: Segment::new(FIRST_STREAM, decoded, config, snapshot, tools),
            done: Vec::new(),
            _temp: temp,
            root,
            store: Some(store),
            pending: None,
            verification: None,
            fail: None,
            failed: None,
            requests: BTreeSet::new(),
            replies: BTreeMap::new(),
            next_request: 0,
            next_command: 0,
            last_seq: 0,
            inference: None,
            started: Vec::new(),
            listening: BTreeSet::new(),
            timers: BTreeSet::new(),
            repairs: 0,
            crashes: 0,
            injector: DeterministicEnv::new(INJECTOR_STREAM),
            workspace: fake_workspace(),
            planning: None,
            executing: None,
            executor: StubExecutor::default(),
            compaction: None,
        };
        driver.step(Input::Start);
        driver
    }

    /// Feed one input, record it, and check the per-step invariants.
    fn step(&mut self, input: Input) -> Vec<Effect> {
        let before = self.runtime.phase();
        let at = self.env.now();
        let effects = self.runtime.step(input.clone(), &mut self.env);
        if matches!(before, Phase::RecoveryRequired | Phase::AwaitingResolution) {
            invents_nothing(&effects);
        }
        for effect in &effects {
            self.observe(effect);
        }
        let view = self.runtime.view();
        assert_eq!(view.event_seq, self.last_seq, "view lags the event stream");
        if view.phase == Phase::Faulted || view.snapshot.active_turn().is_none() {
            assert!(self.listening.is_empty(), "stream not released");
        }
        self.segment.inputs.push(input);
        self.segment.times.push(at);
        self.segment.effects.extend(effects.iter().cloned());
        self.segment.views.push(view);
        effects
    }

    fn observe(&mut self, effect: &Effect) {
        match effect {
            Effect::InspectUncertain { .. } => {}
            Effect::ProposeCompaction { token, input } => {
                assert!(self.compaction.is_none(), "overlapping compaction");
                assert_eq!(self.runtime.snapshot().state(), RuntimeState::Idle);
                assert!(self.runtime.snapshot().active_turn().is_none());
                self.compaction = Some((*token, input.clone()));
            }
            Effect::CancelCompaction { token } => {
                if self
                    .compaction
                    .as_ref()
                    .is_some_and(|(active, _)| active == token)
                {
                    self.compaction = None;
                }
            }
            Effect::VerifyArtifacts { token, refs } => {
                assert!(self.pending.is_none() && self.verification.is_none());
                self.verification = Some((*token, refs.clone()));
            }
            Effect::Persist {
                token,
                commit,
                command,
                artifacts,
                at,
            } => {
                assert!(
                    self.pending.is_none(),
                    "a second Persist while one is in flight"
                );
                self.pending = Some(Pending {
                    token: *token,
                    commit: commit.clone(),
                    command: command.clone(),
                    at: at.clone(),
                    artifacts: artifacts.clone(),
                });
            }
            Effect::StartInference { inference, .. } => {
                self.inference = Some(*inference);
                self.started.push(*inference);
                assert!(self.listening.insert(*inference), "inference reused");
            }
            Effect::PlanCall {
                token,
                call,
                tool,
                operation,
            } => {
                assert!(self.planning.is_none() && self.executing.is_none());
                self.planning = Some((*token, call.clone(), tool.clone(), operation.clone()));
            }
            Effect::Execute { token, operation } => {
                assert!(
                    self.pending.is_none(),
                    "Execute before intent acknowledgement"
                );
                assert!(self.executing.is_none() && self.planning.is_none());
                let intent = self
                    .runtime
                    .snapshot()
                    .events()
                    .find_map(|(_, _, e)| match e {
                        JournalEvent::ExecutionIntent(i)
                            if &i.invocation_id == operation.invocation_id() =>
                        {
                            Some(i)
                        }
                        _ => None,
                    })
                    .expect("Execute without durable intent");
                assert_eq!(intent.effects.as_ref(), Some(operation.plan().effects()));
                assert_eq!(intent.plan_digest, *operation.plan().digest());
                assert_eq!(
                    self.root
                        .read_artifact(
                            &ArtifactRef {
                                sha256: intent.plan_digest.clone(),
                                bytes: operation.plan().bytes().len() as u64
                            },
                            8 * 1024 * 1024
                        )
                        .unwrap(),
                    operation.plan().bytes()
                );
                self.executing = Some((*token, operation.clone(), CancelToken::new()));
            }
            Effect::CancelExecution { token } => {
                let (active, _, cancel) = self.executing.as_ref().expect("cancel without Execute");
                assert_eq!(active, token);
                cancel.cancel();
            }
            Effect::CancelInference { inference } => {
                assert!(self.listening.contains(inference), "cancel after release");
            }
            Effect::ReleaseInference { inference } => {
                assert!(self.listening.remove(inference), "second release");
            }
            Effect::SetTimer { timer, .. } => assert!(self.timers.insert(*timer)),
            Effect::CancelTimer { timer } => {
                self.timers.remove(timer);
            }
            Effect::Emit(event) => {
                assert_eq!(event.seq, self.last_seq + 1, "event seq gap");
                self.last_seq = event.seq;
            }
            Effect::Reply { request, result } => {
                assert!(
                    self.requests.contains(request),
                    "reply to unknown {request}"
                );
                assert!(
                    self.replies.insert(*request, result.clone()).is_none(),
                    "second reply to {request}"
                );
            }
        }
    }

    fn command(&mut self, command: Command) -> RequestId {
        self.next_request += 1;
        let request = RequestId(self.next_request);
        self.requests.insert(request);
        self.step(Input::Command { request, command });
        request
    }

    fn new_command_id(&mut self) -> CommandId {
        self.next_command += 1;
        command_id(self.next_command)
    }

    fn start(&mut self, prompt: &str) -> (RequestId, CommandId) {
        let id = self.new_command_id();
        let request = self.command(Command::StartTurn {
            command_id: id.clone(),
            content: text(prompt),
        });
        (request, id)
    }

    fn cancel(&mut self) -> RequestId {
        let turn_id = self.active_turn().expect("an active turn");
        self.command(Command::Cancel { turn_id })
    }

    fn reply(&self, request: RequestId) -> Option<&Result<Accepted, Rejection>> {
        self.replies.get(&request)
    }

    fn accepted(&self, request: RequestId) -> &Accepted {
        match self.reply(request) {
            Some(Ok(accepted)) => accepted,
            other => panic!("{request} was not accepted: {other:?}"),
        }
    }

    fn rejected(&self, request: RequestId) -> Rejection {
        match self.reply(request) {
            Some(Err(rejection)) => rejection.clone(),
            other => panic!("{request} was not rejected: {other:?}"),
        }
    }

    fn active_turn(&self) -> Option<TurnId> {
        self.runtime.snapshot().active_turn().cloned()
    }

    /// Execute the pending commit like the storage worker, and answer it.
    fn complete(&mut self) -> bool {
        if let Some((token, refs)) = self.verification.take() {
            let missing = refs
                .into_iter()
                .filter(|reference| self.root.verify_artifact(reference).is_err())
                .collect();
            self.step(Input::ArtifactsVerified { token, missing });
            return true;
        }
        let Some(pending) = self.pending.take() else {
            return false;
        };
        let store = self.store.as_mut().expect("store is open");
        self.root.io.test.lock().unwrap().fail = self.fail.take();
        let mut commit = pending.commit.clone();
        let result = (|| {
            for bytes in &pending.artifacts {
                self.root.put_artifact(bytes)?;
            }
            if let Some((id, command)) = &pending.command {
                store.bind_command(&mut commit, id.clone(), command)?;
            }
            store.append(commit.clone(), pending.at.clone())
        })();
        self.root.io.test.lock().unwrap().fail = None;
        let input = match result {
            Ok(ack) => Input::Persisted {
                token: pending.token,
                ack,
                snapshot: store.snapshot(),
            },
            Err(error) => {
                let error = PersistError::from_store(&error);
                if let PersistError::Rejected { code } = &error {
                    panic!(
                        "storage rejected a commit the core believed legal: {code}\n{commit:#?}"
                    );
                }
                self.failed = Some(commit);
                Input::PersistFailed {
                    token: pending.token,
                    error,
                }
            }
        };
        self.step(input);
        true
    }

    /// Answer the pending commit with a failure without touching storage.
    fn fail_pending(&mut self, error: PersistError) {
        let pending = self.pending.take().expect("a commit in flight");
        self.step(Input::PersistFailed {
            token: pending.token,
            error,
        });
    }

    /// Answer commits until none is in flight.
    fn settle(&mut self) {
        while self.complete() {}
    }

    fn provider(&mut self, event: ProviderEvent) -> Vec<Effect> {
        let inference = self.inference.expect("an inference was started");
        self.step(Input::Provider { inference, event })
    }

    fn ended(&mut self) -> Vec<Effect> {
        let inference = self.inference.expect("an inference was started");
        self.step(Input::ProviderEnded { inference })
    }

    fn fire(&mut self, timer: TimerId) -> Vec<Effect> {
        self.timers.remove(&timer);
        self.step(Input::TimerFired { timer })
    }

    /// Drop the runtime and store, reopen the journal and start a new runtime
    /// with the next identity stream, as a restarted process would.
    fn reopen(&mut self) {
        self.reopen_unstarted();
        self.step(Input::Start);
    }

    /// `reopen`, without feeding `Start` to the new runtime.
    fn reopen_unstarted(&mut self) {
        assert!(self.pending.is_none(), "reopen with a commit in flight");
        self.check_reopened();
        self.attach();
    }

    /// Die as a killed process would: no shutdown and no answer for the
    /// commit in flight, which `crash` decides the fate of. Requests the dead
    /// process held are never answered.
    fn kill(&mut self, crash: Crash) {
        self.crashes += 1;
        if let Some(pending) = self.pending.take()
            && crash != Crash::Lost
        {
            let store = self.store.as_mut().expect("store is open");
            if crash == Crash::Torn {
                self.root.io.test.lock().unwrap().fail = Some(Point::PartialWrite);
            }
            let mut commit = pending.commit;
            let result = (|| {
                for bytes in &pending.artifacts {
                    self.root.put_artifact(bytes)?;
                }
                if let Some((id, command)) = &pending.command {
                    store.bind_command(&mut commit, id.clone(), command)?;
                }
                store.append(commit.clone(), pending.at)
            })();
            self.root.io.test.lock().unwrap().fail = None;
            match (crash, result) {
                (Crash::Durable, Ok(_)) | (Crash::Torn, Err(StoreError::Io(_))) => {}
                (_, result) => panic!("crash write {crash:?} gave {result:?}\n{commit:#?}"),
            }
        }
        self.verification = None;
        self.compaction = None;
        self.requests
            .retain(|request| self.replies.contains_key(request));
        drop(self.store.take());
    }

    /// Kill the process and restart it.
    fn crash(&mut self, crash: Crash) {
        self.kill(crash);
        self.restart();
    }

    /// Start a new process over the journal: repair a torn tail as a user who
    /// inspected it would, open, and feed `Start`.
    fn restart(&mut self) {
        drop(self.store.take());
        self.repair_if_torn();
        self.failed = None;
        self.attach();
        self.step(Input::Start);
    }

    /// Repair a torn tail with the fingerprint and size just inspected.
    fn repair_if_torn(&mut self) {
        let inspection = JournalStore::inspect(&self.root, &self.session).unwrap();
        match &inspection.diagnostic {
            None => {}
            Some(diagnostic) if diagnostic.kind == DiagnosticKind::TornTail => {
                let at = DeterministicEnv::BASE.parse().unwrap();
                JournalStore::repair_tail(self.root.clone(), &self.session, &inspection, at)
                    .unwrap();
                self.repairs += 1;
            }
            Some(diagnostic) => panic!("journal is not repairable: {diagnostic:?}"),
        }
    }

    /// Append a commit through this process's writer as a tool runtime
    /// would, just before the process dies. The runtime never sees it. The
    /// commit in flight, if any, is dropped unwritten first.
    fn inject(
        &mut self,
        build: impl FnOnce(&SessionSnapshot, &mut DeterministicEnv, &DataRoot) -> Commit,
    ) {
        self.pending = None;
        let store = self.store.as_mut().expect("the dying process's writer");
        let commit = build(&store.snapshot(), &mut self.injector, &self.root);
        let at = DeterministicEnv::BASE.parse().unwrap();
        if let Err(error) = store.append(commit.clone(), at) {
            panic!("storage rejected an injected commit: {error}\n{commit:#?}");
        }
    }

    /// Open the journal under a new runtime with the next identity stream.
    fn attach(&mut self) {
        let stream = self.segment.stream + 1;
        let store = JournalStore::open(self.root.clone(), &self.session).unwrap();
        let snapshot = store.snapshot();
        self.store = Some(store);
        let config = self.segment.config.clone();
        let configuration =
            SessionConfiguration::load(&self.root, snapshot.configuration()).unwrap();
        self.env = DeterministicEnv::new(stream);
        let tools = self.segment.tools.clone();
        self.runtime = Runtime::with_tools(
            snapshot.clone(),
            configuration.clone(),
            config.clone(),
            tools.clone(),
            &mut self.env,
        );
        let segment = std::mem::replace(
            &mut self.segment,
            Segment::new(stream, configuration, config, snapshot, tools),
        );
        self.done.push(segment);
        self.last_seq = 0;
        self.inference = None;
        // A restarted process holds none of the old runtime's streams.
        self.listening.clear();
        self.timers.clear();
        self.planning = None;
        self.executing = None;
        self.compaction = None;
    }

    /// Reopen the journal and compare it with the core's published snapshot.
    /// Returns the reopened (or valid-prefix) snapshot.
    fn check_reopened(&mut self) -> SessionSnapshot {
        let published = self.runtime.snapshot().clone();
        drop(self.store.take());
        let reopened = match JournalStore::open(self.root.clone(), &self.session) {
            Ok(store) => store.snapshot(),
            Err(StoreError::Journal(diagnostic))
                if diagnostic.kind == DiagnosticKind::TornTail && self.failed.is_some() =>
            {
                JournalStore::inspect(&self.root, &self.session)
                    .unwrap()
                    .snapshot
                    .unwrap()
            }
            Err(error) => panic!("reopen failed: {error}"),
        };
        if !same(&reopened, &published) {
            // Only an I/O failure after a complete write leaves one more record:
            // the failed commit itself, durable although never acknowledged.
            let failed = self.failed.as_ref().expect("reopened snapshot differs");
            assert_eq!(reopened.sequence(), published.sequence() + 1);
            assert!(
                failed
                    .entries
                    .iter()
                    .all(|e| reopened.entry(&e.id).is_some())
            );
            assert_eq!(failed.head.to.as_ref(), reopened.head());
        }
        reopened
    }

    /// Resolve every uncertain invocation the runtime waits for as unknown.
    fn acknowledge_all(&mut self) {
        while self.runtime.phase() == Phase::AwaitingResolution {
            let invocation_id = self.runtime.view().awaiting[0].invocation_id.clone();
            let request = self.command(Command::ResolveUncertain {
                invocation_id,
                resolution: UserResolution::AcknowledgedUnknown,
            });
            self.settle();
            self.accepted(request);
        }
    }

    /// End of every test: all requests answered once; the journal reopens to
    /// the published snapshot (allowing for an I/O failure's durable commit).
    /// A run that leaves a turn open or a fault is restarted, so recovery
    /// closes it: every started turn ends with exactly one `TurnFinished`.
    fn finish(mut self) -> Finished {
        assert!(self.pending.is_none(), "test ended with a commit in flight");
        let missing: Vec<_> = self
            .requests
            .iter()
            .filter(|r| !self.replies.contains_key(r))
            .collect();
        assert!(missing.is_empty(), "requests without a reply: {missing:?}");
        let faulted = self.failed.is_some() || self.runtime.phase() == Phase::Faulted;
        if self.runtime.phase() != Phase::Durable(RuntimeState::Idle) {
            self.check_reopened();
            self.verification = None;
            self.restart();
            self.settle();
            self.acknowledge_all();
        }
        assert_eq!(self.runtime.phase(), Phase::Durable(RuntimeState::Idle));
        let reopened = self.check_reopened();
        self.done.push(self.segment);
        Finished {
            segments: self.done,
            reopened,
            faulted,
            repairs: self.repairs,
            crashes: self.crashes,
        }
    }
}

impl Segment {
    fn new(
        stream: u16,
        configuration: SessionConfiguration,
        config: RuntimeConfig,
        initial: SessionSnapshot,
        tools: ToolSet,
    ) -> Self {
        let views = vec![
            Runtime::with_tools(
                initial.clone(),
                configuration.clone(),
                config.clone(),
                tools.clone(),
                &mut DeterministicEnv::new(stream),
            )
            .view(),
        ];
        Self {
            stream,
            configuration,
            config,
            tools,
            initial,
            inputs: Vec::new(),
            times: Vec::new(),
            effects: Vec::new(),
            views,
        }
    }

    fn events(&self) -> impl Iterator<Item = &RuntimeEvent> {
        self.effects.iter().filter_map(|effect| match effect {
            Effect::Emit(event) => Some(event),
            _ => None,
        })
    }
}

struct Finished {
    segments: Vec<Segment>,
    reopened: SessionSnapshot,
    faulted: bool,
    repairs: u64,
    crashes: u64,
}

/// Identity stream of records written directly by tests, as a tool runtime.
const INJECTOR_STREAM: u16 = 0xffc;

/// While recovering, the core never opens a stream, writes an assistant
/// response or a result with executor provenance.
fn invents_nothing(effects: &[Effect]) {
    for effect in effects {
        match effect {
            Effect::StartInference { .. } => panic!("a stream opened during recovery"),
            Effect::PlanCall { .. } | Effect::Execute { .. } | Effect::CancelExecution { .. } => {
                panic!("a tool effect during recovery")
            }
            Effect::Persist { commit, .. } => {
                for entry in &commit.entries {
                    match &entry.payload {
                        EntryPayload::AssistantResponse(_) => {
                            panic!("recovery wrote an assistant response")
                        }
                        EntryPayload::ToolResult(r) => assert_ne!(
                            r.result.metadata.provenance,
                            Provenance::Executor,
                            "recovery wrote an executor result"
                        ),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

/// Snapshot identity: the last record's exact-byte checksum covers the chain.
fn same(a: &SessionSnapshot, b: &SessionSnapshot) -> bool {
    a.session_id() == b.session_id()
        && a.sequence() == b.sequence()
        && a.checksum() == b.checksum()
        && a.head() == b.head()
        && a.state() == b.state()
        && a.active_turn() == b.active_turn()
        && a.entries().len() == b.entries().len()
        && a.events().len() == b.events().len()
}

/// Every started turn has exactly one `TurnFinished`, including after faults
/// and crashes, because `finish` lets recovery close what a run left open.
/// Only recovery interrupts a turn, and the journal holds nothing invented:
/// every assistant response came from a stream the core started, no result
/// claims an executor, and each uncertainty is recorded once. Each response
/// records the fingerprint its stream was sent, and each result the code
/// its text leads with.
fn check_turns(finished: &Finished) {
    let snapshot = &finished.reopened;
    assert!(!snapshot.requires_recovery());
    let mut dispatched = BTreeSet::new();
    for effect in finished.segments.iter().flat_map(|s| &s.effects) {
        if let Effect::Execute { operation, .. } = effect {
            assert!(
                dispatched.insert(operation.invocation_id().clone()),
                "invocation dispatched twice across restart"
            );
        }
    }
    let mut outcomes: BTreeMap<TurnId, usize> = BTreeMap::new();
    let mut uncertain = BTreeSet::new();
    for (_, _, event) in snapshot.events() {
        match event {
            JournalEvent::TurnStarted(start) => {
                assert!(outcomes.insert(start.turn_id.clone(), 0).is_none());
            }
            JournalEvent::TurnFinished(end) => {
                if end.outcome == TurnOutcome::Interrupted {
                    assert_eq!(end.code.as_deref(), Some("process_restart"));
                }
                *outcomes.get_mut(&end.turn_id).unwrap() += 1;
            }
            JournalEvent::ExecutionUncertain(v) => {
                assert!(uncertain.insert(v.invocation_id.clone()), "recorded twice");
            }
            _ => {}
        }
    }
    for (turn, count) in &outcomes {
        assert_eq!(*count, 1, "turn {turn} has {count} TurnFinished");
    }
    let streamed: BTreeMap<_, _> = finished
        .segments
        .iter()
        .flat_map(|segment| &segment.effects)
        .filter_map(|effect| match effect {
            Effect::StartInference {
                response_id,
                fingerprint,
                ..
            } => Some((response_id.clone(), fingerprint.clone())),
            _ => None,
        })
        .collect();
    // A fixture's own history was not written by this runtime.
    let initial = &finished.segments[0].initial;
    for (id, stored) in snapshot.entries() {
        if initial.entry(id).is_some() {
            continue;
        }
        match &stored.entry.payload {
            EntryPayload::AssistantResponse(r) => {
                let sent = streamed.get(&r.response_id);
                assert!(sent.is_some(), "an assistant response no stream produced");
                assert_eq!(r.projection_fingerprint.as_ref(), sent, "{:?}", r.outcome);
                let mut native_bytes = 0;
                for block in &r.content {
                    if let ContentBlock::ProviderState {
                        owner, artifact, ..
                    } = block
                    {
                        assert_eq!(owner, &r.model);
                        assert!(artifact.bytes <= 256 * 1024);
                        native_bytes += artifact.bytes;
                    }
                }
                if let Some(continuation) = &r.continuation {
                    assert_eq!(continuation.owner, r.model);
                    assert_eq!(Some(&continuation.input_fingerprint), sent);
                    assert_eq!(r.outcome, ResponseOutcome::Completed);
                    assert!(continuation.artifact.bytes <= 256 * 1024);
                    native_bytes += continuation.artifact.bytes;
                }
                assert!(native_bytes <= 4 * 1024 * 1024);
            }
            EntryPayload::ToolResult(r) => {
                if r.result.metadata.provenance == Provenance::Executor {
                    let invocation = r
                        .result
                        .invocation_id
                        .as_ref()
                        .expect("executor result needs intent");
                    assert!(finished.segments.iter().flat_map(|s| &s.effects).any(|e| matches!(e, Effect::Execute { operation, .. } if operation.invocation_id() == invocation)), "executor result without dispatch");
                }
                check_code(&r.result);
            }
            _ => {}
        }
    }
    // The runtime announced exactly the journaled outcomes, once each.
    let announced: Vec<_> = finished
        .segments
        .iter()
        .flat_map(Segment::events)
        .filter_map(|event| match &event.kind {
            EventKind::TurnFinished { turn_id, .. } => Some(turn_id.clone()),
            _ => None,
        })
        .collect();
    let unique: BTreeSet<_> = announced.iter().collect();
    assert_eq!(unique.len(), announced.len(), "a turn finished twice");
    for turn in &announced {
        assert_eq!(outcomes.get(turn), Some(&1));
    }
}

/// A result the runtime wrote: a user's result has no code, and any other
/// result has the code its text leads with. Tests inject other texts.
fn check_code(result: &ToolResult) {
    let code = result.metadata.code.as_deref();
    let [ContentBlock::Text { text }] = &result.content[..] else {
        panic!("a runtime result is one text block")
    };
    let prefix = text.split_once(':').map(|(prefix, _)| prefix);
    match result.metadata.provenance {
        Provenance::UserObserved | Provenance::UserAcknowledgedUnknown => {
            assert_eq!(code, None, "{text}")
        }
        _ if result.status == ToolStatus::Succeeded => assert_eq!(code, None, "{text}"),
        _ => {
            let known = [
                ResultCode::ToolsUnavailable,
                ResultCode::Cancelled,
                ResultCode::ProcessRestart,
            ]
            .map(ResultCode::as_str);
            assert!(code.is_some(), "{text}");
            if prefix.is_some_and(|p| known.contains(&p)) {
                assert_eq!(code, prefix, "{text}");
            }
        }
    }
}

/// For every recorded view, the view plus all later events equals the final
/// view; earlier events are ignored as duplicates and no gap appears.
fn check_resync(finished: &Finished) {
    for segment in &finished.segments {
        let events: Vec<_> = segment.events().collect();
        let last = ClientView::from(segment.views.last().unwrap().clone());
        for view in &segment.views {
            let mut client = ClientView::from(view.clone());
            for event in &events {
                assert_ne!(client.apply(event), ApplyOutcome::NeedsResync);
            }
            assert_eq!(client, last, "resync from seq {}", view.event_seq);
        }
    }
}

/// Replay each segment's recorded inputs into a fresh runtime and environment;
/// the effects must be byte-identical.
fn check_replay(finished: &Finished) {
    for segment in &finished.segments {
        let mut env = DeterministicEnv::new(segment.stream);
        let mut runtime = Runtime::with_tools(
            segment.initial.clone(),
            segment.configuration.clone(),
            segment.config.clone(),
            segment.tools.clone(),
            &mut env,
        );
        let replayed: Vec<_> = segment
            .inputs
            .iter()
            .zip(&segment.times)
            .flat_map(|(input, at)| {
                let elapsed = (at.instant() - env.now().instant())
                    .try_into()
                    .expect("monotonic fake clock");
                env.advance(elapsed);
                runtime.step(input.clone(), &mut env)
            })
            .collect();
        assert_eq!(format!("{replayed:?}"), format!("{:?}", segment.effects));
    }
}

/// Effects with storage-chosen bytes (record IDs and checksums) abstracted, so
/// two runs against separate journals can be compared.
fn normalized(finished: &Finished) -> Vec<String> {
    let ack = |ack: &CommitAck| format!("ack(seq={}, head={:?})", ack.sequence, ack.head);
    finished
        .segments
        .iter()
        .flat_map(|segment| &segment.effects)
        .map(|effect| match effect {
            Effect::Emit(RuntimeEvent {
                seq,
                epoch: _,
                kind: EventKind::Committed { ack: a, .. },
            }) => format!("Emit({seq}, Committed({}))", ack(a)),
            Effect::Reply {
                request,
                result: Ok(accepted),
            } => {
                let accepted = match accepted {
                    Accepted::CompactionPreviewed { .. } | Accepted::CompactionCancelled => {
                        format!("{accepted:?}")
                    }
                    Accepted::CompactionCommitted { preview_id, ack: a } => {
                        format!("CompactionCommitted({preview_id}, {})", ack(a))
                    }
                    Accepted::TurnStarted { turn_id, ack: a } => {
                        format!("TurnStarted({turn_id}, {})", ack(a))
                    }
                    Accepted::Cancelling { turn_id, ack: a } => {
                        format!("Cancelling({turn_id}, {})", ack(a))
                    }
                    Accepted::Resolved {
                        invocation_id,
                        ack: a,
                    } => format!("Resolved({invocation_id}, {})", ack(a)),
                    Accepted::HeadSelected { ack: a } => format!("HeadSelected({})", ack(a)),
                    Accepted::ModelSwitched { ack: a, .. } => format!("ModelSwitched({})", ack(a)),
                    Accepted::ContextControlSet {
                        ack: a, turn_id, ..
                    } => format!("ContextControlSet({turn_id}, {})", ack(a)),
                    Accepted::CompactionStrategySet { ack: a, strategy } => {
                        format!("CompactionStrategySet({strategy}, {})", ack(a))
                    }
                    Accepted::ApprovalSubmitted {
                        approval_id,
                        ack: a,
                    } => format!("ApprovalSubmitted({approval_id}, {})", ack(a)),
                };
                format!("Reply({request}, {accepted})")
            }
            other => format!("{other:?}"),
        })
        .collect()
}

/// Run a scenario twice with fresh environments and journals, replay it, and
/// check resync, reopening and turn outcomes.
fn scenario(config: RuntimeConfig, body: fn(&mut Driver)) {
    scenario_from(|| Driver::with_config(config.clone()), body);
}

/// `scenario` for a driver built by `new`, such as one over a fixture.
fn scenario_from(new: impl Fn() -> Driver, body: impl Fn(&mut Driver)) {
    let run = || {
        let mut driver = new();
        body(&mut driver);
        driver.finish()
    };
    let first = run();
    let second = run();
    assert_eq!(normalized(&first), normalized(&second), "runs differ");
    check_replay(&first);
    check_resync(&first);
    check_turns(&first);
}

/// One line per emitted commit: entry kinds, then journal events.
fn shapes(driver: &Driver) -> Vec<String> {
    driver
        .done
        .iter()
        .chain([&driver.segment])
        .flat_map(|segment| &segment.effects)
        .filter_map(|effect| match effect {
            Effect::Persist { commit, .. } => Some(shape(commit)),
            _ => None,
        })
        .collect()
}

fn shape(commit: &Commit) -> String {
    let state = |s: RuntimeState| format!("{s:?}").to_lowercase();
    let mut parts: Vec<String> = commit
        .entries
        .iter()
        .map(|entry| match &entry.payload {
            EntryPayload::UserMessage(_) => "user".into(),
            EntryPayload::AssistantResponse(r) => format!(
                "assistant({})",
                match r.finish_reason {
                    Some(reason) => format!("{:?}/{reason:?}", r.outcome),
                    None => format!("{:?}", r.outcome),
                }
                .to_lowercase()
            ),
            EntryPayload::ToolResult(r) => format!("result({:?})", r.result.status).to_lowercase(),
            EntryPayload::ModelChanged(_) => "model_changed".into(),
            EntryPayload::Compaction(_) => "compaction".into(),
            EntryPayload::ContextControl(_) => "context_control".into(),
        })
        .collect();
    if commit.head.reason == HeadReason::Navigate {
        parts.push("navigate".into());
    }
    for event in &commit.events {
        parts.push(match event {
            JournalEvent::TurnStarted(_) => "turn_started".into(),
            JournalEvent::StateChanged(c) => format!("{}>{}", state(c.from), state(c.to)),
            JournalEvent::TurnFinished(f) => match &f.code {
                Some(code) => format!("finished({:?}, {code})", f.outcome).to_lowercase(),
                None => format!("finished({:?})", f.outcome).to_lowercase(),
            },
            JournalEvent::ExecutionUncertain(_) => "execution_uncertain".into(),
            JournalEvent::RecoveryResolved(r) => {
                format!("recovery_resolved({:?})", r.resolution).to_lowercase()
            }
            JournalEvent::AuthorizationDecided(_) => "authorization_decided".into(),
            JournalEvent::ExecutionIntent(_) => "execution_intent".into(),
            other => format!("{other:?}"),
        });
    }
    parts.join(" ")
}

#[test]
fn deterministic_env_issues_unique_valid_uuidv7_across_streams() {
    let mut seen = BTreeSet::new();
    for stream in [1, 2] {
        let mut env = DeterministicEnv::new(stream);
        for _ in 0..100 {
            assert!(seen.insert(env.entry_id().to_string()));
            assert!(seen.insert(env.turn_id().to_string()));
            assert!(seen.insert(env.response_id().to_string()));
            assert!(seen.insert(env.call_id().to_string()));
        }
        assert_eq!(env.now(), DeterministicEnv::BASE.parse().unwrap());
    }
    assert!(!seen.contains(&command_id(1).to_string()));
}

#[test]
fn client_view_reports_a_skipped_seq_and_ignores_old_events() {
    let mut driver = Driver::new();
    let initial = driver.runtime.view();
    driver.start("hello");
    driver.settle();
    let events: Vec<_> = driver.segment.events().cloned().collect();
    assert!(events.len() >= 3);

    let mut client = ClientView::from(initial.clone());
    assert_eq!(client.apply(&events[1]), ApplyOutcome::NeedsResync);
    assert_eq!(client, ClientView::from(initial), "a gap changes nothing");
    assert_eq!(client.apply(&events[0]), ApplyOutcome::Applied);
    assert_eq!(client.apply(&events[0]), ApplyOutcome::Duplicate);
    assert_eq!(client.apply(&events[2]), ApplyOutcome::NeedsResync);
    for event in &events[1..] {
        assert_eq!(client.apply(event), ApplyOutcome::Applied);
    }
    assert_eq!(client, ClientView::from(driver.runtime.view()));
    driver.cancel();
    driver.settle();
    driver.fire(*driver.timers.iter().next().unwrap());
    driver.settle();
    driver.finish();
}

#[test]
fn replacement_epoch_requires_resync_before_comparing_sequence() {
    let mut driver = Driver::new();
    driver.start("hello");
    driver.settle();
    driver.provider(ProviderEvent::Started);
    for _ in 0..100 {
        driver.provider(ProviderEvent::TextDelta {
            block_index: 0,
            text: "x".into(),
        });
    }
    let mut client = ClientView::from(driver.runtime.view());
    assert!(client.view().event_seq > 100);
    driver.ended();
    driver.fail_pending(PersistError::Io {
        message: "failed".into(),
    });
    driver.reopen_unstarted();
    assert_ne!(client.view().epoch, driver.runtime.view().epoch);
    let events = driver.step(Input::PublishView);
    let Effect::Emit(event) = &events[0] else {
        panic!("phase event missing")
    };
    assert_eq!(event.seq, 1);
    let previous = client.clone();
    assert_eq!(client.apply(event), ApplyOutcome::NeedsResync);
    assert_eq!(client, previous);
    driver.finish();
}

#[test]
fn media_verification_attaches_retries_and_rejects_missing_without_writes() {
    let mut driver = Driver::new();
    let before = driver.runtime.snapshot().sequence();
    let content = vec![ContentBlock::Image {
        artifact: ArtifactRef {
            sha256: Digest::of(b"unknown"),
            bytes: 7,
        },
        mime_type: "image/png".into(),
    }];
    let command = Command::StartTurn {
        command_id: command_id(1),
        content,
    };
    let first = driver.command(command.clone());
    let retry = driver.command(command);
    let busy = driver.command(Command::StartTurn {
        command_id: command_id(2),
        content: text("other"),
    });
    assert_eq!(driver.rejected(busy), Rejection::Busy);
    assert!(driver.pending.is_none());
    driver.settle();
    assert_eq!(
        driver.rejected(first),
        Rejection::Invalid("artifact_missing")
    );
    assert_eq!(
        driver.rejected(retry),
        Rejection::Invalid("artifact_missing")
    );
    assert_eq!(driver.runtime.snapshot().sequence(), before);
    let artifact = driver.root.put_artifact(b"image").unwrap();
    let started = driver.command(Command::StartTurn {
        command_id: command_id(3),
        content: vec![ContentBlock::Image {
            artifact,
            mime_type: "image/png".into(),
        }],
    });
    driver.settle();
    driver.accepted(started);
    driver.ended();
    driver.settle();
    let finished = driver.finish();
    check_replay(&finished);
    check_resync(&finished);
}

#[test]
fn media_metadata_is_checked_before_preflight_and_verification_failure_faults_waiters() {
    let mut driver = Driver::new();
    let reference = driver.root.put_artifact(b"image").unwrap();
    for block in [
        ContentBlock::Image {
            artifact: reference.clone(),
            mime_type: String::new(),
        },
        ContentBlock::Audio {
            artifact: reference.clone(),
            mime_type: String::new(),
        },
        ContentBlock::File {
            artifact: reference.clone(),
            mime_type: "text/plain".into(),
            name: String::new(),
        },
    ] {
        let command_id = driver.new_command_id();
        let request = driver.command(Command::StartTurn {
            command_id,
            content: vec![block],
        });
        assert_eq!(
            driver.rejected(request),
            Rejection::Invalid("invalid_media")
        );
        assert!(driver.verification.is_none() && driver.pending.is_none());
    }
    let command_id = driver.new_command_id();
    let command = Command::StartTurn {
        command_id,
        content: vec![ContentBlock::Image {
            artifact: reference,
            mime_type: "image/png".into(),
        }],
    };
    let first = driver.command(command.clone());
    let retry = driver.command(command);
    let (token, _) = driver.verification.take().unwrap();
    driver.step(Input::PersistFailed {
        token,
        error: PersistError::WorkerLost,
    });
    assert_eq!(driver.rejected(first), Rejection::Faulted);
    assert_eq!(driver.rejected(retry), Rejection::Faulted);
    assert_eq!(driver.runtime.snapshot().sequence(), 0);
    let finished = driver.finish();
    check_replay(&finished);
}
