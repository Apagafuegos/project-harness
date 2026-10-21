//! Runtime tests against real storage. Every commit the core emits is appended
//! to a `JournalStore` with the test-build oracle on; any rejection fails.

mod random;
mod scenarios;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use crate::context::{ModelLimits, SessionConfiguration};
use crate::runtime::*;
use crate::session::{io::Point, *};

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
    initial: SessionSnapshot,
    inputs: Vec<Input>,
    effects: Vec<Effect>,
    /// The view before the first input and after each one.
    views: Vec<RuntimeView>,
}

/// The commit handed to the storage worker and not yet answered.
struct Pending {
    token: CommitToken,
    commit: Commit,
    command: Option<(CommandId, serde_json::Value)>,
    at: Timestamp,
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
            runtime: Runtime::new(snapshot.clone(), decoded.clone(), config.clone(), &mut env),
            env,
            segment: Segment::new(FIRST_STREAM, decoded, config, snapshot),
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
        }
    }

    /// Feed one input, record it, and check the per-step invariants.
    fn step(&mut self, input: Input) -> Vec<Effect> {
        let effects = self.runtime.step(input.clone(), &mut self.env);
        for effect in &effects {
            self.observe(effect);
        }
        let view = self.runtime.view();
        assert_eq!(view.event_seq, self.last_seq, "view lags the event stream");
        if view.phase == Phase::Faulted || view.snapshot.active_turn().is_none() {
            assert!(self.listening.is_empty(), "stream not released");
        }
        self.segment.inputs.push(input);
        self.segment.effects.extend(effects.iter().cloned());
        self.segment.views.push(view);
        effects
    }

    fn observe(&mut self, effect: &Effect) {
        match effect {
            Effect::VerifyArtifacts { token, refs } => {
                assert!(self.pending.is_none() && self.verification.is_none());
                self.verification = Some((*token, refs.clone()));
            }
            Effect::Persist {
                token,
                commit,
                command,
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
                });
            }
            Effect::StartInference { inference, .. } => {
                self.inference = Some(*inference);
                self.started.push(*inference);
                assert!(self.listening.insert(*inference), "inference reused");
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
            Some(Err(rejection)) => *rejection,
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
        assert!(self.pending.is_none(), "reopen with a commit in flight");
        self.check_reopened();
        let stream = self.segment.stream + 1;
        let store = JournalStore::open(self.root.clone(), &self.session).unwrap();
        let snapshot = store.snapshot();
        self.store = Some(store);
        let config = self.segment.config.clone();
        let configuration =
            SessionConfiguration::load(&self.root, snapshot.configuration()).unwrap();
        self.env = DeterministicEnv::new(stream);
        self.runtime = Runtime::new(
            snapshot.clone(),
            configuration.clone(),
            config.clone(),
            &mut self.env,
        );
        let segment = std::mem::replace(
            &mut self.segment,
            Segment::new(stream, configuration, config, snapshot),
        );
        self.done.push(segment);
        self.last_seq = 0;
        self.inference = None;
        // A restarted process holds none of the old runtime's streams.
        self.listening.clear();
        self.timers.clear();
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

    /// End of every test: all requests answered once; the journal reopens to
    /// the published snapshot (allowing for an I/O failure's durable commit).
    fn finish(mut self) -> Finished {
        assert!(self.pending.is_none(), "test ended with a commit in flight");
        let missing: Vec<_> = self
            .requests
            .iter()
            .filter(|r| !self.replies.contains_key(r))
            .collect();
        assert!(missing.is_empty(), "requests without a reply: {missing:?}");
        let reopened = self.check_reopened();
        self.done.push(self.segment);
        Finished {
            segments: self.done,
            reopened,
            faulted: self.failed.is_some(),
        }
    }
}

impl Segment {
    fn new(
        stream: u16,
        configuration: SessionConfiguration,
        config: RuntimeConfig,
        initial: SessionSnapshot,
    ) -> Self {
        let views = vec![
            Runtime::new(
                initial.clone(),
                configuration.clone(),
                config.clone(),
                &mut DeterministicEnv::new(stream),
            )
            .view(),
        ];
        Self {
            stream,
            configuration,
            config,
            initial,
            inputs: Vec::new(),
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

/// Each started turn has at most one `TurnFinished`, and exactly one unless
/// a fault stopped the runtime during it.
fn check_turns(finished: &Finished) {
    let snapshot = &finished.reopened;
    let mut outcomes: BTreeMap<TurnId, usize> = BTreeMap::new();
    for (_, _, event) in snapshot.events() {
        match event {
            JournalEvent::TurnStarted(start) => {
                assert!(outcomes.insert(start.turn_id.clone(), 0).is_none());
            }
            JournalEvent::TurnFinished(end) => {
                assert_ne!(
                    end.outcome,
                    TurnOutcome::Interrupted,
                    "only recovery interrupts a turn"
                );
                *outcomes.get_mut(&end.turn_id).unwrap() += 1;
            }
            _ => {}
        }
    }
    for (turn, count) in &outcomes {
        let unfinished_allowed = (finished.faulted || finished.segments.len() > 1)
            && snapshot.active_turn() == Some(turn);
        assert!(
            *count == 1 || (*count == 0 && unfinished_allowed),
            "turn {turn} has {count} TurnFinished"
        );
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
        let mut runtime = Runtime::new(
            segment.initial.clone(),
            segment.configuration.clone(),
            segment.config.clone(),
            &mut env,
        );
        let replayed: Vec<_> = segment
            .inputs
            .iter()
            .flat_map(|input| runtime.step(input.clone(), &mut env))
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
                    Accepted::TurnStarted { turn_id, ack: a } => {
                        format!("TurnStarted({turn_id}, {})", ack(a))
                    }
                    Accepted::Cancelling { turn_id, ack: a } => {
                        format!("Cancelling({turn_id}, {})", ack(a))
                    }
                    Accepted::HeadSelected { ack: a } => format!("HeadSelected({})", ack(a)),
                    Accepted::ModelSwitched { ack: a } => format!("ModelSwitched({})", ack(a)),
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
    let run = || {
        let mut driver = Driver::with_config(config.clone());
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
    driver.reopen();
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
