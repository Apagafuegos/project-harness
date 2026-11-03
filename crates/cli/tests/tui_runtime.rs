//! Offline terminal-loop tests, with real storage and scripted inference.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures_util::{Stream, future::BoxFuture};
use harness::{
    tui::{
        App, SessionInfo,
        runtime::{self, Commands, Signal},
    },
    wiring::{self, SessionOptions},
};
use harness_core::{
    context::ModelLimits,
    model::{
        ProviderEvent,
        scripted::{Script, ScriptedProvider, Step},
    },
    runtime::*,
    session::*,
};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;
use std::{io, sync::Arc, time::Duration};

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}
fn ctrl(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}
fn reply(text: &str) -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, text),
        Step::completed(FinishReason::Stop),
    ])
}
fn stalled() -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, "partial"),
        Step::Stall,
    ])
}

struct Fixture {
    _temp: tempfile::TempDir,
    options: SessionOptions,
    handle: SessionHandle,
    session: SessionId,
}
impl Fixture {
    async fn new(scripts: Vec<Script>, capacity: usize) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("work");
        std::fs::create_dir(&workspace).unwrap();
        let model = ModelRef {
            provider_instance: "mock".into(),
            model: "scripted".into(),
        };
        let mut driver = DriverConfig {
            event_capacity: capacity,
            ..Default::default()
        };
        driver.runtime.cancel_grace = Duration::from_millis(100);
        driver.runtime.context.catalog.insert(
            model.clone(),
            ModelLimits {
                context_tokens: Some(128_000),
                max_output_tokens: None,
            },
        );
        let options = SessionOptions {
            data_root: temp.path().join("state"),
            workspace,
            session: None,
            model,
            explicit_model: false,
            explicit_provider: false,
            switch_policy: Portability::Reject,
            known_instances: vec![],
            summary_model: None,
            compaction_strategy: harness::wiring::creation_compaction_strategy(),
            explicit_strategy: false,
            driver,
            sandbox: None,
        };
        let opened = wiring::open_session(&options, Arc::new(ScriptedProvider::new(scripts)))
            .await
            .unwrap();
        Self {
            _temp: temp,
            options,
            handle: opened.handle,
            session: opened.session,
        }
    }
    fn app(&self) -> App {
        let mut app = App::new(SessionInfo {
            workspace: self.options.workspace.clone(),
            model: self.options.model.clone(),
            context_limit: None,
        });
        app.set_catalog(self.options.driver.runtime.context.catalog.clone());
        app
    }
    fn snapshot(&self) -> SessionSnapshot {
        let root = DataRoot::open(&self.options.data_root, false).unwrap();
        JournalStore::inspect(&root, &self.session)
            .unwrap()
            .snapshot
            .unwrap()
    }
}
async fn wait(handle: &SessionHandle, condition: impl Fn(&RuntimeView) -> bool) {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if condition(&handle.view().await.unwrap()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "view deadline: {:?}",
        handle.view().await.unwrap()
    );
}
fn subscription(handle: &SessionHandle) -> impl Stream<Item = SubscriptionItem> + use<> {
    let mut sub = handle.subscribe();
    async_stream::stream! { while let Some(item) = sub.next().await { yield item; } }
}
fn text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}
async fn drive(
    f: &Fixture,
    events: impl Stream<Item = io::Result<Event>>,
    commands: &impl Commands,
) -> (App, Terminal<TestBackend>, runtime::Stats) {
    let mut app = f.app();
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
    let stats = tokio::time::timeout(
        Duration::from_secs(15),
        runtime::drive(
            &mut terminal,
            &mut app,
            events,
            subscription(&f.handle),
            futures_util::stream::pending::<Signal>(),
            commands,
            || {},
        ),
    )
    .await
    .expect("loop deadline")
    .unwrap();
    (app, terminal, stats)
}

#[tokio::test]
async fn streamed_answer_paste_resize_and_ctrl_d() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let f = Fixture::new(
        vec![Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, "streamed-one"),
            Step::Wait(rx),
            Step::text(0, "-two"),
            Step::completed(FinishReason::Stop),
        ])],
        64,
    )
    .await;
    let f = &f;
    let events = async_stream::stream! {
        wait(&f.handle, |v| v.phase == Phase::Durable(RuntimeState::Idle)).await;
        yield Ok(Event::Paste("hello\nworld".into()));
        yield Ok(key(KeyCode::Enter));
        wait(&f.handle, |v| v.partial.is_some()).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        yield Ok(Event::Resize(110, 35));
        tx.send(()).unwrap();
        wait(&f.handle, |v| v.active_turn.is_none()).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        yield Ok(ctrl('d'));
    };
    let (_, terminal, stats) = drive(f, events, &f.handle).await;
    assert!(stats.frames >= 3);
    assert!(text(&terminal).contains("streamed-one-two"));
    assert!(text(&terminal).contains("128k"));
    f.handle.shutdown().await;
    assert!(f.snapshot().entries().any(|(_, e)| matches!(&e.entry.payload, EntryPayload::UserMessage(m) if matches!(&m.content[0], ContentBlock::Text { text } if text == "hello\nworld"))));
}

#[tokio::test]
async fn approval_allow_deny_and_dismiss() {
    for (answer, allowed) in [
        (KeyCode::Char('a'), true),
        (KeyCode::Char('d'), false),
        (KeyCode::Esc, false),
        (KeyCode::Char('r'), true),
    ] {
        let proposal = Script::new([
            Step::Event(ProviderEvent::Started),
            Step::Event(ProviderEvent::ToolCallStarted {
                block_index: 0,
                name: "write".into(),
                provider_call_id: None,
            }),
            Step::Event(ProviderEvent::ToolArgumentsDelta {
                block_index: 0,
                fragment: json!({"path":"created", "text":"safe"}).to_string(),
            }),
            Step::Event(ProviderEvent::ToolCallReady { block_index: 0 }),
            Step::completed(FinishReason::ToolCalls),
        ]);
        let f = Fixture::new(vec![proposal, reply("tool done")], 64).await;
        let f = &f;
        let events = async_stream::stream! {
            wait(&f.handle, |v| v.phase == Phase::Durable(RuntimeState::Idle)).await;
            yield Ok(Event::Paste("do tool".into())); yield Ok(key(KeyCode::Enter));
            wait(&f.handle, |v| v.pending_approval.is_some()).await;
            tokio::time::sleep(Duration::from_millis(30)).await;
            yield Ok(key(answer));
            wait(&f.handle, |v| v.active_turn.is_none()).await;
            yield Ok(ctrl('d'));
        };
        drive(f, events, &f.handle).await;
        f.handle.shutdown().await;
        assert_eq!(f.options.workspace.join("created").exists(), allowed);
        assert!(f.snapshot().entries().any(|(_, e)| matches!(&e.entry.payload, EntryPayload::ToolResult(result) if result.result.status == if allowed { ToolStatus::Succeeded } else { ToolStatus::Denied })));
    }
}

#[tokio::test]
async fn esc_cancel_and_double_ctrl_c_quit_are_durable() {
    for double in [false, true] {
        let f = Fixture::new(vec![stalled()], 64).await;
        let f = &f;
        let events = async_stream::stream! {
            wait(&f.handle, |v| v.phase == Phase::Durable(RuntimeState::Idle)).await;
            yield Ok(Event::Paste("run".into())); yield Ok(key(KeyCode::Enter));
            wait(&f.handle, |v| v.partial.is_some()).await;
            // The loop must consume the initial view and active turn first.
            tokio::time::sleep(Duration::from_millis(30)).await;
            if double {
                yield Ok(ctrl('c')); yield Ok(ctrl('c'));
            } else {
                yield Ok(key(KeyCode::Esc));
                wait(&f.handle, |v| v.active_turn.is_none()).await;
                yield Ok(ctrl('d'));
            }
        };
        drive(f, events, &f.handle).await;
        // Assert before shutdown: shutdown must not mask an early loop return.
        let view = f.handle.view().await.unwrap();
        assert!(view.active_turn.is_none());
        assert!(view.snapshot.events().any(|(_, _, e)| matches!(e, JournalEvent::TurnFinished(f) if f.outcome == TurnOutcome::Cancelled)));
        f.handle.shutdown().await;
        assert!(f.snapshot().events().any(|(_, _, e)| matches!(e, JournalEvent::TurnFinished(f) if f.outcome == TurnOutcome::Cancelled)));
    }
}

struct Slow(std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>);
impl Commands for Slow {
    fn command(&self, _: Command) -> BoxFuture<'static, Result<Accepted, Rejection>> {
        let release = self.0.lock().unwrap().take().unwrap();
        Box::pin(async move {
            release.await.unwrap();
            Err(Rejection::Busy)
        })
    }
}
#[tokio::test]
async fn slow_command_does_not_block_ticks_and_rejection_is_visible() {
    let f = Fixture::new(vec![], 64).await;
    let f = &f;
    let (release, gate) = tokio::sync::oneshot::channel();
    let slow = Slow(std::sync::Mutex::new(Some(gate)));
    let events = async_stream::stream! {
        wait(&f.handle, |v| v.phase == Phase::Durable(RuntimeState::Idle)).await;
        yield Ok(Event::Paste("pending".into())); yield Ok(key(KeyCode::Enter));
        tokio::time::sleep(Duration::from_millis(250)).await;
        // Only input progression can release this command. An inline await
        // deadlocks, rather than merely shifting the measured tick window.
        release.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        yield Ok(ctrl('d'));
    };
    let (_, terminal, stats) = drive(f, events, &slow).await;
    assert!(stats.pending_ticks > 0, "ticks: {stats:?}");
    assert!(text(&terminal).contains("command rejected: busy"));
    f.handle.shutdown().await;
}

#[tokio::test]
async fn faulted_reset_still_renders_banner_and_accepts_quit() {
    let f = Fixture::new(vec![], 64).await;
    let mut view = f.handle.view().await.unwrap();
    view.phase = Phase::Faulted;
    let subscriptions =
        futures_util::stream::iter([SubscriptionItem::Reset(ClientView::from(view))])
            .chain(futures_util::stream::pending());
    use futures_util::StreamExt;
    let events = async_stream::stream! {
        tokio::time::sleep(Duration::from_millis(50)).await;
        yield Ok(ctrl('d'));
    };
    let mut app = f.app();
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
    runtime::drive(
        &mut terminal,
        &mut app,
        events,
        subscriptions,
        futures_util::stream::pending(),
        &f.handle,
        || {},
    )
    .await
    .unwrap();
    assert!(text(&terminal).contains("runtime fault"));
    assert!(text(&terminal).contains("faulted"));
    f.handle.shutdown().await;
}

#[tokio::test]
async fn lag_reset_does_not_duplicate_rows() {
    let f = Fixture::new(vec![reply("unique-answer")], 1).await;
    let mut sub = f.handle.subscribe();
    let first = sub.next().await.unwrap();
    f.handle
        .command(Command::StartTurn {
            command_id: CommandId::new(),
            content: vec![ContentBlock::Text {
                text: "prompt".into(),
            }],
        })
        .await
        .unwrap();
    wait(&f.handle, |v| v.active_turn.is_none()).await;
    let reset = sub.next().await.unwrap();
    assert!(matches!(reset, SubscriptionItem::Reset(_)));
    let subscriptions = futures_util::stream::iter([first, reset.clone(), reset])
        .chain(futures_util::stream::pending());
    use futures_util::StreamExt;
    let events = async_stream::stream! { tokio::time::sleep(Duration::from_millis(50)).await; yield Ok(ctrl('d')); };
    let mut app = f.app();
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
    runtime::drive(
        &mut terminal,
        &mut app,
        events,
        subscriptions,
        futures_util::stream::pending(),
        &f.handle,
        || {},
    )
    .await
    .unwrap();
    assert_eq!(text(&terminal).matches("unique-answer").count(), 1);
    f.handle.shutdown().await;
}

/// The plain-harness entry path creates sessions through the same shared wiring
/// as `ask`, so the chosen new-session strategy is persisted in the creation
/// configuration. The parser default itself is proven in `ask_cli`'s tests.
#[tokio::test]
async fn plain_tui_new_session_records_the_shared_masking_default() {
    let f = Fixture::new(Vec::new(), 16).await;
    let view = f.handle.view().await.unwrap();
    let root = DataRoot::open(&f.options.data_root, false).unwrap();
    let bytes = root
        .read_artifact(view.snapshot.configuration(), 1024 * 1024)
        .unwrap();
    let configuration: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        f.options.compaction_strategy,
        harness::wiring::creation_compaction_strategy()
    );
    assert_eq!(
        configuration["compaction"]["strategy"],
        "observation_masking"
    );
    assert!(configuration["compaction"]["summary_model"].is_null());
    assert_eq!(configuration["compaction"]["automatic"], false);
    f.handle.shutdown().await;
}

#[tokio::test]
async fn activity_ticks_are_injected_only_during_active_turns_and_idle_has_no_timer() {
    let idle = Fixture::new(vec![], 64).await;
    let events = async_stream::stream! {
        tokio::time::sleep(Duration::from_millis(300)).await;
        yield Ok(ctrl('d'));
    };
    let (_, _, stats) = drive(&idle, events, &idle.handle).await;
    assert_eq!(stats.activity_ticks, 0);
    assert!(stats.ticks <= 2, "idle render timer kept waking: {stats:?}");
    idle.handle.shutdown().await;
    let busy = Fixture::new(vec![stalled()], 64).await;
    let f = &busy;
    let events = async_stream::stream! {
        yield Ok(Event::Paste("work".into()));yield Ok(key(KeyCode::Enter));
        wait(&f.handle,|v|v.partial.is_some()).await;
        tokio::time::sleep(Duration::from_millis(350)).await;
        yield Ok(key(KeyCode::Esc));
        wait(&f.handle,|v|v.active_turn.is_none()).await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        yield Ok(ctrl('d'));
    };
    let (_, _, stats) = drive(f, events, &f.handle).await;
    assert!(stats.activity_ticks >= 3, "{stats:?}");
    f.handle.shutdown().await;
}
