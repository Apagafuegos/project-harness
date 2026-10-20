//! Real worker/channels on a current-thread runtime, with explicit virtual time.

use std::{
    collections::VecDeque,
    sync::atomic::Ordering,
    task::{Context, Poll, Waker},
    time::Duration,
};

use super::*;
use crate::session::{
    io::{Gate, Point},
    *,
};

struct Scripted {
    opened: mpsc::UnboundedSender<(InferenceRequest, mpsc::Sender<ProviderEvent>)>,
    scripts: VecDeque<Vec<ProviderEvent>>,
}

impl Provider for Scripted {
    fn start(&mut self, request: InferenceRequest) -> mpsc::Receiver<ProviderEvent> {
        let (send, receive) = mpsc::channel(256);
        if let Some(script) = self.scripts.pop_front() {
            for event in script {
                send.try_send(event).unwrap();
            }
        } else {
            self.opened.send((request, send)).unwrap();
        }
        receive
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: Arc<DataRoot>,
    session: SessionId,
    handle: SessionHandle,
    streams: mpsc::UnboundedReceiver<(InferenceRequest, mpsc::Sender<ProviderEvent>)>,
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
        Self::configured(config, WorkerOptions::default(), VecDeque::new()).await
    }

    async fn configured(
        config: DriverConfig,
        options: WorkerOptions,
        scripts: VecDeque<Vec<ProviderEvent>>,
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
            let configuration = root.put_artifact(b"{}").unwrap();
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
        let provider = Scripted { opened, scripts };
        let handle = SessionHandle::launch(
            root.clone(),
            session.clone(),
            Some((header, DeterministicEnv::BASE.parse().unwrap())),
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

    async fn start(&mut self, n: u64) -> (TurnId, mpsc::Sender<ProviderEvent>) {
        let Accepted::TurnStarted { turn_id, .. } =
            self.handle.command(Self::command(n)).await.unwrap()
        else {
            panic!("start")
        };
        let (request, stream) = self.streams.recv().await.unwrap();
        assert_eq!(request.turn_id, turn_id);
        assert_eq!(request.inference, InferenceId(n));
        assert!(!request.response_id.as_str().is_empty());
        assert!(!request.head.as_str().is_empty());
        assert_eq!(request.model.model, "test");
        stream.send(ProviderEvent::Started).await.unwrap();
        (turn_id, stream)
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
    let mut fixture = Fixture::new(DriverConfig::default()).await;
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
    let (_, stream) = fixture.streams.recv().await.unwrap();
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
async fn cancellation_grace_uses_virtual_time_and_drops_late_events() {
    let mut fixture = Fixture::new(DriverConfig::default()).await;
    let (turn_id, stream) = fixture.start(1).await;
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
    assert!(stream.send(delta("late")).await.is_err());
    assert_eq!(
        fixture.handle.view().await.unwrap().phase,
        Phase::Durable(RuntimeState::Cancelling)
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        fixture.handle.view().await.unwrap().phase,
        Phase::Durable(RuntimeState::Cancelling)
    );
    tokio::time::advance(Duration::from_secs(1)).await;
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
        ..DriverConfig::default()
    })
    .await;
    let mut lagging = fixture.handle.subscribe();
    let SubscriptionItem::Reset(mut client) = lagging.next().await.unwrap() else {
        panic!("initial reset")
    };
    let (_, stream) = fixture.start(1).await;
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
    let mut fixture = Fixture::new(DriverConfig::default()).await;
    let (_, stream) = fixture.start(1).await;
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
        let fixture = Fixture::new(DriverConfig::default()).await;
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
            assert!(matches!(result, Err(PersistError::Rejected { .. })));
            assert_eq!(fixture.handle.view().await.unwrap().phase, Phase::Faulted);
            // A second open reports the same evidence, not a live leaked lock.
            assert!(
                matches!(StorageWorker::open(fixture.root.clone(), fixture.session.clone()).await, Err(PersistError::Rejected { code }) if code != "session_locked")
            );
        } else {
            let reopened = result.unwrap();
            assert_ne!(reopened.epoch, old.epoch);
            assert_eq!(catch_up(&mut subscription, &mut client, &reopened).await, 1);
            assert_eq!(reopened.snapshot.sequence(), 1);
            assert_eq!(reopened.phase, Phase::RecoveryRequired);
            let accepted = fixture.handle.command(Fixture::command(1)).await.unwrap();
            assert!(matches!(accepted, Accepted::TurnStarted { ack, .. } if ack.sequence == 1));
            assert_eq!(
                fixture.handle.command(Fixture::command(2)).await,
                Err(Rejection::RecoveryRequired)
            );
        }
        fixture.handle.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn worker_panic_or_lost_reply_faults_pending_commands_without_hanging() {
    for panic in [true, false] {
        let fixture = Fixture::new(DriverConfig::default()).await;
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
    let fixture = Fixture::new(DriverConfig::default()).await;
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
    let fixture = Fixture::new(DriverConfig::default()).await;
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
    let fixture = Fixture::new(DriverConfig::default()).await;
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
        let mut fixture = Fixture::new(DriverConfig::default()).await;
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
    let mut fixture = Fixture::new(DriverConfig::default()).await;
    let (turn_id, stream) = fixture.start(1).await;
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

    // The new runtime restarts its counters, so its cancel timer is TimerId(1)
    // again: the same ID the old runtime armed for its cancel.
    let Accepted::TurnStarted { turn_id, .. } =
        fixture.handle.command(Fixture::command(3)).await.unwrap()
    else {
        panic!("start")
    };
    let (request, stream) = fixture.streams.recv().await.unwrap();
    assert_eq!(request.inference, InferenceId(1));
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
    timers.send((old.epoch.clone(), TimerId(1))).unwrap();
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
        ..DriverConfig::default()
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
        ..DriverConfig::default()
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
    let (_, stream) = fixture.streams.recv().await.unwrap();
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
                ..RuntimeConfig::default()
            },
            ..DriverConfig::default()
        })
        .await;
        let (turn, stream) = fixture.start(1).await;
        for event in tool_script().into_iter().skip(1) {
            stream.send(event).await.unwrap();
        }
        drop(stream);
        if limit == 2 {
            let (_, stream) = fixture.streams.recv().await.unwrap();
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
    let fixture = Fixture::new(DriverConfig::default()).await;
    assert!(
        matches!(StorageWorker::open(fixture.root.clone(), fixture.session.clone()).await,
        Err(PersistError::Rejected { code }) if code == "session_locked")
    );
    fixture.handle.shutdown().await;
    let (worker, snapshot) = StorageWorker::open(fixture.root.clone(), fixture.session.clone())
        .await
        .unwrap();
    let mut env = DeterministicEnv::new(10);
    let mut core = Runtime::new(snapshot, RuntimeConfig::default(), &mut env);
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
            .append(commit.clone(), command.clone(), at.clone())
            .await,
        Err(PersistError::Io { .. })
    ));
    assert!(matches!(
        worker.sender.append(commit, command, at).await,
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
    let fixture = Fixture::new(DriverConfig::default()).await;
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
    let mut fixture = Fixture::new(DriverConfig::default()).await;
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
    let (_, stream) = fixture.streams.recv().await.unwrap();
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
        let mut store = JournalStore::create(root, session, header, at).unwrap();
        let mut runtime = Runtime::new(store.snapshot(), RuntimeConfig::default(), &mut env);
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
    let scripts = (0..TURNS)
        .flat_map(|_| {
            [
                tool_script(),
                vec![ProviderEvent::Started, finished(FinishReason::Stop)],
            ]
        })
        .collect();
    let fixture = Fixture::configured(
        DriverConfig::default(),
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
