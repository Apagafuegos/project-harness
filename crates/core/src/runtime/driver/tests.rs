//! Real worker/channels on a current-thread runtime, with explicit virtual time.

#[path = "compaction_tests.rs"]
mod compaction_tests;
#[path = "host_tests.rs"]
mod host_tests;
#[path = "model_tests.rs"]
mod model_tests;
#[path = "tool_tests.rs"]
mod tool_tests;

use std::{
    collections::VecDeque,
    sync::atomic::Ordering,
    task::{Context, Poll, Waker},
    time::Duration,
};

use futures_core::Stream;

use super::*;
use crate::context::{
    ContextError, ContextItem, ExclusionReason, Flags, ModelLimits, SessionConfiguration,
    fingerprint,
};
use crate::{
    model::scripted::{Script, ScriptedProvider, Step},
    session::{
        io::{Gate, Point},
        *,
    },
};

/// One opened stream, handed to the test, which plays the provider.
type Opened = (ModelRequest, mpsc::Sender<ProviderEvent>, CancellationToken);

/// Test-only adapter: each stream is a channel the test writes into, so tests
/// can pace events against the views they observe.
struct Channel {
    opened: mpsc::UnboundedSender<Opened>,
}

impl ModelProvider for Channel {
    fn stream(&self, request: ModelRequest, control: OperationControl) -> ModelEventStream {
        let (send, receive) = mpsc::channel(256);
        let _ = self.opened.send((request, send, control.cancel));
        Box::pin(Received(receive))
    }
}

struct Received(mpsc::Receiver<ProviderEvent>);

impl Stream for Received {
    type Item = ProviderEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<ProviderEvent>> {
        self.0.poll_recv(cx)
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: Arc<DataRoot>,
    session: SessionId,
    handle: SessionHandle,
    streams: mpsc::UnboundedReceiver<Opened>,
    awake: JoinHandle<()>,
    options: WorkerOptions,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.awake.abort();
    }
}

impl Fixture {
    async fn new(config: DriverConfig) -> Self {
        Self::configured(config, WorkerOptions::default(), None).await
    }

    /// A fixture whose inferences replay `scripts` through the public mock.
    async fn scripted(
        config: DriverConfig,
        options: WorkerOptions,
        scripts: impl IntoIterator<Item = Script>,
    ) -> (Self, Arc<ScriptedProvider>) {
        let provider = Arc::new(ScriptedProvider::new(scripts));
        let fixture = Self::configured(config, options, Some(provider.clone())).await;
        (fixture, provider)
    }

    /// Without a provider, the test plays each stream through `streams`.
    async fn configured(
        config: DriverConfig,
        options: WorkerOptions,
        provider: Option<Arc<dyn ModelProvider>>,
    ) -> Self {
        Self::with_configuration(config, options, provider, b"{}").await
    }

    /// A session whose configuration artifact holds `configuration`.
    async fn with_configuration(
        config: DriverConfig,
        options: WorkerOptions,
        provider: Option<Arc<dyn ModelProvider>>,
        configuration: &'static [u8],
    ) -> Self {
        // Prevent paused time from automatically advancing while a real OS
        // thread is syncing. Only tests' explicit advance() fires timers.
        let awake = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        let (ready, receive) = oneshot::channel();
        std::thread::spawn(move || {
            let temp = tempfile::tempdir().unwrap();
            let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
            let configuration = root.put_artifact(configuration).unwrap();
            let header = SessionHeader {
                workspace: "/workspace".into(),
                initial_model: ModelRef {
                    provider_instance: "mock".into(),
                    model: "test".into(),
                },
                policy_revision: "initial".into(),
                configuration,
            };
            let mut env = DeterministicEnv::new(0xffe);
            let session: SessionId = env.entry_id().to_string().parse().unwrap();
            ready.send((temp, root, session, header)).unwrap();
        });
        let (temp, root, session, header) = receive.await.unwrap();
        root.io.test.lock().unwrap().trace.clear();
        root.io.test.lock().unwrap().threads.clear();
        let (opened, streams) = mpsc::unbounded_channel();
        let provider = provider.unwrap_or_else(|| Arc::new(Channel { opened }));
        let handle = SessionHandle::launch(
            root.clone(),
            session.clone(),
            Opening::Create(header, DeterministicEnv::BASE.parse().unwrap()),
            config,
            DeterministicEnv::new(1),
            provider,
            options.clone(),
        )
        .await
        .unwrap();
        Self {
            _temp: temp,
            root,
            session,
            handle,
            streams,
            awake,
            options,
        }
    }

    fn command(n: u64) -> Command {
        let id = uuid::Uuid::from_u128(
            (0x019a_0000_0000_u128 << 80)
                | (0x7 << 76)
                | (0xfff << 64)
                | (0b10 << 62)
                | u128::from(n),
        );
        Command::StartTurn {
            command_id: id.to_string().parse().unwrap(),
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        }
    }

    async fn start(&mut self, n: u64) -> (TurnId, mpsc::Sender<ProviderEvent>, CancellationToken) {
        let Accepted::TurnStarted { turn_id, .. } =
            self.handle.command(Self::command(n)).await.unwrap()
        else {
            panic!("start")
        };
        let (request, stream, cancel) = self.streams.recv().await.unwrap();
        assert!(!request.response_id.as_str().is_empty());
        assert_eq!(request.input.model.model, "test");
        assert!(!cancel.is_cancelled());
        stream.send(ProviderEvent::Started).await.unwrap();
        (turn_id, stream, cancel)
    }

    async fn wait(&self, check: impl Fn(&RuntimeView) -> bool) -> RuntimeView {
        for _ in 0..100_000 {
            let view = self.handle.view().await.unwrap();
            if check(&view) {
                return view;
            }
            tokio::task::yield_now().await;
        }
        panic!("driver did not reach expected view");
    }

    fn gate(&self, point: Point) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, receive) = oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        self.root.io.test.lock().unwrap().gate = Some(Gate {
            point,
            entered,
            release: wait,
        });
        (receive, release)
    }

    async fn reopened(&self) -> SessionSnapshot {
        self.handle.shutdown().await;
        let (worker, snapshot) = StorageWorker::open(self.root.clone(), self.session.clone())
            .await
            .unwrap();
        worker.shutdown().await;
        snapshot
    }
}

fn finished(reason: FinishReason) -> ProviderEvent {
    ProviderEvent::Finished {
        outcome: ProviderOutcome::Completed,
        finish_reason: Some(reason),
        error: None,
        continuation: None,
    }
}
fn delta(text: &str) -> ProviderEvent {
    ProviderEvent::TextDelta {
        block_index: 0,
        text: text.into(),
    }
}
fn idle(view: &RuntimeView) -> bool {
    view.phase == Phase::Durable(RuntimeState::Idle)
}

async fn catch_up(
    subscription: &mut Subscription,
    client: &mut ClientView,
    target: &RuntimeView,
) -> usize {
    let mut resets = 0;
    for _ in 0..10_000 {
        if client == &ClientView::from(target.clone()) {
            return resets;
        }
        match subscription.next().await.expect("subscription closed") {
            SubscriptionItem::Reset(view) => {
                *client = view;
                resets += 1;
            }
            SubscriptionItem::Event(event) => {
                assert_eq!(client.apply(&event), ApplyOutcome::Applied)
            }
        }
    }
    panic!("subscription did not converge");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn end_to_end_replies_follow_sync_and_only_worker_touches_storage() {
    let mut fixture = Fixture::new(config()).await;
    let (entered, release) = fixture.gate(Point::Written);
    let handle = fixture.handle.clone();
    let command = tokio::spawn(async move { handle.command(Fixture::command(1)).await });
    entered.await.unwrap();
    assert!(!command.is_finished(), "reply before sync");
    let before = fixture.handle.view().await.unwrap();
    assert_eq!(before.snapshot.sequence(), 0);
    release.send(()).unwrap();
    let Accepted::TurnStarted { ack, .. } = command.await.unwrap().unwrap() else {
        panic!("start")
    };
    assert_eq!(ack.sequence, 1);
    let (_, stream, _) = fixture.streams.recv().await.unwrap();
    stream.send(ProviderEvent::Started).await.unwrap();
    stream.send(delta("Hello")).await.unwrap();
    let partial = fixture
        .wait(|view| {
            view.partial
                .as_ref()
                .is_some_and(|p| p.blocks.contains_key(&0))
        })
        .await;
    assert_eq!(
        partial.partial.unwrap().blocks[&0],
        PartialBlock::Text("Hello".into())
    );
    stream.send(finished(FinishReason::Stop)).await.unwrap();
    drop(stream);
    let live = fixture.wait(idle).await;
    let snapshot = fixture.reopened().await;
    assert_eq!(snapshot.checksum(), live.snapshot.checksum());
    let trace = fixture.root.io.test.lock().unwrap();
    assert!(trace.trace.contains(&Point::Synced));
    assert!(
        trace
            .threads
            .iter()
            .all(|name| name.starts_with("harness-storage-")),
        "{:?}",
        trace.threads
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unconfirmed_cancellation_drops_late_events_and_releases_at_grace() {
    let mut fixture = Fixture::new(config()).await;
    let (turn_id, stream, cancel) = fixture.start(1).await;
    stream.send(delta("before")).await.unwrap();
    fixture
        .wait(|view| view.partial.as_ref().is_some_and(|p| !p.blocks.is_empty()))
        .await;
    fixture
        .handle
        .command(Command::Cancel {
            turn_id: turn_id.clone(),
        })
        .await
        .unwrap();
    // The token is cancelled, but the stream stays open for a confirmation.
    assert!(cancel.is_cancelled());
    stream.send(delta("late")).await.unwrap();
    assert_eq!(
        fixture.handle.view().await.unwrap().phase,
        Phase::Durable(RuntimeState::Cancelling)
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        fixture.handle.view().await.unwrap().phase,
        Phase::Durable(RuntimeState::Cancelling)
    );
    assert!(!stream.is_closed());
    tokio::time::advance(Duration::from_secs(1)).await;
    let live = fixture.wait(idle).await;
    assert!(stream.is_closed(), "released when the grace period ends");
    assert_eq!(
        live.snapshot
            .turns()
            .find(|(id, _)| **id == turn_id)
            .unwrap()
            .1
            .outcome,
        Some(TurnOutcome::Cancelled)
    );
    let response = live
        .snapshot
        .entries()
        .find_map(|(_, entry)| match &entry.entry.payload {
            EntryPayload::AssistantResponse(response) => Some(response),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        response.content,
        vec![ContentBlock::Text {
            text: "before".into()
        }]
    );
    fixture.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn lagging_and_midstream_subscribers_get_partial_then_converge() {
    let mut fixture = Fixture::new(DriverConfig {
        event_capacity: 4,
        ..config()
    })
    .await;
    let mut lagging = fixture.handle.subscribe();
    let SubscriptionItem::Reset(mut client) = lagging.next().await.unwrap() else {
        panic!("initial reset")
    };
    let (_, stream, _) = fixture.start(1).await;
    for _ in 0..80 {
        stream.send(delta("x")).await.unwrap();
    }
    let live = fixture
        .wait(|view| {
            view.partial
                .as_ref()
                .is_some_and(|p| p.blocks.get(&0) == Some(&PartialBlock::Text("x".repeat(80))))
        })
        .await;
    assert_eq!(catch_up(&mut lagging, &mut client, &live).await, 1);
    let mut joined = fixture.handle.subscribe();
    let SubscriptionItem::Reset(mut joined_client) = joined.next().await.unwrap() else {
        panic!("initial reset")
    };
    assert_eq!(joined_client, ClientView::from(live));
    stream.send(delta("tail")).await.unwrap();
    stream.send(finished(FinishReason::Stop)).await.unwrap();
    drop(stream);
    let live = fixture.wait(idle).await;
    catch_up(&mut lagging, &mut client, &live).await;
    catch_up(&mut joined, &mut joined_client, &live).await;
    assert_eq!(client, joined_client);
    fixture.handle.shutdown().await;
    assert!(joined.next().await.is_none());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn fault_reopen_changes_epoch_and_preserves_existing_subscription() {
    let mut fixture = Fixture::new(config()).await;
    let (_, stream, _) = fixture.start(1).await;
    for _ in 0..100 {
        stream.send(delta("x")).await.unwrap();
    }
    stream.send(finished(FinishReason::Stop)).await.unwrap();
    drop(stream);
    let live = fixture.wait(idle).await;
    let mut subscription = fixture.handle.subscribe();
    let SubscriptionItem::Reset(mut client) = subscription.next().await.unwrap() else {
        panic!("reset")
    };
    assert!(client.view().event_seq > 100);
    fixture.root.io.test.lock().unwrap().fail = Some(Point::BeforeWrite);
    assert_eq!(
        fixture.handle.command(Fixture::command(2)).await,
        Err(Rejection::Faulted)
    );
    let fault = fixture.wait(|view| view.phase == Phase::Faulted).await;
    catch_up(&mut subscription, &mut client, &fault).await;
    let reopened = fixture.handle.reopen().await.unwrap();
    assert_ne!(live.epoch, reopened.epoch);
    assert_eq!(reopened.snapshot.checksum(), live.snapshot.checksum());
    assert!(idle(&reopened));
    assert_eq!(catch_up(&mut subscription, &mut client, &reopened).await, 1);
    assert!(
        matches!(fixture.handle.reopen().await, Err(PersistError::Rejected { code }) if code == "not_faulted")
    );
    fixture.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn uncertain_writes_are_never_retried_and_torn_tails_require_explicit_repair() {
    for point in [Point::PartialWrite, Point::BeforeAck] {
        let mut fixture = Fixture::new(config()).await;
        let mut subscription = fixture.handle.subscribe();
        let SubscriptionItem::Reset(mut client) = subscription.next().await.unwrap() else {
            panic!("initial reset")
        };
        fixture.root.io.test.lock().unwrap().fail = Some(point);
        assert_eq!(
            fixture.handle.command(Fixture::command(1)).await,
            Err(Rejection::Faulted)
        );
        let old = fixture.handle.view().await.unwrap();
        assert_eq!(old.snapshot.sequence(), 0);
        let result = fixture.handle.reopen().await;
        if point == Point::PartialWrite {
            let Err(PersistError::TornTail(torn)) = result else {
                panic!("expected the torn-tail inspection, got {result:?}")
            };
            let inspection = torn.inspection();
            assert_eq!(inspection.snapshot.as_ref().unwrap().sequence(), 0);
            assert!(torn.tail_bytes() > 0);
            assert_eq!(fixture.handle.view().await.unwrap().phase, Phase::Faulted);
            // A second open reports the same evidence, not a live leaked lock.
            assert!(
                matches!(StorageWorker::open(fixture.root.clone(), fixture.session.clone()).await, Err(PersistError::TornTail(again)) if again == torn)
            );
        } else {
            let reopened = result.unwrap();
            assert_ne!(reopened.epoch, old.epoch);
            assert_eq!(reopened.snapshot.sequence(), 1);
            // The durable opening commit is never repeated: a retry learns it,
            // and recovery closes the turn without asking.
            let accepted = fixture.handle.command(Fixture::command(1)).await.unwrap();
            assert!(matches!(accepted, Accepted::TurnStarted { ack, .. } if ack.sequence == 1));
            let live = fixture.wait(idle).await;
            // An epoch reset fetches the current view, which can already be
            // past reopen's intermediate RecoveryRequired view. Catch up to
            // the quiescent recovery result, never that historical checkpoint.
            assert_eq!(catch_up(&mut subscription, &mut client, &live).await, 1);
            assert_eq!(
                finishes(&live),
                [(TurnOutcome::Interrupted, Some("process_restart".into()))]
            );
            assert!(
                fixture.streams.try_recv().is_err(),
                "recovery opened a stream"
            );
        }
        fixture.handle.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn worker_panic_or_lost_reply_faults_pending_commands_without_hanging() {
    for panic in [true, false] {
        let fixture = Fixture::new(config()).await;
        if panic {
            fixture.options.panic_next.store(true, Ordering::SeqCst);
        } else {
            fixture
                .options
                .lost_reply_next
                .store(true, Ordering::SeqCst);
        }
        assert_eq!(
            fixture.handle.command(Fixture::command(1)).await,
            Err(Rejection::Faulted)
        );
        assert_eq!(fixture.handle.view().await.unwrap().phase, Phase::Faulted);
        let reopened = fixture.handle.reopen().await.unwrap();
        assert_eq!(reopened.snapshot.sequence(), if panic { 0 } else { 1 });
        fixture.handle.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shutdown_waits_for_inflight_sync_and_releases_lock_for_immediate_open() {
    let fixture = Fixture::new(config()).await;
    let (entered, release) = fixture.gate(Point::Written);
    let handle = fixture.handle.clone();
    let command = tokio::spawn(async move { handle.command(Fixture::command(1)).await });
    entered.await.unwrap();
    let handle = fixture.handle.clone();
    let shutdown = tokio::spawn(async move { handle.shutdown().await });
    let handle = fixture.handle.clone();
    let concurrent = tokio::spawn(async move { handle.shutdown().await });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(!shutdown.is_finished() && !concurrent.is_finished());
    release.send(()).unwrap();
    command.await.unwrap().unwrap();
    shutdown.await.unwrap();
    concurrent.await.unwrap();
    let (worker, snapshot) = StorageWorker::open(fixture.root.clone(), fixture.session.clone())
        .await
        .unwrap();
    assert_eq!(snapshot.state(), RuntimeState::Idle);
    assert!(
        fixture
            .root
            .io
            .test
            .lock()
            .unwrap()
            .trace
            .contains(&Point::Synced)
    );
    worker.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn commands_during_shutdown_drain_are_rejected_as_shutting_down() {
    // Sent while the drain waits for an in-flight sync and the turn's cancel.
    let fixture = Fixture::new(config()).await;
    let (entered, release) = fixture.gate(Point::Written);
    let handle = fixture.handle.clone();
    let command = tokio::spawn(async move { handle.command(Fixture::command(1)).await });
    entered.await.unwrap();
    let handle = fixture.handle.clone();
    let shutdown = tokio::spawn(async move { handle.shutdown().await });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        fixture.handle.command(Fixture::command(2)).await,
        Err(Rejection::ShuttingDown)
    );
    assert!(
        matches!(fixture.handle.put_artifact(b"late".to_vec()).await, Err(PersistError::Rejected { code }) if code == "shutting_down")
    );
    assert_eq!(fixture.handle.view().await.unwrap().snapshot.sequence(), 0);
    assert!(!shutdown.is_finished());
    release.send(()).unwrap();
    command.await.unwrap().unwrap();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(2)).await;
    shutdown.await.unwrap();

    // Queued after the loop ended, while the drain joins an artifact upload.
    let fixture = Fixture::new(config()).await;
    let (entered, release) = fixture.gate(Point::ArtifactWritten);
    let handle = fixture.handle.clone();
    let upload = tokio::spawn(async move { handle.put_artifact(b"upload".to_vec()).await });
    entered.await.unwrap();
    let handle = fixture.handle.clone();
    let shutdown = tokio::spawn(async move { handle.shutdown().await });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let mut queued = std::pin::pin!(fixture.handle.command(Fixture::command(1)));
    assert!(
        queued
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    release.send(()).unwrap();
    upload.await.unwrap().unwrap();
    assert_eq!(queued.await, Err(Rejection::ShuttingDown));
    shutdown.await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn overlapping_storage_effects_fault_and_keep_the_first_operation() {
    for media in [false, true] {
        let mut fixture = Fixture::new(config()).await;
        let mut command = Fixture::command(1);
        if media {
            let artifact = fixture
                .handle
                .put_artifact(b"image".to_vec())
                .await
                .unwrap();
            let Command::StartTurn { content, .. } = &mut command else {
                unreachable!()
            };
            *content = vec![ContentBlock::Image {
                artifact,
                mime_type: "image/png".into(),
            }];
        }
        let mut subscription = fixture.handle.subscribe();
        let SubscriptionItem::Reset(_) = subscription.next().await.unwrap() else {
            panic!("initial reset")
        };
        fixture
            .options
            .duplicate_storage_next
            .store(true, Ordering::SeqCst);
        assert_eq!(
            fixture.handle.command(command).await,
            Err(Rejection::Faulted)
        );
        loop {
            let Some(SubscriptionItem::Event(event)) = subscription.next().await else {
                panic!("fault event")
            };
            if let EventKind::Fault(cause) = event.kind {
                assert!(matches!(cause, FaultCause::Internal("storage_overlap")));
                break;
            }
        }
        // The first operation still completes exactly once; the second never runs.
        let reopened = fixture.handle.reopen().await.unwrap();
        if media {
            assert_eq!(reopened.snapshot.sequence(), 0);
            assert!(idle(&reopened));
        } else {
            assert_eq!(reopened.snapshot.sequence(), 1);
            assert_eq!(reopened.phase, Phase::RecoveryRequired);
        }
        assert!(fixture.streams.try_recv().is_err());
        fixture.handle.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn timer_fire_from_a_replaced_runtime_is_dropped_after_reopen() {
    let mut fixture = Fixture::new(config()).await;
    let (turn_id, stream, _) = fixture.start(1).await;
    fixture
        .handle
        .command(Command::Cancel { turn_id })
        .await
        .unwrap();
    // The old runtime's TimerId(1) ends this grace period.
    tokio::time::advance(Duration::from_secs(2)).await;
    let old = fixture.wait(idle).await;
    drop(stream);
    fixture.root.io.test.lock().unwrap().fail = Some(Point::BeforeWrite);
    assert_eq!(
        fixture.handle.command(Fixture::command(2)).await,
        Err(Rejection::Faulted)
    );
    let reopened = fixture.handle.reopen().await.unwrap();
    assert_ne!(reopened.epoch, old.epoch);

    // The new runtime restarts its counters and repeats the same sequence, so
    // its idle and grace timers reuse the IDs the old runtime armed.
    let Accepted::TurnStarted { turn_id, .. } =
        fixture.handle.command(Fixture::command(3)).await.unwrap()
    else {
        panic!("start")
    };
    let (_, stream, _) = fixture.streams.recv().await.unwrap();
    stream.send(ProviderEvent::Started).await.unwrap();
    fixture
        .handle
        .command(Command::Cancel {
            turn_id: turn_id.clone(),
        })
        .await
        .unwrap();
    let before = fixture.handle.view().await.unwrap();
    let timers = fixture.handle.timer_sender().await;
    // Fire every low ID of the old epoch; one aliases the armed grace timer.
    for n in 1..=8 {
        timers.send((old.epoch.clone(), TimerId(n))).unwrap();
    }
    // Fires are polled before the mailbox, and the worker answers in order: any
    // commit the fire caused is synced before this upload and in the next view.
    fixture.handle.view().await.unwrap();
    fixture
        .handle
        .put_artifact(b"barrier".to_vec())
        .await
        .unwrap();
    let view = fixture.handle.view().await.unwrap();
    assert_eq!(view.phase, Phase::Durable(RuntimeState::Cancelling));
    assert_eq!(view.active_turn.as_ref(), Some(&turn_id));
    assert_eq!(view.snapshot.sequence(), before.snapshot.sequence());
    // The current runtime's own timer still ends the grace period.
    tokio::time::advance(Duration::from_secs(2)).await;
    let live = fixture.wait(idle).await;
    assert_eq!(
        live.snapshot
            .turns()
            .find(|(id, _)| **id == turn_id)
            .unwrap()
            .1
            .outcome,
        Some(TurnOutcome::Cancelled)
    );
    drop(stream);
    fixture.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn bounded_command_mailbox_rejects_overload_without_waiting() {
    let fixture = Fixture::new(DriverConfig {
        command_capacity: 1,
        ..config()
    })
    .await;
    let mut first = std::pin::pin!(fixture.handle.command(Fixture::command(1)));
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let mut overloaded = std::pin::pin!(fixture.handle.command(Fixture::command(2)));
    assert_eq!(
        overloaded
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Err(Rejection::Overloaded))
    );
    first.await.unwrap();
    // Drop the stream at shutdown; explicit virtual time covers its grace.
    let handle = fixture.handle.clone();
    let closing = tokio::spawn(async move { handle.shutdown().await });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(2)).await;
    closing.await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn media_upload_is_synced_and_missing_or_invalid_refs_write_nothing() {
    let mut fixture = Fixture::new(DriverConfig {
        max_artifact_bytes: 32,
        ..config()
    })
    .await;
    fixture.root.io.test.lock().unwrap().trace.clear();
    assert!(
        matches!(fixture.handle.put_artifact(vec![0; 33]).await, Err(PersistError::Rejected { code }) if code == "artifact_too_large")
    );
    assert!(fixture.root.io.test.lock().unwrap().trace.is_empty());
    let artifact = fixture
        .handle
        .put_artifact(b"image".to_vec())
        .await
        .unwrap();
    assert!(
        fixture
            .root
            .io
            .test
            .lock()
            .unwrap()
            .trace
            .contains(&Point::ArtifactDirectorySynced)
    );
    for bad in [
        ArtifactRef {
            sha256: Digest::of(b"absent"),
            bytes: 6,
        },
        ArtifactRef {
            bytes: 99,
            ..artifact.clone()
        },
    ] {
        let Command::StartTurn { command_id, .. } = Fixture::command(1) else {
            unreachable!()
        };
        assert_eq!(
            fixture
                .handle
                .command(Command::StartTurn {
                    command_id,
                    content: vec![ContentBlock::Image {
                        artifact: bad,
                        mime_type: "image/png".into()
                    }]
                })
                .await,
            Err(Rejection::Invalid("artifact_missing"))
        );
        assert_eq!(fixture.handle.view().await.unwrap().snapshot.sequence(), 0);
    }
    let Command::StartTurn { command_id, .. } = Fixture::command(2) else {
        unreachable!()
    };
    fixture
        .handle
        .command(Command::StartTurn {
            command_id,
            content: vec![ContentBlock::Image {
                artifact: artifact.clone(),
                mime_type: "image/png".into(),
            }],
        })
        .await
        .unwrap();
    let (_, stream, _) = fixture.streams.recv().await.unwrap();
    stream.send(ProviderEvent::Started).await.unwrap();
    stream.send(finished(FinishReason::Stop)).await.unwrap();
    drop(stream);
    fixture.wait(idle).await;
    let snapshot = fixture.reopened().await;
    let user = snapshot
        .entries()
        .find_map(|(_, entry)| match &entry.entry.payload {
            EntryPayload::UserMessage(user) => Some(user),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        user.content,
        vec![ContentBlock::Image {
            artifact,
            mime_type: "image/png".into()
        }]
    );
}

fn finishes(view: &RuntimeView) -> Vec<(TurnOutcome, Option<String>)> {
    view.snapshot
        .events()
        .filter_map(|(_, _, event)| match event {
            JournalEvent::TurnFinished(finished) => Some((finished.outcome, finished.code.clone())),
            _ => None,
        })
        .collect()
}

fn responses(view: &RuntimeView) -> Vec<AssistantResponse> {
    view.snapshot
        .entries()
        .filter_map(|(_, entry)| match &entry.entry.payload {
            EntryPayload::AssistantResponse(response) => Some(response.clone()),
            _ => None,
        })
        .collect()
}

fn error(code: ModelErrorCode) -> ModelError {
    ModelError {
        code,
        message: "scripted".into(),
        provider_code: None,
        retry_after_ms: None,
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn scripted_cancellation_is_confirmed_without_advancing_time() {
    let (fixture, provider) = Fixture::scripted(
        config(),
        WorkerOptions::default(),
        [Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, "partial"),
            Step::AwaitCancel,
            Step::cancelled(),
        ])],
    )
    .await;
    let Accepted::TurnStarted { turn_id, .. } =
        fixture.handle.command(Fixture::command(1)).await.unwrap()
    else {
        panic!("start")
    };
    fixture
        .wait(|view| {
            view.partial
                .as_ref()
                .is_some_and(|p| p.blocks.get(&0) == Some(&PartialBlock::Text("partial".into())))
        })
        .await;
    assert!(matches!(
        fixture.handle.command(Command::Cancel { turn_id }).await,
        Ok(Accepted::Cancelling { .. })
    ));
    // No virtual time passes: the confirmation, not the grace timer, ends it.
    let live = fixture.wait(idle).await;
    assert_eq!(finishes(&live), [(TurnOutcome::Cancelled, None)]);
    let [response] = &responses(&live)[..] else {
        panic!("one response")
    };
    assert_eq!(response.outcome, ResponseOutcome::Cancelled);
    assert_eq!(
        response.content,
        [ContentBlock::Text {
            text: "partial".into()
        }]
    );
    let [record] = &provider.streams()[..] else {
        panic!("one stream")
    };
    assert_eq!(record.request.response_id, response.response_id);
    assert_eq!(record.request.input.model, response.model);
    assert_eq!(record.yielded, 3);
    assert!(record.cancelled && record.dropped);
    fixture.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn scripted_stall_is_cancelled_then_released_when_grace_ends() {
    let (fixture, provider) = Fixture::scripted(
        config(),
        WorkerOptions::default(),
        [Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, "partial"),
            Step::Stall,
        ])],
    )
    .await;
    let Accepted::TurnStarted { turn_id, .. } =
        fixture.handle.command(Fixture::command(1)).await.unwrap()
    else {
        panic!("start")
    };
    fixture
        .wait(|view| view.partial.as_ref().is_some_and(|p| !p.blocks.is_empty()))
        .await;
    fixture
        .handle
        .command(Command::Cancel { turn_id })
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        fixture.handle.view().await.unwrap().phase,
        Phase::Durable(RuntimeState::Cancelling)
    );
    let record = &provider.streams()[0];
    assert!(record.cancelled && !record.dropped, "still listening");
    tokio::time::advance(Duration::from_secs(1)).await;
    let live = fixture.wait(idle).await;
    assert_eq!(finishes(&live), [(TurnOutcome::Cancelled, None)]);
    assert!(provider.streams()[0].dropped);
    fixture.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn scripted_failures_record_their_outcomes_and_release_every_stream() {
    let (fixture, provider) = Fixture::scripted(
        config(),
        WorkerOptions::default(),
        [
            // Setup failure: a terminal event before any start.
            Script::new([Step::failed(error(ModelErrorCode::Authentication))]),
            // Mid-stream failure after partial output.
            Script::new([
                Step::Event(ProviderEvent::Started),
                Step::text(0, "half"),
                Step::failed(error(ModelErrorCode::Transport)),
            ]),
            // Interruption: EOF without a terminal event.
            Script::new([Step::Event(ProviderEvent::Started), Step::text(0, "cut")]),
            // Malformed arguments, then a stall: released without any grace.
            Script::new([
                Step::Event(ProviderEvent::Started),
                Step::Event(ProviderEvent::ToolCallStarted {
                    block_index: 0,
                    name: "read".into(),
                    provider_call_id: None,
                }),
                Step::Event(ProviderEvent::ToolArgumentsDelta {
                    block_index: 0,
                    fragment: "{".into(),
                }),
                Step::Event(ProviderEvent::ToolCallReady { block_index: 0 }),
                Step::Stall,
            ]),
        ],
    )
    .await;
    for n in 1..=4 {
        fixture.handle.command(Fixture::command(n)).await.unwrap();
        fixture
            .wait(|view| idle(view) && finishes(view).len() == n as usize)
            .await;
    }
    let live = fixture.handle.view().await.unwrap();
    let failed = |code: &str| (TurnOutcome::Failed, Some(code.to_owned()));
    assert_eq!(
        finishes(&live),
        [
            failed("authentication"),
            failed("transport"),
            failed("provider_eof"),
            failed("malformed_stream"),
        ]
    );
    let outcomes: Vec<_> = responses(&live)
        .iter()
        .map(|response| (response.outcome, response.content.clone()))
        .collect();
    let text = |text: &str| vec![ContentBlock::Text { text: text.into() }];
    assert_eq!(
        outcomes,
        [
            (ResponseOutcome::Failed, vec![]),
            (ResponseOutcome::Failed, text("half")),
            (ResponseOutcome::Interrupted, text("cut")),
            (ResponseOutcome::Failed, vec![]),
        ]
    );
    assert_eq!(provider.remaining(), 0);
    assert!(provider.streams().iter().all(|record| record.dropped));
    fixture.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancel_preempts_an_always_ready_stream_within_a_few_events() {
    const DELTAS: usize = 20_000;
    let script = Script::new(
        std::iter::once(Step::Event(ProviderEvent::Started))
            .chain((0..DELTAS).map(|_| Step::text(0, "x")))
            .chain([Step::completed(FinishReason::Stop)]),
    );
    let (fixture, provider) = Fixture::scripted(config(), WorkerOptions::default(), [script]).await;
    let Accepted::TurnStarted { turn_id, .. } =
        fixture.handle.command(Fixture::command(1)).await.unwrap()
    else {
        panic!("start")
    };
    // The stream never pends, yet the mailbox is served between its events.
    fixture
        .wait(|view| {
            view.partial.as_ref().is_some_and(
                |p| matches!(p.blocks.get(&0), Some(PartialBlock::Text(t)) if !t.is_empty()),
            )
        })
        .await;
    let yielded = provider.streams()[0].yielded;
    assert!(
        yielded < DELTAS / 10,
        "{yielded} events before the first view"
    );
    assert!(matches!(
        fixture.handle.command(Command::Cancel { turn_id }).await,
        Ok(Accepted::Cancelling { .. })
    ));
    let live = fixture.wait(idle).await;
    let [response] = &responses(&live)[..] else {
        panic!("one response")
    };
    assert_eq!(response.outcome, ResponseOutcome::Cancelled);
    let [ContentBlock::Text { text }] = &response.content[..] else {
        panic!("one text block")
    };
    // `yielded` counts `Started`; every later delta was dropped as stale.
    assert!(
        text.len() < yielded + 3,
        "{} deltas applied, {yielded} events yielded when the cancel was sent",
        text.len()
    );
    fixture.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn provider_idle_timeout_restarts_on_events_and_fails_a_stalled_turn() {
    let (release, wait) = oneshot::channel();
    let config = DriverConfig {
        runtime: RuntimeConfig {
            provider_idle_timeout: Duration::from_secs(10),
            ..known()
        },
        ..config()
    };
    let (fixture, provider) = Fixture::scripted(
        config,
        WorkerOptions::default(),
        [Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, "a"),
            Step::Wait(wait),
            Step::text(0, "b"),
            Step::Stall,
        ])],
    )
    .await;
    fixture.handle.command(Fixture::command(1)).await.unwrap();
    let text = |text: &'static str| {
        move |view: &RuntimeView| {
            view.partial
                .as_ref()
                .is_some_and(|p| p.blocks.get(&0) == Some(&PartialBlock::Text(text.into())))
        }
    };
    fixture.wait(text("a")).await;
    let inferring = Phase::Durable(RuntimeState::Inferring);
    tokio::time::advance(Duration::from_secs(9)).await;
    assert_eq!(fixture.handle.view().await.unwrap().phase, inferring);
    // An event at 9 s restarts the timer: nothing fires at 10 s.
    release.send(()).unwrap();
    fixture.wait(text("ab")).await;
    tokio::time::advance(Duration::from_secs(9)).await;
    assert_eq!(fixture.handle.view().await.unwrap().phase, inferring);
    assert!(!provider.streams()[0].dropped);
    tokio::time::advance(Duration::from_secs(1)).await;
    let live = fixture.wait(idle).await;
    assert_eq!(
        finishes(&live),
        [(TurnOutcome::Failed, Some("provider_stalled".into()))]
    );
    let [response] = &responses(&live)[..] else {
        panic!("one response")
    };
    assert_eq!(response.outcome, ResponseOutcome::Interrupted);
    assert_eq!(response.content, [ContentBlock::Text { text: "ab".into() }]);
    let record = &provider.streams()[0];
    assert!(record.cancelled && record.dropped);
    fixture.handle.shutdown().await;
}

fn tool_script() -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::Started,
        ProviderEvent::ToolCallStarted {
            block_index: 0,
            name: "read".into(),
            provider_call_id: None,
        },
        ProviderEvent::ToolArgumentsDelta {
            block_index: 0,
            fragment: "{}".into(),
        },
        ProviderEvent::ToolCallReady { block_index: 0 },
        finished(FinishReason::ToolCalls),
    ]
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn tool_loop_and_inference_limit_use_core_decisions_through_real_channels() {
    for limit in [1, 2] {
        let mut fixture = Fixture::new(DriverConfig {
            runtime: RuntimeConfig {
                max_inferences_per_turn: limit,
                ..known()
            },
            ..config()
        })
        .await;
        let (turn, stream, _) = fixture.start(1).await;
        for event in tool_script().into_iter().skip(1) {
            stream.send(event).await.unwrap();
        }
        drop(stream);
        if limit == 2 {
            let (_, stream, _) = fixture.streams.recv().await.unwrap();
            stream.send(ProviderEvent::Started).await.unwrap();
            stream.send(delta("done")).await.unwrap();
            stream.send(finished(FinishReason::Stop)).await.unwrap();
        }
        let view = fixture.wait(idle).await;
        let outcome = view
            .snapshot
            .turns()
            .find(|(id, _)| **id == turn)
            .unwrap()
            .1
            .outcome;
        assert_eq!(
            outcome,
            Some(if limit == 1 {
                TurnOutcome::Failed
            } else {
                TurnOutcome::Completed
            })
        );
        assert!(view.snapshot.entries().any(|(_, entry)| matches!(&entry.entry.payload, EntryPayload::ToolResult(result) if result.result.status == ToolStatus::Denied)));
        fixture.handle.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn failed_open_leaves_no_worker_and_poisoned_worker_still_answers() {
    let fixture = Fixture::new(config()).await;
    assert!(
        matches!(StorageWorker::open(fixture.root.clone(), fixture.session.clone()).await,
        Err(PersistError::Rejected { code }) if code == "session_locked")
    );
    fixture.handle.shutdown().await;
    let (worker, snapshot) = StorageWorker::open(fixture.root.clone(), fixture.session.clone())
        .await
        .unwrap();
    let mut env = DeterministicEnv::new(10);
    let configuration =
        SessionConfiguration::load(&fixture.root, snapshot.configuration()).unwrap();
    let mut core = Runtime::new(snapshot, configuration, known(), &mut env);
    let effects = core.step(
        Input::Command {
            request: RequestId(1),
            command: Fixture::command(1),
        },
        &mut env,
    );
    let (commit, command, at) = effects
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Persist {
                commit,
                command,
                at,
                ..
            } => Some((commit, command, at)),
            _ => None,
        })
        .unwrap();
    fixture.root.io.test.lock().unwrap().fail = Some(Point::BeforeWrite);
    assert!(matches!(
        worker
            .sender
            .append(commit.clone(), command.clone(), Vec::new(), at.clone())
            .await,
        Err(PersistError::Io { .. })
    ));
    assert!(matches!(
        worker.sender.append(commit, command, Vec::new(), at).await,
        Err(PersistError::Poisoned)
    ));
    worker.shutdown().await;
    let (worker, _) = StorageWorker::open(fixture.root.clone(), fixture.session.clone())
        .await
        .unwrap();
    worker.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn artifact_io_failure_faults_runtime_even_without_pending_commit() {
    let fixture = Fixture::new(config()).await;
    fixture.root.io.test.lock().unwrap().fail = Some(Point::ArtifactSynced);
    assert!(matches!(
        fixture.handle.put_artifact(b"upload".to_vec()).await,
        Err(PersistError::Io { .. })
    ));
    fixture.wait(|view| view.phase == Phase::Faulted).await;
    assert_eq!(
        fixture.handle.command(Fixture::command(1)).await,
        Err(Rejection::Faulted)
    );
    assert!(idle(&fixture.handle.reopen().await.unwrap()));
    fixture.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn media_preflight_holds_the_exclusive_slot_and_attaches_matching_retries() {
    let mut fixture = Fixture::new(config()).await;
    let reference = fixture
        .handle
        .put_artifact(b"image".to_vec())
        .await
        .unwrap();
    // Hold the worker behind an artifact upload so verification stays pending.
    let (entered, release) = fixture.gate(Point::ArtifactWritten);
    let handle = fixture.handle.clone();
    let upload = tokio::spawn(async move { handle.put_artifact(b"other".to_vec()).await });
    entered.await.unwrap();
    let Command::StartTurn { command_id, .. } = Fixture::command(1) else {
        unreachable!()
    };
    let media = Command::StartTurn {
        command_id,
        content: vec![ContentBlock::Image {
            artifact: reference,
            mime_type: "image/png".into(),
        }],
    };
    let mut first = std::pin::pin!(fixture.handle.command(media.clone()));
    let mut retry = std::pin::pin!(fixture.handle.command(media));
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(
        retry
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(
        fixture.handle.command(Fixture::command(2)).await,
        Err(Rejection::Busy)
    );
    assert_eq!(fixture.handle.view().await.unwrap().snapshot.sequence(), 0);
    release.send(()).unwrap();
    upload.await.unwrap().unwrap();
    assert_eq!(first.await.unwrap(), retry.await.unwrap());
    let (_, stream, _) = fixture.streams.recv().await.unwrap();
    stream.send(ProviderEvent::Started).await.unwrap();
    stream.send(finished(FinishReason::Stop)).await.unwrap();
    fixture.wait(idle).await;
    fixture.handle.shutdown().await;
}

/// Direct-store reference for exactly the same six-commit unavailable-tool
/// workload. This runs on a dedicated benchmark thread, never on Tokio.
fn direct_tool_turns(turns: u64) -> Duration {
    crate::session::state::without_oracle(|| {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
        let header = SessionHeader {
            workspace: "/workspace".into(),
            initial_model: ModelRef {
                provider_instance: "mock".into(),
                model: "test".into(),
            },
            policy_revision: "initial".into(),
            configuration: root.put_artifact(b"{}").unwrap(),
        };
        let mut env = DeterministicEnv::new(1);
        let at = env.now();
        let session = DeterministicEnv::new(0xffe)
            .entry_id()
            .to_string()
            .parse()
            .unwrap();
        let configuration = SessionConfiguration::load(&root, &header.configuration).unwrap();
        let mut store = JournalStore::create(root, session, header, at).unwrap();
        let mut runtime = Runtime::new(store.snapshot(), configuration, known(), &mut env);
        let started = std::time::Instant::now();
        for n in 1..=turns {
            let mut inputs = VecDeque::from([Input::Command {
                request: RequestId(n),
                command: Fixture::command(n),
            }]);
            while let Some(input) = inputs.pop_front() {
                for effect in runtime.step(input, &mut env) {
                    match effect {
                        Effect::Persist {
                            token,
                            mut commit,
                            command,
                            at,
                            ..
                        } => {
                            if let Some((id, command)) = command {
                                store.bind_command(&mut commit, id, &command).unwrap();
                            }
                            let ack = store.append(commit, at).unwrap();
                            inputs.push_front(Input::Persisted {
                                token,
                                ack,
                                snapshot: store.snapshot(),
                            });
                        }
                        Effect::StartInference { inference, .. } => {
                            let script = if inference.0 % 2 == 1 {
                                tool_script()
                            } else {
                                vec![ProviderEvent::Started, finished(FinishReason::Stop)]
                            };
                            for event in script {
                                inputs.push_back(Input::Provider { inference, event });
                            }
                        }
                        _ => {}
                    }
                }
            }
            assert_eq!(runtime.phase(), Phase::Durable(RuntimeState::Idle));
        }
        let elapsed = started.elapsed();
        assert_eq!(store.snapshot().sequence(), turns * 6);
        elapsed
    })
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
#[ignore = "opt-in worker benchmark; measures real elapsed cost of 12,000 synced commits"]
async fn session_handle_tool_turn_benchmark() {
    const TURNS: u64 = 1_000;
    let (send, receive) = oneshot::channel();
    std::thread::Builder::new()
        .name("harness-storage-benchmark-direct".into())
        .spawn(move || {
            send.send(direct_tool_turns(TURNS)).unwrap();
        })
        .unwrap();
    let direct = receive.await.unwrap();
    let scripts = (0..TURNS).flat_map(|_| {
        [
            Script::new(tool_script().into_iter().map(Step::Event)),
            Script::new([
                Step::Event(ProviderEvent::Started),
                Step::completed(FinishReason::Stop),
            ]),
        ]
    });
    let (fixture, _) = Fixture::scripted(
        config(),
        WorkerOptions {
            without_oracle: true,
            ..WorkerOptions::default()
        },
        scripts,
    )
    .await;
    fixture.awake.abort();
    let mut subscription = fixture.handle.subscribe();
    subscription.next().await.unwrap();
    // Real monotonic time is used only in this explicit opt-in benchmark.
    let started = std::time::Instant::now();
    for n in 1..=TURNS {
        fixture.handle.command(Fixture::command(n)).await.unwrap();
        loop {
            match subscription.next().await.unwrap() {
                SubscriptionItem::Event(RuntimeEvent {
                    kind: EventKind::TurnFinished { .. },
                    ..
                }) => break,
                SubscriptionItem::Reset(client) if idle(client.view()) => break,
                _ => {}
            }
        }
    }
    let worker = started.elapsed();
    assert_eq!(
        fixture.handle.view().await.unwrap().snapshot.sequence(),
        TURNS * 6
    );
    fixture.handle.shutdown().await;
    let per_turn = |duration: Duration| duration.as_secs_f64() * 1000.0 / TURNS as f64;
    eprintln!(
        "{TURNS} identical six-commit tool turns: direct JournalStore {:.3} ms/turn; SessionHandle {:.3} ms/turn; channels/driver/worker delta {:+.3} ms/turn ({:+.1}%)",
        per_turn(direct),
        per_turn(worker),
        per_turn(worker) - per_turn(direct),
        (worker.as_secs_f64() / direct.as_secs_f64() - 1.0) * 100.0
    );
}

/// The default runtime configuration with a limit for the test model, so its
/// budget is known; the default catalog is empty.
fn known() -> RuntimeConfig {
    let mut config = RuntimeConfig::default();
    config.context.catalog.insert(
        test_model(),
        ModelLimits {
            context_tokens: Some(1_000_000),
            max_output_tokens: None,
        },
    );
    config
}

fn config() -> DriverConfig {
    DriverConfig {
        runtime: known(),
        ..DriverConfig::default()
    }
}

fn test_model() -> ModelRef {
    ModelRef {
        provider_instance: "mock".into(),
        model: "test".into(),
    }
}

fn reply(text: &str) -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, text),
        Step::completed(FinishReason::Stop),
    ])
}

/// A projection that does not fit, or whose budget is unknown, fails the turn
/// from `Preparing`: no C2, and the provider is never asked for a stream.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn context_overflow_fails_the_turn_before_any_stream_opens() {
    let mut config = config();
    // The default deductions total 16,384 tokens, leaving one usable token;
    // "hello" is estimated at two.
    config.runtime.context.catalog.insert(
        test_model(),
        ModelLimits {
            context_tokens: Some(16_385),
            max_output_tokens: None,
        },
    );
    let (fixture, provider) =
        Fixture::scripted(config, WorkerOptions::default(), [reply("unused")]).await;
    let before = fixture.handle.view().await.unwrap().snapshot.sequence();
    assert_eq!(
        fixture.handle.command(Fixture::command(1)).await,
        Err(Rejection::ContextBudgetExceeded {
            reason: Some(crate::context::compaction::NotEligible::Empty)
        })
    );
    let view = fixture.handle.view().await.unwrap();
    assert_eq!(view.snapshot.sequence(), before);
    assert!(finishes(&view).is_empty());
    assert!(provider.streams().is_empty());
    fixture.handle.shutdown().await;
}

/// RFC-001 C01: with no limit for the model, execution refuses to infer.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unknown_budget_fails_the_turn_before_any_stream_opens() {
    let flags = refused_before_any_stream(DriverConfig::default(), "context_budget_unknown").await;
    assert!(flags.budget_unknown && !flags.overflow && !flags.preview);
}

/// Start a turn that `config` refuses in `Preparing` with `code`, check that
/// nothing reached the provider, and return the flags `ContextBuilt` carried.
async fn refused_before_any_stream(config: DriverConfig, code: &str) -> Flags {
    let (fixture, provider) =
        Fixture::scripted(config, WorkerOptions::default(), [reply("unused")]).await;
    let mut subscription = fixture.handle.subscribe();
    // Take the reset first, so the turn's events arrive as events.
    assert!(matches!(
        subscription.next().await,
        Some(SubscriptionItem::Reset(_))
    ));
    assert!(matches!(
        fixture.handle.command(Fixture::command(1)).await,
        Ok(Accepted::TurnStarted { .. })
    ));
    let view = fixture.wait(idle).await;
    assert_eq!(finishes(&view), [(TurnOutcome::Failed, Some(code.into()))]);
    assert!(provider.streams().is_empty(), "a stream was opened");
    assert_eq!(provider.remaining(), 1);
    let inferring = view.snapshot.events().any(|(_, _, event)| {
        matches!(event, JournalEvent::StateChanged(c) if c.to == RuntimeState::Inferring)
    });
    assert!(!inferring, "C2 was committed");
    // Subscribers learn why from `ContextBuilt` before the turn finishes.
    let mut built = None;
    while let Some(item) = subscription.next().await {
        match item {
            SubscriptionItem::Event(RuntimeEvent {
                kind: EventKind::ContextBuilt { flags, .. },
                ..
            }) => built = Some(flags),
            SubscriptionItem::Event(RuntimeEvent {
                kind: EventKind::TurnFinished { code: finished, .. },
                ..
            }) => {
                assert_eq!(finished.as_deref(), Some(code));
                break;
            }
            _ => {}
        }
    }
    let flags = built.expect("ContextBuilt precedes TurnFinished");
    // Inspection keeps answering, with the same flags.
    let report = fixture.handle.inspect_context(None, None).await.unwrap();
    assert_eq!(report.flags, flags);
    fixture.handle.shutdown().await;
    flags
}

/// The worker decodes the configuration artifact, so its instructions reach
/// every request; `inspect_context` reads any branch without moving the head.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn requests_carry_the_projection_and_inspection_reads_any_branch() {
    let provider = Arc::new(ScriptedProvider::new([
        reply("one"),
        reply("two"),
        reply("three"),
    ]));
    let fixture = Fixture::with_configuration(
        config(),
        WorkerOptions::default(),
        Some(provider.clone()),
        br#"{"instructions":[{"level":"system","text":"Be brief."}]}"#,
    )
    .await;
    fixture.handle.command(Fixture::command(1)).await.unwrap();
    let first = fixture.wait(idle).await.snapshot.head().cloned();
    fixture.handle.command(Fixture::command(2)).await.unwrap();
    let second = fixture.wait(idle).await;
    let second_head = second.snapshot.head().cloned();
    let [first_stream, second_stream] = &provider.streams()[..] else {
        panic!("two streams")
    };
    let kinds = |items: &[ContextItem]| -> Vec<&'static str> {
        items
            .iter()
            .map(|item| match item {
                ContextItem::Instructions { .. } => "instructions",
                ContextItem::User { .. } => "user",
                ContextItem::Assistant { .. } => "assistant",
                ContextItem::ToolResult { .. } => "tool_result",
                ContextItem::Summary { .. } => "summary",
                ContextItem::Request { .. } => "request",
            })
            .collect()
    };
    assert_eq!(
        kinds(&first_stream.request.input.items),
        ["instructions", "user"]
    );
    let input = &second_stream.request.input;
    assert_eq!(
        kinds(&input.items),
        ["instructions", "user", "assistant", "user"]
    );
    // The request is exactly what inspection rebuilds at its head.
    let user = input.items[3].entry_id().unwrap().clone();
    let inspected = fixture
        .handle
        .inspect_context(Some(user), None)
        .await
        .unwrap();
    assert_eq!(inspected.fingerprint, fingerprint(input));

    // Start a sibling of the second turn, then inspect the old branch.
    assert!(matches!(
        fixture
            .handle
            .command(Command::SelectHead {
                head: first.clone()
            })
            .await,
        Ok(Accepted::HeadSelected { .. })
    ));
    fixture.handle.command(Fixture::command(3)).await.unwrap();
    let third = fixture.wait(idle).await;
    let current = third.snapshot.head().cloned();
    let sibling = fixture
        .handle
        .inspect_context(second_head.clone(), None)
        .await
        .unwrap();
    assert_eq!(sibling.identity.head, second_head);
    assert_eq!(sibling.included.last().unwrap().entry_id, second_head);
    let own = fixture.handle.inspect_context(None, None).await.unwrap();
    assert_eq!(own.identity.head, current);
    assert!(own.included.iter().all(|item| item.entry_id != second_head));
    assert_ne!(own.fingerprint, sibling.fingerprint);
    let view = fixture.handle.view().await.unwrap();
    assert_eq!(
        view.snapshot.head().cloned(),
        current,
        "inspection moved the head"
    );
    assert_eq!(view.snapshot.sequence(), third.snapshot.sequence());
    // A preview for another model changes the model and nothing else here.
    let other = ModelRef {
        provider_instance: "other".into(),
        model: "model".into(),
    };
    let preview = fixture
        .handle
        .inspect_context(None, Some(other.clone()))
        .await
        .unwrap();
    assert!(preview.flags.preview);
    assert_eq!(preview.identity.model, other);
    assert!(
        preview
            .excluded
            .iter()
            .all(|e| e.reason != ExclusionReason::ProviderStateNotPortable)
    );
    assert_eq!(
        fixture
            .handle
            .inspect_context(Some(EntryId::new()), None)
            .await,
        Err(InspectError::Context(ContextError::UnknownHead))
    );
    fixture.handle.shutdown().await;
    assert_eq!(
        fixture.handle.inspect_context(None, None).await,
        Err(InspectError::Closed)
    );
}

/// An invalid configuration artifact refuses to open rather than building
/// projections without its instructions.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn open_refuses_a_configuration_with_invalid_instructions() {
    let fixture = Fixture::scripted(config(), WorkerOptions::default(), [])
        .await
        .0;
    fixture.handle.shutdown().await;
    let root = fixture.root.clone();
    let configuration = root
        .put_artifact(br#"{"instructions":"be brief"}"#)
        .unwrap();
    let header = SessionHeader {
        workspace: "/workspace".into(),
        initial_model: test_model(),
        policy_revision: "initial".into(),
        configuration,
    };
    let session = SessionId::new();
    let created = SessionHandle::create(
        root,
        session,
        header,
        Arc::new(ScriptedProvider::new([])),
        config(),
    )
    .await;
    assert!(matches!(
        created,
        Err(PersistError::Rejected { code }) if code == "invalid_instructions"
    ));
}

// ----- startup recovery and tail repair ---------------------------------------

/// Keeps paused time from auto-advancing while real threads do I/O.
struct Awake(JoinHandle<()>);

impl Awake {
    fn new() -> Self {
        Self(tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        }))
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Build a journal with plain storage on another thread, as a crashed
/// process left it.
fn journal_on_thread<T: Send + 'static>(
    build: impl FnOnce(&Arc<DataRoot>) -> T + Send + 'static,
) -> (tempfile::TempDir, Arc<DataRoot>, T) {
    std::thread::spawn(move || {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
        let value = build(&root);
        (temp, root, value)
    })
    .join()
    .unwrap()
}

/// Open `session` through a handle whose provider is a scripted mock with no
/// scripts: any stream it opened would be recorded.
async fn open_scripted(
    root: &Arc<DataRoot>,
    session: &SessionId,
    config: DriverConfig,
) -> (SessionHandle, Arc<ScriptedProvider>) {
    let provider = Arc::new(ScriptedProvider::new([]));
    let handle = SessionHandle::launch(
        root.clone(),
        session.clone(),
        Opening::Open,
        config,
        DeterministicEnv::new(2),
        provider.clone(),
        WorkerOptions::default(),
    )
    .await
    .unwrap();
    (handle, provider)
}

async fn wait_for(handle: &SessionHandle, check: impl Fn(&RuntimeView) -> bool) -> RuntimeView {
    for _ in 0..100_000 {
        let view = handle.view().await.unwrap();
        if check(&view) {
            return view;
        }
        tokio::task::yield_now().await;
    }
    panic!("handle did not reach the expected view");
}

/// Recovery wrote no assistant response and no executor result: the only
/// responses are those `before` already had.
fn invented_nothing(before: usize, view: &RuntimeView) {
    assert_eq!(responses(view).len(), before, "recovery wrote a response");
    assert!(view.snapshot.entries().all(|(_, stored)| !matches!(
        &stored.entry.payload,
        EntryPayload::ToolResult(r) if r.result.metadata.provenance == Provenance::Executor
    )));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn open_closes_an_interrupted_turn_without_any_stream() {
    let _awake = Awake::new();
    // A crash left a turn in `Inferring`: C1 and C2 are durable.
    let (_temp, root, session) = journal_on_thread(|root| {
        let configuration = root.put_artifact(b"{}").unwrap();
        let header = SessionHeader {
            workspace: "/workspace".into(),
            initial_model: test_model(),
            policy_revision: "initial".into(),
            configuration: configuration.clone(),
        };
        let at: crate::session::Timestamp = DeterministicEnv::BASE.parse().unwrap();
        let mut env = DeterministicEnv::new(0xffe);
        let session: SessionId = env.entry_id().to_string().parse().unwrap();
        let mut store =
            JournalStore::create(root.clone(), session.clone(), header, at.clone()).unwrap();
        let (turn, user) = (env.turn_id(), env.entry_id());
        let state = |from, to| {
            JournalEvent::StateChanged(StateChanged {
                turn_id: turn.clone(),
                from,
                to,
            })
        };
        let opening = Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: None,
                to: Some(user.clone()),
                reason: HeadReason::Advance,
            },
            entries: vec![Entry {
                id: user.clone(),
                parent_id: None,
                turn_id: Some(turn.clone()),
                payload: EntryPayload::UserMessage(UserMessage {
                    content: vec![ContentBlock::Text {
                        text: "interrupted".into(),
                    }],
                }),
            }],
            events: vec![
                JournalEvent::TurnStarted(TurnStarted {
                    turn_id: turn.clone(),
                    user_entry_id: user.clone(),
                    configuration,
                }),
                state(RuntimeState::Idle, RuntimeState::Preparing),
            ],
        };
        store.append(opening, at.clone()).unwrap();
        let infer = Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: Some(user.clone()),
                to: Some(user),
                reason: HeadReason::Retain,
            },
            entries: Vec::new(),
            events: vec![state(RuntimeState::Preparing, RuntimeState::Inferring)],
        };
        store.append(infer, at).unwrap();
        session
    });
    let (handle, provider) = open_scripted(&root, &session, config()).await;
    let live = wait_for(&handle, idle).await;
    assert_eq!(
        finishes(&live),
        [(TurnOutcome::Interrupted, Some("process_restart".into()))]
    );
    assert!(provider.streams().is_empty(), "recovery opened a stream");
    invented_nothing(0, &live);
    handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn uncertain_invocation_waits_for_the_user_through_the_handle() {
    let _awake = Awake::new();
    let (_temp, root, session) = journal_on_thread(|root| {
        let examples =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/rfcs/001/examples");
        for entry in std::fs::read_dir(examples.join("artifacts")).unwrap() {
            root.put_artifact(&std::fs::read(entry.unwrap().path()).unwrap())
                .unwrap();
        }
        let session: SessionId = "019a0000-0000-7000-8000-000000000002".parse().unwrap();
        let mut file = root.journal(&session, true, true).unwrap();
        std::io::Write::write_all(
            &mut file,
            &std::fs::read(examples.join("interrupted-execution.jsonl")).unwrap(),
        )
        .unwrap();
        file.sync_all().unwrap();
        session
    });
    let (handle, provider) = open_scripted(&root, &session, config()).await;
    let mut subscription = handle.subscribe();
    let waiting = wait_for(&handle, |view| view.phase == Phase::AwaitingResolution).await;
    let SubscriptionItem::Reset(client) = subscription.next().await.unwrap() else {
        panic!("reset")
    };
    assert_eq!(client.view().awaiting, waiting.awaiting);
    let [uncertain] = &waiting.awaiting[..] else {
        panic!("one invocation waits")
    };
    assert_eq!(uncertain.tool, "read");
    assert_eq!(
        handle.command(Fixture::command(1)).await,
        Err(Rejection::ResolutionRequired)
    );
    let before = responses(&waiting).len();
    let accepted = handle
        .command(Command::ResolveUncertain {
            invocation_id: uncertain.invocation_id.clone(),
            resolution: UserResolution::Observed {
                status: ToolStatus::Succeeded,
                text: "README.md was read".into(),
                file_effect: None,
            },
        })
        .await
        .unwrap();
    assert!(
        matches!(accepted, Accepted::Resolved { invocation_id, .. } if invocation_id == uncertain.invocation_id)
    );
    let live = wait_for(&handle, idle).await;
    assert!(live.awaiting.is_empty());
    assert!(provider.streams().is_empty(), "recovery opened a stream");
    invented_nothing(before, &live);
    handle.shutdown().await;
    // Resolved for good: reopening needs no recovery.
    let (handle, _) = open_scripted(&root, &session, config()).await;
    assert_eq!(
        handle.view().await.unwrap().phase,
        Phase::Durable(RuntimeState::Idle)
    );
    handle.shutdown().await;
}

/// A turn run through a handle over a v1 journal continues it as v2: a v1
/// prefix, a v2 suffix and one unbroken checksum chain.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_turn_appended_to_a_v1_journal_is_written_as_v4() {
    let _awake = Awake::new();
    let examples =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/rfcs/001/examples");
    let v1 = std::fs::read(examples.join("compaction.jsonl")).unwrap();
    let v1_records = v1.split(|b| *b == b'\n').filter(|l| !l.is_empty()).count() as u64;
    let (_temp, root, session) = journal_on_thread(move |root| {
        for entry in std::fs::read_dir(examples.join("artifacts")).unwrap() {
            root.put_artifact(&std::fs::read(entry.unwrap().path()).unwrap())
                .unwrap();
        }
        let session: SessionId = "019a0000-0000-7000-8000-000000000003".parse().unwrap();
        let mut file = root.journal(&session, true, true).unwrap();
        std::io::Write::write_all(&mut file, &v1).unwrap();
        file.sync_all().unwrap();
        session
    });
    let mut config = config();
    config.runtime.context.catalog.insert(
        ModelRef {
            provider_instance: "mock-default".into(),
            model: "scripted-text".into(),
        },
        crate::context::ModelLimits {
            context_tokens: Some(128_000),
            max_output_tokens: None,
        },
    );
    let provider = Arc::new(ScriptedProvider::new([reply("answered as v3")]));
    let handle = SessionHandle::launch(
        root.clone(),
        session.clone(),
        Opening::Open,
        config,
        DeterministicEnv::new(2),
        provider,
        WorkerOptions::default(),
    )
    .await
    .unwrap();
    handle.command(Fixture::command(1)).await.unwrap();
    let live = wait_for(&handle, |view| {
        idle(view) && view.snapshot.sequence() >= v1_records
    })
    .await;
    let [.., response] = &responses(&live)[..] else {
        panic!("a response")
    };
    assert!(response.projection_fingerprint.is_some());
    handle.shutdown().await;
    let (_, _, (inspection, versions)) = journal_on_thread(move |_| {
        let inspection = JournalStore::inspect(&root, &session).unwrap();
        let path = root
            .path()
            .join("sessions")
            .join(format!("{session}.jsonl"));
        let versions: Vec<u64> = std::fs::read(path)
            .unwrap()
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| {
                serde_json::from_slice::<serde_json::Value>(l).unwrap()["record"]["version"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        (inspection, versions)
    });
    assert_eq!(inspection.diagnostic, None, "the chain verifies");
    let new_records = live.snapshot.sequence() + 1 - v1_records;
    assert_eq!(new_records, 3, "opening, inference and response commits");
    let mut expected = vec![1; v1_records as usize];
    expected.extend(std::iter::repeat_n(4, new_records as usize));
    assert_eq!(versions, expected);
    assert_eq!(
        inspection.versions,
        [(1, v1_records), (4, new_records)].into()
    );
}

/// Fault a fixture's open turn with a torn C3: the stream finished, but
/// only half of its response commit reached the journal.
async fn tear_response(fixture: &mut Fixture) {
    let (_, stream, _) = fixture.start(1).await;
    stream.send(delta("partial")).await.unwrap();
    fixture.root.io.test.lock().unwrap().fail = Some(Point::PartialWrite);
    stream.send(finished(FinishReason::Stop)).await.unwrap();
    drop(stream);
    fixture.wait(|view| view.phase == Phase::Faulted).await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn torn_tail_is_repaired_on_a_faulted_handle_only_with_its_inspection() {
    let mut fixture = Fixture::new(config()).await;
    let mut subscription = fixture.handle.subscribe();
    let SubscriptionItem::Reset(mut client) = subscription.next().await.unwrap() else {
        panic!("reset")
    };
    tear_response(&mut fixture).await;
    let old = fixture.handle.view().await.unwrap();
    let Err(PersistError::TornTail(torn)) = fixture.handle.reopen().await else {
        panic!("reopen must report the torn tail")
    };
    let inspection = torn.inspection().clone();
    let prefix = inspection.snapshot.as_ref().unwrap();
    assert_eq!(
        prefix.state(),
        RuntimeState::Inferring,
        "the valid prefix ends mid-turn"
    );
    let other = Digest::of(b"another file");
    for (fingerprint, size) in [
        (other, inspection.file_bytes),
        (inspection.fingerprint.clone(), inspection.file_bytes - 1),
    ] {
        assert_eq!(
            fixture
                .handle
                .reopen_repairing_tail(fingerprint, size)
                .await,
            Err(RepairError::StaleRepair)
        );
        assert_eq!(fixture.handle.view().await.unwrap().phase, Phase::Faulted);
    }
    let repaired = fixture
        .handle
        .reopen_repairing_tail(inspection.fingerprint.clone(), inspection.file_bytes)
        .await
        .unwrap();
    assert_ne!(repaired.epoch, old.epoch);
    let live = fixture.wait(idle).await;
    // The old subscription resets to the new epoch, then follows recovery.
    assert!(catch_up(&mut subscription, &mut client, &live).await >= 1);
    assert_eq!(
        finishes(&live),
        [(TurnOutcome::Interrupted, Some("process_restart".into()))]
    );
    let repair = live
        .snapshot
        .events()
        .find_map(|(_, _, event)| match event {
            JournalEvent::TailRepaired(repair) => Some(repair.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(repair.valid_bytes, inspection.valid_bytes);
    assert_eq!(repair.quarantine_artifact.bytes, torn.tail_bytes());
    assert!(
        responses(&live).is_empty(),
        "the torn response is not invented"
    );
    assert_eq!(
        fixture
            .handle
            .reopen_repairing_tail(inspection.fingerprint, inspection.file_bytes)
            .await,
        Err(RepairError::NotFaulted)
    );
    fixture.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn repair_tail_refuses_stale_locked_and_complete_corrupt_journals() {
    let mut fixture = Fixture::new(config()).await;
    let provider = || -> Arc<dyn ModelProvider> { Arc::new(ScriptedProvider::new([])) };
    let (root, session) = (fixture.root.clone(), fixture.session.clone());
    let repair = |fingerprint: Digest, size: u64| {
        SessionHandle::repair_tail(
            root.clone(),
            session.clone(),
            fingerprint,
            size,
            provider(),
            config(),
        )
    };
    // A live engine holds the session.
    assert!(matches!(
        repair(Digest::of(b"x"), 1).await,
        Err(RepairError::Locked)
    ));
    tear_response(&mut fixture).await;
    fixture.handle.shutdown().await;
    let Err(PersistError::TornTail(torn)) = SessionHandle::open(
        fixture.root.clone(),
        fixture.session.clone(),
        provider(),
        config(),
    )
    .await
    else {
        panic!("open must report the torn tail")
    };
    let inspection = torn.inspection();
    assert!(matches!(
        repair(Digest::of(b"x"), inspection.file_bytes).await,
        Err(RepairError::StaleRepair)
    ));
    assert!(matches!(
        repair(inspection.fingerprint.clone(), inspection.file_bytes + 1).await,
        Err(RepairError::StaleRepair)
    ));
    let handle = repair(inspection.fingerprint.clone(), inspection.file_bytes)
        .await
        .unwrap();
    let live = wait_for(&handle, idle).await;
    assert_eq!(
        finishes(&live),
        [(TurnOutcome::Interrupted, Some("process_restart".into()))]
    );
    handle.shutdown().await;

    // A complete corrupt record is never repairable, and nothing is written.
    let path = fixture
        .root
        .path()
        .join("sessions")
        .join(format!("{}.jsonl", fixture.session));
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, b"{\"not\":\"a record\"}\n").unwrap();
    }
    let corrupt = JournalStore::inspect(&fixture.root, &fixture.session).unwrap();
    assert_eq!(
        corrupt.diagnostic.as_ref().unwrap().kind,
        DiagnosticKind::Corrupt
    );
    assert!(matches!(
        SessionHandle::open(
            fixture.root.clone(),
            fixture.session.clone(),
            provider(),
            config()
        )
        .await,
        Err(PersistError::Rejected { .. })
    ));
    assert!(matches!(
        repair(corrupt.fingerprint.clone(), corrupt.file_bytes).await,
        Err(RepairError::NotRepairable)
    ));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), corrupt.file_bytes);

    // Nor is a torn file without a valid header.
    let (_temp, root, session) = journal_on_thread(|root| {
        let session: SessionId = DeterministicEnv::new(0xffa)
            .entry_id()
            .to_string()
            .parse()
            .unwrap();
        let mut file = root.journal(&session, true, true).unwrap();
        std::io::Write::write_all(&mut file, b"{\"record\":").unwrap();
        session
    });
    let headless = JournalStore::inspect(&root, &session).unwrap();
    assert!(headless.snapshot.is_none());
    assert!(matches!(
        SessionHandle::repair_tail(
            root,
            session,
            headless.fingerprint,
            headless.file_bytes,
            provider(),
            config(),
        )
        .await,
        Err(RepairError::NotRepairable)
    ));
}
