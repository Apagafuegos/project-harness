//! Offline inspector acceptance: pure App buffers and the real terminal loop,
//! real JSONL storage and scripted session/summary model requests.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures_util::{Stream, future::BoxFuture};
use harness::tui::{
    Action, App, SessionInfo,
    runtime::{self, Commands, Signal},
};
use harness_core::{
    context::{ContextReport, ModelLimits},
    model::{
        ProviderEvent,
        scripted::{Script, ScriptedProvider, Step},
    },
    runtime::*,
    session::*,
};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const SENTINEL: &str = "SENTINEL-NATIVE-SECRET-9f31";
const SUMMARY: &str = "## Task and constraints\nKeep the task.\n## Decisions\nUse Rust.\n## Files and actions\nNo files changed.\n## Outcomes\nThree turns completed.\n## Unresolved work\nContinue testing.";
fn model(name: &str) -> ModelRef {
    ModelRef {
        provider_instance: "mock".into(),
        model: name.into(),
    }
}
fn reply(text: &str) -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, text),
        Step::completed(FinishReason::Stop),
    ])
}
fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}
fn ctrl(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}
fn press(app: &mut App, code: KeyCode) -> Vec<Action> {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
}
fn open(app: &mut App) {
    app.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
}
async fn frame_tick() {
    tokio::time::sleep(Duration::from_millis(80)).await;
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: Arc<DataRoot>,
    session: SessionId,
    handle: SessionHandle,
    provider: Arc<ScriptedProvider>,
    config: DriverConfig,
}
impl Fixture {
    async fn new(scripts: Vec<Script>, automatic: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let configuration = root
            .put_artifact(
                &serde_json::to_vec(&json!({
                    "instructions": [{"level":"system", "text":"Inspector fixture instructions"}],
                    "compaction": {"summary_model": model("summary"), "automatic": automatic}
                }))
                .unwrap(),
            )
            .unwrap();
        let mut config = DriverConfig::default();
        for name in ["session", "other", "summary"] {
            config.runtime.context.catalog.insert(
                model(name),
                ModelLimits {
                    context_tokens: Some(128_000),
                    max_output_tokens: None,
                },
            );
        }
        if automatic {
            config.runtime.context.compaction_trigger_permille = 1;
        }
        let provider = Arc::new(ScriptedProvider::new(scripts));
        let session = SessionId::new();
        let handle = SessionHandle::create(
            root.clone(),
            session.clone(),
            SessionHeader {
                workspace: workspace.to_str().unwrap().into(),
                initial_model: model("session"),
                policy_revision: "inspector".into(),
                configuration,
            },
            provider.clone(),
            config.clone(),
        )
        .await
        .unwrap();
        Self {
            _temp: temp,
            root,
            session,
            handle,
            provider,
            config,
        }
    }
    fn app(&self) -> App {
        let mut app = App::new(SessionInfo {
            workspace: self.root.path().into(),
            model: model("session"),
            context_limit: None,
        });
        app.set_catalog(self.config.runtime.context.catalog.clone());
        app
    }
    fn journal(&self) -> Vec<u8> {
        std::fs::read(
            self.root
                .path()
                .join("sessions")
                .join(format!("{}.jsonl", self.session)),
        )
        .unwrap()
    }
    async fn reset(&self, app: &mut App) {
        app.on_subscription(SubscriptionItem::Reset(ClientView::from(
            self.handle.view().await.unwrap(),
        )));
    }
    async fn turn(&self, text: &str) {
        self.handle
            .command(Command::StartTurn {
                command_id: CommandId::new(),
                content: vec![ContentBlock::Text { text: text.into() }],
            })
            .await
            .unwrap();
        wait(&self.handle, |v| {
            v.active_turn.is_none() && v.phase == Phase::Durable(RuntimeState::Idle)
        })
        .await;
    }
    async fn three_turns(&self) {
        for n in 0..3 {
            self.turn(&format!("question {n}")).await;
        }
    }
}
async fn wait(handle: &SessionHandle, predicate: impl Fn(&RuntimeView) -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if predicate(&handle.view().await.unwrap()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("view deadline");
}
fn subscriptions(handle: &SessionHandle) -> impl Stream<Item = SubscriptionItem> + use<> {
    let mut sub = handle.subscribe();
    async_stream::stream! { while let Some(item) = sub.next().await { yield item; } }
}
async fn drive(
    f: &Fixture,
    app: &mut App,
    events: impl Stream<Item = io::Result<Event>>,
    commands: &impl Commands,
) -> (Terminal<TestBackend>, runtime::Stats) {
    drive_signals(
        f,
        app,
        events,
        futures_util::stream::pending::<Signal>(),
        commands,
    )
    .await
}

async fn drive_signals(
    f: &Fixture,
    app: &mut App,
    events: impl Stream<Item = io::Result<Event>>,
    signals: impl Stream<Item = Signal>,
    commands: &impl Commands,
) -> (Terminal<TestBackend>, runtime::Stats) {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let stats = tokio::time::timeout(
        Duration::from_secs(5),
        runtime::drive(
            &mut terminal,
            app,
            events,
            subscriptions(&f.handle),
            signals,
            commands,
            || {},
        ),
    )
    .await
    .expect("loop blocked")
    .unwrap();
    (terminal, stats)
}
fn screen(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content
        .chunks(terminal.backend().buffer().area.width as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    screen(&terminal)
}
fn pages(app: &mut App) -> String {
    let mut out = String::new();
    for _ in 0..20 {
        out.push_str(&render(app, 80, 24));
        out.push('\n');
        press(app, KeyCode::PageDown);
    }
    for _ in 0..20 {
        press(app, KeyCode::PageUp);
    }
    out
}
/// A lag reset can skip a committed turn and arrive during the next stream.
/// Compare every panel with a fresh client of exactly that authoritative view.
#[tokio::test]
async fn lag_reset_mid_stream_replaces_transcript_inspector_and_branch_tree() {
    let f = Fixture::new(
        vec![
            reply("old answer"),
            Script::new([
                Step::Event(ProviderEvent::Started),
                Step::text(0, "new streaming answer"),
                Step::Stall,
            ]),
        ],
        false,
    )
    .await;
    f.turn("old question").await;
    let old_view = f.handle.view().await.unwrap();
    let old_report = f.handle.inspect_context(None, None).await.unwrap();
    f.handle
        .command(Command::StartTurn {
            command_id: CommandId::new(),
            content: vec![ContentBlock::Text {
                text: "new question".into(),
            }],
        })
        .await
        .unwrap();
    wait(&f.handle, |v| {
        v.partial.as_ref().is_some_and(|p| !p.blocks.is_empty())
    })
    .await;
    let new_view = f.handle.view().await.unwrap();
    let new_report = f.handle.inspect_context(None, None).await.unwrap();
    assert_ne!(
        old_report.identity.journal_sequence,
        new_report.identity.journal_sequence
    );

    for panel in [None, Some('k'), Some('t')] {
        let mut lagged = f.app();
        lagged.on_subscription(SubscriptionItem::Reset(ClientView::from(old_view.clone())));
        let mut fresh = f.app();
        fresh.on_subscription(SubscriptionItem::Reset(ClientView::from(new_view.clone())));
        if let Some(key) = panel {
            for app in [&mut lagged, &mut fresh] {
                app.on_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL));
            }
        }
        if panel == Some('k') {
            lagged.on_context_report(Ok(old_report.clone()));
            assert!(render(&mut lagged, 120, 50).contains("old question"));
        }
        lagged.on_subscription(SubscriptionItem::Reset(ClientView::from(new_view.clone())));
        assert_eq!(
            render(&mut lagged, 120, 50),
            render(&mut fresh, 120, 50),
            "panel {panel:?} retained pre-reset state"
        );
        // Late old reports must not repopulate the panel after invalidation.
        if panel == Some('k') {
            lagged.on_context_report(Ok(old_report.clone()));
            assert_eq!(render(&mut lagged, 120, 50), render(&mut fresh, 120, 50));
            for app in [&mut lagged, &mut fresh] {
                app.on_context_report(Ok(new_report.clone()));
            }
        }
        let screen = render(&mut lagged, 120, 50);
        assert_eq!(screen, render(&mut fresh, 120, 50));
        if panel != Some('k') {
            assert!(screen.contains("new question"));
        }
        if panel.is_none() {
            assert!(screen.contains("new streaming answer"));
        }
    }
    f.handle.shutdown().await;
}

async fn preview(
    f: &Fixture,
) -> (
    EntryId,
    harness_core::context::compaction::CompactionPreview,
) {
    let Accepted::CompactionPreviewed {
        preview_id,
        preview,
    } = f.handle.command(Command::PreviewCompaction).await.unwrap()
    else {
        panic!("preview expected")
    };
    (preview_id, *preview)
}
fn start_preview(
    app: &mut App,
    id: EntryId,
    preview: harness_core::context::compaction::CompactionPreview,
) {
    assert_eq!(
        press(app, KeyCode::Char('c')),
        vec![Action::Command(Command::PreviewCompaction)]
    );
    app.on_compaction_preview(id, Ok(preview));
}

#[tokio::test]
async fn pure_panel_budget_order_exclusions_configuration_and_preview_label() {
    let f = Fixture::new(
        vec![Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, "included answer"),
            Step::Event(ProviderEvent::ReasoningSummaryDelta {
                block_index: 1,
                text: "display only".into(),
            }),
            Step::completed(FinishReason::Stop),
        ])],
        false,
    )
    .await;
    f.turn("included question").await;
    let mut app = f.app();
    f.reset(&mut app).await;
    open(&mut app);
    app.on_context_report(Ok(f.handle.inspect_context(None, None).await.unwrap()));
    println!(
        "INSPECTOR_RENDER_BEGIN\n{}\nINSPECTOR_RENDER_END",
        render(&mut app, 80, 24)
    );
    let all = pages(&mut app);
    for expected in [
        "Limit 128000",
        "output reserve 4096",
        "Used estimate",
        "remaining estimate",
        "Flags:",
        "Included items",
        "Instructions",
        "User",
        "Assistant",
        "included question",
        "included answer",
        "reasoning_display_only",
        "Fingerprint:",
        "Configuration:",
        "projection v",
    ] {
        assert!(all.contains(expected), "missing {expected}: {all}");
    }
    assert!(all.find("Instructions").unwrap() < all.find("User").unwrap());
    assert!(all.find("User").unwrap() < all.find("Assistant").unwrap());
    press(&mut app, KeyCode::Char('m'));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    app.on_context_report(Ok(f
        .handle
        .inspect_context(None, Some(model("other")))
        .await
        .unwrap()));
    assert!(render(&mut app, 80, 24).contains("MODEL OVERRIDE PREVIEW"));
    f.handle.shutdown().await;
}

#[tokio::test]
async fn loop_panel_report_refreshes_after_committed_turn() {
    let fixture = Fixture::new(vec![reply("refreshed answer")], false).await;
    let f = &fixture;
    let mut app = f.app();
    let events = async_stream::stream! {
        yield Ok(ctrl('k')); frame_tick().await;
        f.turn("refreshed question").await;
        frame_tick().await; frame_tick().await; frame_tick().await; frame_tick().await;
        yield Ok(ctrl('d'));
    };
    let (_, stats) = drive(f, &mut app, events, &f.handle).await;
    let all = pages(&mut app);
    assert!(all.contains("refreshed question") && all.contains("refreshed answer"));
    assert!(stats.frames >= 2);
    f.handle.shutdown().await;
}

#[tokio::test]
async fn loop_model_override_uses_merged_catalog_and_never_switches() {
    let f = Fixture::new(vec![], false).await;
    let before = f.journal();
    let mut app = f.app();
    let events = async_stream::stream! {
        yield Ok(ctrl('k')); frame_tick().await; yield Ok(key(KeyCode::Char('m')));
        frame_tick().await; yield Ok(key(KeyCode::Down)); yield Ok(key(KeyCode::Enter));
        frame_tick().await; yield Ok(ctrl('d'));
    };
    let (terminal, _) = drive(&f, &mut app, events, &f.handle).await;
    let text = screen(&terminal);
    assert!(
        text.contains("mock/other") && text.contains("nothing switched"),
        "{text}"
    );
    assert_eq!(
        f.handle
            .view()
            .await
            .unwrap()
            .snapshot
            .model_at(None)
            .unwrap(),
        &model("session")
    );
    assert_eq!(f.journal(), before);
    assert!(f.provider.streams().is_empty());
    f.handle.shutdown().await;
}

#[tokio::test]
async fn preview_first_requires_a_render_and_explicit_enter() {
    let f = Fixture::new(
        vec![reply("one"), reply("two"), reply("three"), reply(SUMMARY)],
        false,
    )
    .await;
    f.three_turns().await;
    let before = f.journal();
    let (id, proposed) = preview(&f).await;
    let mut app = f.app();
    f.reset(&mut app).await;
    open(&mut app);
    start_preview(&mut app, id.clone(), proposed);
    assert!(
        press(&mut app, KeyCode::Enter).is_empty(),
        "arrival is not a shown preview"
    );
    render(&mut app, 10, 2);
    assert!(
        press(&mut app, KeyCode::Enter).is_empty(),
        "clipped panes cannot authorize"
    );
    println!(
        "COMPACTION_RENDER_BEGIN\n{}\nCOMPACTION_RENDER_END",
        render(&mut app, 80, 24)
    );
    for _ in 0..4 {
        press(&mut app, KeyCode::PageDown);
    }
    println!(
        "COMPACTION_DIFF_RENDER_BEGIN\n{}\nCOMPACTION_DIFF_RENDER_END",
        render(&mut app, 80, 24)
    );
    for _ in 0..8 {
        press(&mut app, KeyCode::PageUp);
    }
    render(&mut app, 80, 24);
    let all = pages(&mut app);
    for expected in [
        "COMPACTION PREVIEW",
        "Task and constraints",
        "Source manifest",
        "covered",
        "superseded summaries",
        "CompactionDiff",
        "fits budget true",
        "instructions:",
        "tool calls:",
        "TOTAL:",
    ] {
        assert!(all.contains(expected), "missing {expected}");
    }
    assert_eq!(f.journal(), before);
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        vec![Action::Command(Command::CommitCompaction {
            preview_id: id
        })]
    );
    let requests = f.provider.streams();
    assert_eq!(
        requests.last().unwrap().request.input.model,
        model("summary")
    );
    f.handle.shutdown().await;
}

#[tokio::test]
async fn loop_compaction_preview_then_commit_summary_in_transcript_and_report() {
    let fixture = Fixture::new(
        vec![reply("one"), reply("two"), reply("three"), reply(SUMMARY)],
        false,
    )
    .await;
    let f = &fixture;
    f.three_turns().await;
    let mut app = f.app();
    // End the first loop with a shown but uncommitted preview.
    let events = async_stream::stream! {
        yield Ok(ctrl('k')); yield Ok(key(KeyCode::Char('c')));
        wait_summary_done(&f.provider, 4).await;
        frame_tick().await; frame_tick().await; yield Ok(ctrl('d'));
    };
    drive(f, &mut app, events, &f.handle).await;
    assert!(pages(&mut app).contains("CompactionDiff"));
    let events = async_stream::stream! {
        yield Ok(key(KeyCode::Enter)); frame_tick().await; frame_tick().await; yield Ok(ctrl('d'));
    };
    drive(f, &mut app, events, &f.handle).await;
    let all = pages(&mut app);
    assert!(
        all.contains("Summary") && all.contains("covered_by_summary"),
        "{all}"
    );
    open(&mut app); // close inspector, leaving the durable transcript
    let transcript = render(&mut app, 80, 45);
    assert!(transcript.contains("compaction summary") && transcript.contains("Keep the task."));
    assert_eq!(f.provider.streams().len(), 4);
    f.handle.shutdown().await;
}

#[tokio::test]
async fn loop_preview_cancel_leaves_journal_bytes_identical() {
    let f = Fixture::new(
        vec![reply("one"), reply("two"), reply("three"), reply(SUMMARY)],
        false,
    )
    .await;
    f.three_turns().await;
    let before = f.journal();
    let mut app = f.app();
    let events = async_stream::stream! {
        yield Ok(ctrl('k')); yield Ok(key(KeyCode::Char('c'))); frame_tick().await; frame_tick().await;
        yield Ok(key(KeyCode::Esc)); frame_tick().await; yield Ok(ctrl('d'));
    };
    drive(&f, &mut app, events, &f.handle).await;
    assert!(pages(&mut app).contains("nothing committed"));
    assert_eq!(f.journal(), before);
    f.handle.shutdown().await;
}

async fn wait_streams(provider: &ScriptedProvider, count: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while provider.streams().len() < count {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("provider stream deadline");
}

/// Wait until the summary generation stream is consumed and dropped, which
/// precedes its preview reply reaching the app.
async fn wait_summary_done(provider: &ScriptedProvider, count: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let streams = provider.streams();
            if streams.len() >= count && streams.last().is_some_and(|stream| stream.dropped) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("summary stream deadline");
}

#[tokio::test]
async fn loop_generation_shows_progress_then_quit_cancels_without_waiting() {
    let fixture = Fixture::new(
        vec![
            reply("one"),
            reply("two"),
            reply("three"),
            Script::new([Step::Stall]),
        ],
        false,
    )
    .await;
    let f = &fixture;
    f.three_turns().await;
    let before = f.journal();
    let mut app = f.app();
    let events = async_stream::stream! {
        yield Ok(ctrl('k')); yield Ok(key(KeyCode::Char('c')));
        // Keep the input stream open; only the signal may end the loop.
        std::future::pending::<()>().await;
    };
    // Terminate after generation starts: restoration stops drawing, so the
    // final buffer is exactly the frame rendered while the summary stalled.
    let signals = async_stream::stream! {
        wait_streams(&f.provider, 4).await;
        frame_tick().await; frame_tick().await;
        yield Signal::Terminate;
    };
    let (terminal, _) = drive_signals(f, &mut app, events, signals, &f.handle).await;
    assert!(
        screen(&terminal).contains("Generating compaction preview"),
        "{}",
        screen(&terminal)
    );
    assert_eq!(f.journal(), before);
    assert!(f.provider.streams().last().unwrap().dropped);
    f.handle.shutdown().await;
}

#[tokio::test]
async fn loop_stale_preview_names_the_changed_field_and_offers_retry() {
    let fixture = Fixture::new(
        vec![
            reply("one"),
            reply("two"),
            reply("three"),
            reply(SUMMARY),
            reply("four"),
        ],
        false,
    )
    .await;
    let f = &fixture;
    f.three_turns().await;
    let mut app = f.app();
    let events = async_stream::stream! {
        yield Ok(ctrl('k')); yield Ok(key(KeyCode::Char('c')));
        wait_summary_done(&f.provider, 4).await;
        frame_tick().await; frame_tick().await;
        f.turn("intervening committed turn").await;
        frame_tick().await; frame_tick().await;
        yield Ok(key(KeyCode::Enter)); frame_tick().await; frame_tick().await;
        yield Ok(ctrl('d'));
    };
    drive(f, &mut app, events, &f.handle).await;
    let text = render(&mut app, 80, 24);
    assert!(
        text.contains("stale_preview: head changed") && text.contains("new preview"),
        "{text}"
    );
    assert!(
        !f.handle
            .view()
            .await
            .unwrap()
            .snapshot
            .entries()
            .any(|(_, e)| matches!(e.entry.payload, EntryPayload::Compaction(_)))
    );
    f.handle.shutdown().await;
}

#[tokio::test]
async fn loop_not_eligible_names_reason_and_makes_zero_provider_calls() {
    let f = Fixture::new(vec![], false).await;
    let mut app = f.app();
    let events = async_stream::stream! {
        yield Ok(ctrl('k')); yield Ok(key(KeyCode::Char('c'))); frame_tick().await; yield Ok(ctrl('d'));
    };
    let (terminal, _) = drive(&f, &mut app, events, &f.handle).await;
    assert!(screen(&terminal).contains("compaction: empty"));
    assert!(f.provider.streams().is_empty());
    f.handle.shutdown().await;
}

#[tokio::test]
async fn loop_automatic_compaction_visible_and_refreshes_open_panel() {
    let text = "long source dialogue ".repeat(80);
    let fixture = Fixture::new(
        vec![reply(&text), reply(&text), reply(&text), reply(SUMMARY)],
        true,
    )
    .await;
    let f = &fixture;
    let mut app = f.app();
    let events = async_stream::stream! {
        yield Ok(ctrl('k')); frame_tick().await;
        f.three_turns().await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if f.handle.view().await.unwrap().snapshot.entries().any(|(_, e)| matches!(e.entry.payload, EntryPayload::Compaction(_))) { break; }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }).await.unwrap();
        frame_tick().await; yield Ok(ctrl('d'));
    };
    drive(f, &mut app, events, &f.handle).await;
    assert!(pages(&mut app).contains("Summary"));
    open(&mut app);
    let transcript = render(&mut app, 80, 30);
    assert!(
        transcript.contains("compaction previewed") && transcript.contains("compaction committed"),
        "{transcript}"
    );
    f.handle.shutdown().await;
}

struct SlowInspection {
    handle: SessionHandle,
    calls: Arc<AtomicUsize>,
}
impl Commands for SlowInspection {
    fn command(&self, command: Command) -> BoxFuture<'static, Result<Accepted, Rejection>> {
        Commands::command(&self.handle, command)
    }
    fn inspect_context(
        &self,
        head: Option<EntryId>,
        model_override: Option<ModelRef>,
    ) -> BoxFuture<'static, Result<ContextReport, InspectError>> {
        let handle = self.handle.clone();
        let calls = self.calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(60)).await;
            handle.inspect_context(head, model_override).await
        })
    }
}
#[tokio::test]
async fn slow_inspection_never_blocks_keys_resize_or_render_ticks() {
    let f = Fixture::new(vec![], false).await;
    let mut app = f.app();
    let calls = Arc::new(AtomicUsize::new(0));
    let slow = SlowInspection {
        handle: f.handle.clone(),
        calls: calls.clone(),
    };
    let events = async_stream::stream! {
        yield Ok(ctrl('k')); frame_tick().await;
        yield Ok(Event::Resize(80, 24)); yield Ok(ctrl('k'));
        yield Ok(Event::Paste("keys-still-work".into())); frame_tick().await;
    };
    let (terminal, stats) = drive(&f, &mut app, events, &slow).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(stats.ticks >= 3 && stats.frames >= 3);
    assert!(screen(&terminal).contains("keys-still-work"));
    f.handle.shutdown().await;
}

#[tokio::test]
async fn opaque_schema_payload_and_portability_sentinel_never_render_in_panel() {
    let f = Fixture::new(
        vec![Script::new([
            Step::Event(ProviderEvent::Started),
            Step::Event(ProviderEvent::ProviderState {
                block_index: 0,
                schema: SENTINEL.into(),
                payload: SENTINEL.as_bytes().to_vec(),
            }),
            Step::text(1, "safe display text"),
            Step::completed(FinishReason::Stop),
        ])],
        false,
    )
    .await;
    f.turn("safe question").await;
    let mut app = f.app();
    let events =
        async_stream::stream! { yield Ok(ctrl('k')); frame_tick().await; yield Ok(ctrl('d')); };
    drive(&f, &mut app, events, &f.handle).await;
    let all = pages(&mut app);
    assert!(!all.contains(SENTINEL), "opaque state leaked");
    assert!(all.contains("safe display text"));
    press(&mut app, KeyCode::Char('m'));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    app.on_context_report(Ok(f
        .handle
        .inspect_context(None, Some(model("other")))
        .await
        .unwrap()));
    assert!(!pages(&mut app).contains(SENTINEL));
    assert!(!render(&mut app, 20, 5).contains(SENTINEL));
    f.handle.shutdown().await;
}

#[tokio::test]
async fn loop_escape_during_generation_shows_progress_and_preserves_journal() {
    let f = Fixture::new(
        vec![
            reply("one"),
            reply("two"),
            reply("three"),
            Script::new([Step::Stall]),
        ],
        false,
    )
    .await;
    f.three_turns().await;
    let before = f.journal();
    let mut progress = f.app();
    f.reset(&mut progress).await;
    open(&mut progress);
    press(&mut progress, KeyCode::Char('c'));
    assert!(render(&mut progress, 80, 24).contains("Generating compaction preview"));
    let mut app = f.app();
    let events = async_stream::stream! {
        yield Ok(ctrl('k')); yield Ok(key(KeyCode::Char('c'))); frame_tick().await;
        yield Ok(key(KeyCode::Esc)); frame_tick().await; yield Ok(ctrl('d'));
    };
    let (terminal, _) = drive(&f, &mut app, events, &f.handle).await;
    assert!(screen(&terminal).contains("nothing committed"));
    assert_eq!(f.journal(), before);
    assert!(f.provider.streams().last().unwrap().dropped);
    f.handle.shutdown().await;
}

#[tokio::test]
async fn loop_budget_failure_preserves_reason_and_suggests_eligible_compaction() {
    for eligible in [false, true] {
        let f = Fixture::new(vec![reply("one"), reply("two"), reply("three")], false).await;
        if eligible {
            f.three_turns().await;
        }
        let calls = f.provider.streams().len();
        let before = f.journal();
        let mut app = f.app();
        let events = async_stream::stream! {
            yield Ok(Event::Paste("x".repeat(500_000))); yield Ok(key(KeyCode::Enter));
            frame_tick().await; frame_tick().await; yield Ok(ctrl('k')); frame_tick().await; yield Ok(ctrl('d'));
        };
        let (terminal, _) = drive(&f, &mut app, events, &f.handle).await;
        let text = screen(&terminal);
        assert!(text.contains("context_budget_exceeded"), "{text}");
        assert!(
            text.contains(if eligible {
                "preview compaction"
            } else {
                "empty"
            }),
            "{text}"
        );
        assert_eq!(f.provider.streams().len(), calls);
        assert_eq!(f.journal(), before);
        f.handle.shutdown().await;
    }
}

#[tokio::test]
async fn pure_unknown_overflow_flags_and_old_report_rejection() {
    let f = Fixture::new(vec![reply("new answer")], false).await;
    let old = f.handle.inspect_context(None, None).await.unwrap();
    f.turn("new question").await;
    let mut app = f.app();
    f.reset(&mut app).await;
    open(&mut app);
    app.on_context_report(Ok(old));
    assert!(render(&mut app, 80, 24).contains("Inspecting context"));
    let mut report = f.handle.inspect_context(None, None).await.unwrap();
    report.flags.overflow = true;
    report.flags.compaction_recommended = true;
    app.on_context_report(Ok(report));
    let text = pages(&mut app);
    assert!(text.contains("overflow=true") && text.contains("compaction_recommended=true"));
    let mut catalog = f.config.runtime.context.catalog.clone();
    catalog.insert(model("a-unknown"), ModelLimits::default());
    app.set_catalog(catalog);
    press(&mut app, KeyCode::Char('m'));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    app.on_context_report(Ok(f
        .handle
        .inspect_context(None, Some(model("a-unknown")))
        .await
        .unwrap()));
    let text = pages(&mut app);
    assert!(text.contains("Limit unknown") && text.contains("budget_unknown=true"));
    f.handle.shutdown().await;
}

#[tokio::test]
async fn inspect_preview_reads_live_proposal_and_stale_rejection_without_regeneration() {
    let fixture = Fixture::new(
        vec![
            reply("one"),
            reply("two"),
            reply("three"),
            reply(SUMMARY),
            reply("four"),
        ],
        false,
    )
    .await;
    let f = &fixture;
    f.three_turns().await;
    let (id, proposed) = preview(f).await;
    let generations = f.provider.streams().len();
    // Inspecting an existing proposal returns it and calls no provider.
    let live = runtime::Commands::inspect_preview(&f.handle, id.clone())
        .await
        .expect("live preview");
    assert_eq!(live, proposed);
    assert_eq!(f.provider.streams().len(), generations);
    // A committed turn invalidates it; the ID only reports stale.
    f.turn("invalidate").await;
    let after_turn = f.provider.streams();
    assert_eq!(after_turn.len(), generations + 1);
    assert_eq!(
        after_turn.last().unwrap().request.input.model,
        model("session")
    );
    assert!(matches!(
        runtime::Commands::inspect_preview(&f.handle, id).await,
        Err(Rejection::StalePreview { .. })
    ));
    assert_eq!(f.provider.streams().len(), generations + 1);
    f.handle.shutdown().await;
}
