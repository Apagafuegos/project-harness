//! Navigation acceptance: real journals, ScriptedProvider, the production loop.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures_util::Stream;
use harness::{
    tui::{
        Action, App,
        runtime::{self, Commands, Signal},
        runtime_nav::SessionCommands,
    },
    wiring::{self, SessionOptions},
};
use harness_core::{
    context::{ModelCapabilities, ModelInfo, ModelLimits},
    model::{
        ProviderEvent,
        scripted::{Script, ScriptedProvider, Step},
    },
    runtime::*,
    session::*,
};
use ratatui::{Terminal, backend::TestBackend};
use std::{io, sync::Arc, time::Duration};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}
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
fn opaque() -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, "answer A"),
        Step::Event(ProviderEvent::ProviderState {
            block_index: 1,
            schema: "native.test.v1".into(),
            payload: b"OPAQUE_SENTINEL_NEVER_SHOW".to_vec(),
        }),
        Step::completed(FinishReason::Stop),
    ])
}
struct Fixture {
    _temp: tempfile::TempDir,
    options: SessionOptions,
    provider: Arc<ScriptedProvider>,
}
impl Fixture {
    fn new(scripts: Vec<Script>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("work");
        std::fs::create_dir(&workspace).unwrap();
        let mut driver = DriverConfig::default();
        driver.runtime.cancel_grace = Duration::from_millis(20);
        for name in ["a", "b"] {
            driver.runtime.context.catalog.insert(
                model(name),
                ModelLimits {
                    context_tokens: Some(128_000),
                    max_output_tokens: Some(8192),
                },
            );
        }
        Self {
            options: SessionOptions {
                data_root: temp.path().join("state"),
                workspace,
                session: None,
                model: model("a"),
                explicit_model: false,
                explicit_provider: false,
                switch_policy: Portability::Reject,
                known_instances: vec!["mock".into()],
                summary_model: None,
                compaction_strategy: harness::wiring::creation_compaction_strategy(),
                explicit_strategy: false,
                inherit_remembered_grants: true,
                driver,
                sandbox: None,
            },
            provider: Arc::new(ScriptedProvider::new(scripts)),
            _temp: temp,
        }
    }
    async fn open(&self) -> wiring::WiredSession {
        wiring::open_session(&self.options, self.provider.clone())
            .await
            .unwrap()
    }
    async fn host(&self) -> SessionCommands {
        SessionCommands::new(
            self.open().await,
            self.options.clone(),
            self.provider.clone(),
        )
    }
    fn root(&self) -> DataRoot {
        DataRoot::open(&self.options.data_root, false).unwrap()
    }
}
async fn wait(host: &SessionCommands, condition: impl Fn(&RuntimeView) -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if condition(&host.handle().await.view().await.unwrap()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("view deadline");
}
async fn settle() {
    tokio::time::sleep(Duration::from_millis(60)).await;
}
async fn turn(host: &SessionCommands, text: &str) {
    host.command(Command::StartTurn {
        command_id: CommandId::new(),
        content: vec![ContentBlock::Text { text: text.into() }],
    })
    .await
    .unwrap();
    wait(host, |v| v.active_turn.is_none()).await;
}
fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
    t.draw(|f| app.render(f)).unwrap();
    t.backend()
        .buffer()
        .content
        .chunks(width as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
async fn reset(host: &SessionCommands) -> App {
    let mut app = host.app();
    app.on_subscription(SubscriptionItem::Reset(ClientView::from(
        host.handle().await.view().await.unwrap(),
    )));
    app
}
async fn drive(
    host: &SessionCommands,
    app: App,
    events: impl Stream<Item = io::Result<Event>>,
) -> App {
    drive_with(host, host.subscriptions(), app, events).await
}
async fn drive_with<C: Commands>(
    commands: &C,
    subscriptions: impl Stream<Item = SubscriptionItem>,
    mut app: App,
    events: impl Stream<Item = io::Result<Event>>,
) -> App {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        runtime::drive(
            &mut terminal,
            &mut app,
            events,
            subscriptions,
            futures_util::stream::pending::<Signal>(),
            commands,
            || {},
        ),
    )
    .await
    .expect("loop deadline")
    .unwrap();
    app
}

/// Counts completed catalog scans, so a test presses Enter only after the rows
/// exist. Timing alone would make the picker's first Enter a no-op race.
#[derive(Clone)]
struct CountedScans {
    host: SessionCommands,
    scans: Arc<std::sync::atomic::AtomicUsize>,
}
impl CountedScans {
    fn new(host: &SessionCommands) -> Self {
        Self {
            host: host.clone(),
            scans: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
    async fn wait_for(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.scans.load(std::sync::atomic::Ordering::SeqCst) < count {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("catalog scan deadline");
    }
}
impl Commands for CountedScans {
    fn command(
        &self,
        command: Command,
    ) -> futures_util::future::BoxFuture<'static, Result<Accepted, Rejection>> {
        self.host.command(command)
    }
    fn sequence(
        &self,
        commands: Vec<Command>,
    ) -> futures_util::future::BoxFuture<'static, Result<Accepted, Rejection>> {
        self.host.sequence(commands)
    }
    fn startup(&self) -> Option<Command> {
        Commands::startup(&self.host)
    }
    fn navigation(&self, action: Action) -> harness::tui::runtime_nav::NavigationFuture {
        let host = self.host.clone();
        let scans = self.scans.clone();
        Box::pin(async move {
            let scan = matches!(action, Action::ListSessions);
            let result = host.navigation(action).await;
            if scan {
                scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            result
        })
    }
}

#[tokio::test]
async fn read_only_approval_setting_reaches_every_app_the_shell_builds() {
    let f = Fixture::new(vec![reply("one")]);
    let host = f.host().await;
    assert!(!host.app().read_only_approval(), "default must stay off");
    assert!(
        host.clone()
            .read_only_approval(true)
            .app()
            .read_only_approval(),
        "the resolved flag/config value must reach the pure app"
    );
    host.shutdown().await;
}

/// Audit L06: the client that records a grant resumes it; `ask` refuses it.
#[tokio::test]
async fn a_recorded_grant_is_resumed_by_the_tui() {
    let f = Fixture::new(Vec::new());
    let root = Arc::new(DataRoot::open(&f.options.data_root, true).unwrap());
    let bytes = serde_json::to_vec(&serde_json::json!({
        "policy_revision": "tui-test",
        "enabled_tools": ["read", "write", "edit"],
        "workspace_read": "ask",
        "workspace_mutation": "ask",
        "process": "ask",
        "network": "deny",
        "protected_paths": [],
        "allowances": [{"tool": "write", "path": "."}]
    }))
    .unwrap();
    let configuration = root.put_artifact(&bytes).unwrap();
    let session = SessionId::new();
    let handle = SessionHandle::create(
        root,
        session.clone(),
        SessionHeader {
            workspace: f.options.workspace.to_string_lossy().into(),
            initial_model: f.options.model.clone(),
            policy_revision: "tui-test".into(),
            configuration,
        },
        f.provider.clone(),
        f.options.driver.clone(),
    )
    .await
    .unwrap();
    handle.shutdown().await;

    let mut options = f.options.clone();
    options.session = Some(session);
    let opened = wiring::open_session(&options, f.provider.clone())
        .await
        .expect("the TUI resumes the grants it recorded");
    let snapshot = opened.handle.view().await.unwrap().snapshot;
    let policy: serde_json::Value = serde_json::from_slice(
        &DataRoot::open(&options.data_root, false)
            .unwrap()
            .read_artifact(snapshot.configuration(), 1024 * 1024)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        policy["allowances"],
        serde_json::json!([{"tool": "write", "path": "."}])
    );
    opened.handle.shutdown().await;
}

#[tokio::test]
async fn picker_keys_and_render_status_refusals() {
    let f = Fixture::new(vec![reply("one")]);
    let host = f.host().await;
    turn(&host, "first title").await;
    let other = f.open().await;
    let other_id = other.session.clone();
    other.handle.shutdown().await;
    let mut entries = f.root().sessions().unwrap();
    let mut app = reset(&host).await;
    assert_eq!(app.on_key(ctrl('o')), vec![Action::ListSessions]);
    for (status, reason) in [
        (CatalogStatus::Busy, "busy"),
        (CatalogStatus::NeedsRepair, "harness journal repair"),
        (CatalogStatus::Unreadable, "unreadable"),
    ] {
        let entry = entries
            .iter_mut()
            .find(|e| e.session_id == other_id)
            .unwrap();
        entry.status = status;
        entry.title = Some("first title".into());
        app.on_sessions(vec![entry.clone()]);
        assert!(app.on_key(key(KeyCode::Enter)).is_empty());
        let screen = render(&mut app, 80, 24);
        assert!(screen.contains(reason), "{screen}");
        assert!(screen.contains("first title"));
    }
    let mut entry = entries
        .into_iter()
        .find(|e| e.session_id == other_id)
        .unwrap();
    entry.status = CatalogStatus::Ok;
    entry.workspace = Some("/different".into());
    app.on_sessions(vec![entry.clone()]);
    assert!(app.on_key(key(KeyCode::Enter)).is_empty());
    assert!(render(&mut app, 80, 24).contains("different workspace"));
    entry.workspace = Some(f.options.workspace.to_string_lossy().into());
    app.on_sessions(vec![entry]);
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        vec![Action::OpenSession(other_id)]
    );
    render(&mut app, 20, 5);
    host.shutdown().await;
}

#[tokio::test]
async fn opening_another_and_back_releases_old_writer_durably_and_new_session() {
    let fixture = Fixture::new(vec![reply("old answer"), reply("other answer")]);
    let f = &fixture;
    let owner = f.host().await;
    let host = &owner;
    let identity = host.session().await;
    let old = &identity;
    turn(host, "old prompt").await;
    let other = f.open().await;
    let target = other.session.clone();
    other.handle.shutdown().await;
    let endpoint = CountedScans::new(host);
    let waiter = endpoint.clone();
    let events = async_stream::stream! {
        yield Ok(Event::Key(ctrl('o'))); waiter.wait_for(1).await;
        yield Ok(Event::Key(key(KeyCode::Enter)));
        wait(host,|v| v.snapshot.session_id()==&target).await; settle().await;
        // A new exclusive writer can immediately acquire the previous journal.
        let writer=JournalStore::open(Arc::new(f.root()),old).unwrap(); drop(writer);
        yield Ok(Event::Paste("other prompt".into())); yield Ok(Event::Key(key(KeyCode::Enter)));
        wait(host,|v| v.snapshot.turns().len()==1 && v.active_turn.is_none()).await; settle().await;
        yield Ok(Event::Key(ctrl('o'))); waiter.wait_for(2).await;
        yield Ok(Event::Key(key(KeyCode::Enter)));
        wait(host,|v| v.snapshot.session_id()==old).await; settle().await;
        yield Ok(Event::Key(ctrl('d')));
    };
    let mut app = drive_with(&endpoint, host.subscriptions(), host.app(), events).await;
    assert!(render(&mut app, 80, 24).contains("old answer"));
    assert!(!render(&mut app, 80, 24).contains("other answer"));
    let endpoint = CountedScans::new(host);
    let waiter = endpoint.clone();
    let events = async_stream::stream! { yield Ok(Event::Key(ctrl('o'))); waiter.wait_for(1).await; yield Ok(Event::Key(key(KeyCode::Char('n')))); wait(host,|v| v.snapshot.session_id()!=old).await; settle().await; yield Ok(Event::Key(ctrl('d'))); };
    let app = drive_with(&endpoint, host.subscriptions(), app, events).await;
    assert_eq!(app.view().unwrap().snapshot.turns().len(), 0);
    assert_eq!(f.root().sessions().unwrap().len(), 3);
    host.shutdown().await;
}

#[tokio::test]
async fn busy_active_turn_handover_is_refused_without_shutdown() {
    let f = Fixture::new(vec![Script::new([
        Step::Event(ProviderEvent::Started),
        Step::Stall,
    ])]);
    let host = f.host().await;
    let id = host.session().await;
    host.command(Command::StartTurn {
        command_id: CommandId::new(),
        content: vec![ContentBlock::Text {
            text: "running".into(),
        }],
    })
    .await
    .unwrap();
    wait(&host, |v| v.partial.is_some()).await;
    let events = async_stream::stream! { yield Ok(Event::Key(ctrl('o'))); settle().await; yield Ok(Event::Key(key(KeyCode::Char('n')))); settle().await; yield Ok(Event::Key(key(KeyCode::Esc))); yield Ok(Event::Key(ctrl('d'))); };
    let mut app = drive(&host, host.app(), events).await;
    assert!(render(&mut app, 80, 24).contains("command rejected: busy"));
    assert_eq!(host.session().await, id);
    assert_eq!(f.root().sessions().unwrap().len(), 1);
    host.shutdown().await;
}

#[tokio::test]
async fn branch_selection_reprojects_and_edit_resubmit_preserves_original_sibling() {
    let f = Fixture::new(vec![
        reply("first answer"),
        reply("second answer"),
        reply("sibling answer"),
    ]);
    let owner = f.host().await;
    let host = &owner;
    turn(host, "first prompt").await;
    turn(host, "second prompt").await;
    let before = host.handle().await.view().await.unwrap().snapshot;
    let original=before.entries().find(|(_,e)| matches!(&e.entry.payload,EntryPayload::UserMessage(m) if matches!(&m.content[0],ContentBlock::Text{text} if text=="first prompt"))).unwrap().1.entry.clone();
    let events = async_stream::stream! { yield Ok(Event::Key(ctrl('t'))); settle().await; yield Ok(Event::Key(key(KeyCode::Enter))); wait(host,|v|v.snapshot.head()!=before.head()).await; settle().await; yield Ok(Event::Key(ctrl('d'))); };
    let mut app = drive(host, host.app(), events).await;
    let screen = render(&mut app, 80, 24);
    assert!(screen.contains("first answer"));
    assert!(!screen.contains("second answer"));
    // Edit the first message while the head is the other branch's head: the
    // parent (the root) is not the selected head, so branching needs SelectHead.
    let events = async_stream::stream! { yield Ok(Event::Key(ctrl('t'))); yield Ok(Event::Key(key(KeyCode::Char('e')))); yield Ok(Event::Paste(" edited".into())); yield Ok(Event::Key(key(KeyCode::Enter))); wait(host,|v|v.snapshot.turns().len()==3 && v.active_turn.is_none()).await; settle().await; yield Ok(Event::Key(ctrl('d'))); };
    let mut app = drive(host, app, events).await;
    let s = &app.view().unwrap().snapshot;
    assert_eq!(s.entry(&original.id).unwrap().entry, original);
    let edited=s.entries().find(|(_,e)|matches!(&e.entry.payload,EntryPayload::UserMessage(m) if matches!(&m.content[0],ContentBlock::Text{text} if text=="first prompt edited"))).unwrap().1;
    assert_eq!(edited.entry.parent_id, original.parent_id);
    assert_ne!(edited.entry.id, original.id);
    assert_eq!(s.turns().len(), 3);
    app.on_key(ctrl('t'));
    let screen = render(&mut app, 80, 24);
    assert!(screen.contains("first prompt edited"));
    assert!(screen.contains("first prompt "));
    assert!(screen.contains("mock/a"));
    assert!(screen.contains('*'));
    host.shutdown().await;
}

#[tokio::test]
async fn select_head_during_active_turn_shows_core_busy_reason() {
    let f = Fixture::new(vec![
        reply("done"),
        Script::new([Step::Event(ProviderEvent::Started), Step::Stall]),
    ]);
    let owner = f.host().await;
    let host = &owner;
    turn(host, "previous").await;
    host.command(Command::StartTurn {
        command_id: CommandId::new(),
        content: vec![ContentBlock::Text {
            text: "running".into(),
        }],
    })
    .await
    .unwrap();
    wait(host, |v| v.partial.is_some()).await;
    let head = host
        .handle()
        .await
        .view()
        .await
        .unwrap()
        .snapshot
        .head()
        .cloned();
    let events = async_stream::stream! { yield Ok(Event::Key(ctrl('t'))); yield Ok(Event::Key(key(KeyCode::Enter))); settle().await; assert_eq!(host.handle().await.view().await.unwrap().snapshot.head(),head.as_ref()); yield Ok(Event::Key(key(KeyCode::Esc))); yield Ok(Event::Key(ctrl('d'))); };
    let mut app = drive(host, host.app(), events).await;
    assert!(render(&mut app, 80, 24).contains("command rejected: busy"));
    host.shutdown().await;
}

#[tokio::test]
async fn switch_reject_first_report_then_explicit_omit_has_no_foreign_state() {
    let f = Fixture::new(vec![opaque(), reply("answer B")]);
    let owner = f.host().await;
    let host = &owner;
    turn(host, "seed").await;
    let before = host
        .handle()
        .await
        .view()
        .await
        .unwrap()
        .snapshot
        .sequence();
    let mut app = reset(host).await;
    app.on_key(ctrl('l'));
    let models = render(&mut app, 80, 24);
    assert!(models.contains("context Some(128000)"));
    assert!(models.contains("tools=true"));
    app.on_key(key(KeyCode::Down));
    let actions = app.on_key(key(KeyCode::Enter));
    assert!(matches!(
        &actions[..],
        [Action::Command(Command::SwitchModel {
            policy: Portability::Reject,
            ..
        })]
    ));
    // Re-enter using the actual loop; the assertion above guards first-attempt policy.
    app = reset(host).await;
    let events = async_stream::stream! {
        yield Ok(Event::Key(ctrl('l'))); yield Ok(Event::Key(key(KeyCode::Down))); yield Ok(Event::Key(key(KeyCode::Enter))); settle().await;
        assert_eq!(host.handle().await.view().await.unwrap().snapshot.sequence(),before);
        // Capture the refusal before confirming in a second drive.
        yield Ok(Event::Key(ctrl('d')));
    };
    // Ctrl-D must retain its quit semantics inside navigation dialogs.
    let mut app = drive(host, app, events).await;
    let report = render(&mut app, 80, 24);
    assert!(report.contains("native.test.v1"), "{report}");
    assert!(report.contains("bytes=26"));
    assert!(!report.contains("OPAQUE_SENTINEL"));
    let events = async_stream::stream! { yield Ok(Event::Key(key(KeyCode::Char('o')))); wait(host,|v|v.snapshot.model_at(v.snapshot.head()).unwrap()==&model("b")).await; settle().await; yield Ok(Event::Paste("next".into())); yield Ok(Event::Key(key(KeyCode::Enter))); wait(host,|v|v.snapshot.turns().len()==2 && v.active_turn.is_none()).await; settle().await; yield Ok(Event::Key(ctrl('d'))); };
    let mut app = drive(host, app, events).await;
    assert!(render(&mut app, 80, 24).contains("mock/b"));
    let streams = f.provider.streams();
    let request = &streams.last().unwrap().request;
    assert_eq!(request.input.model, model("b"));
    assert!(request.input.continuation.is_none());
    let request_json = serde_json::to_string(&request.input.items).unwrap();
    assert!(!request_json.contains("native.test.v1"));
    host.shutdown().await;
}

#[tokio::test]
async fn required_incompatibility_is_not_omittable() {
    let mut f = Fixture::new(vec![reply("image answer"), reply("converted answer")]);
    let mut source = ModelInfo::from(ModelLimits {
        context_tokens: Some(128_000),
        max_output_tokens: None,
    });
    source
        .capabilities
        .input_media
        .insert(harness_core::context::InputMediaKind::Image);
    f.options
        .driver
        .runtime
        .context
        .catalog
        .insert(model("a"), source);
    f.options.driver.runtime.context.catalog.insert(
        model("b"),
        ModelInfo {
            limits: ModelLimits {
                context_tokens: Some(128_000),
                max_output_tokens: None,
            },
            capabilities: ModelCapabilities::default(),
        },
    );
    let host = f.host().await;
    let image = host
        .handle()
        .await
        .put_artifact(b"image fixture".to_vec())
        .await
        .unwrap();
    host.command(Command::StartTurn {
        command_id: CommandId::new(),
        content: vec![ContentBlock::Image {
            mime_type: "image/png".into(),
            artifact: image,
        }],
    })
    .await
    .unwrap();
    wait(&host, |v| v.active_turn.is_none()).await;
    let before = host
        .handle()
        .await
        .view()
        .await
        .unwrap()
        .snapshot
        .sequence();
    let events = async_stream::stream! { yield Ok(Event::Key(ctrl('l'))); yield Ok(Event::Key(key(KeyCode::Down))); yield Ok(Event::Key(key(KeyCode::Enter))); settle().await; yield Ok(Event::Key(key(KeyCode::Char('o')))); settle().await; yield Ok(Event::Key(ctrl('d'))); };
    let mut app = drive(&host, host.app(), events).await;
    let screen = render(&mut app, 80, 24);
    assert!(screen.contains("REQUIRED"));
    assert!(screen.contains("cannot omit"));
    assert!(screen.contains("c convert"));
    assert!(screen.contains("convert: 1 items"));
    assert_eq!(
        host.handle()
            .await
            .view()
            .await
            .unwrap()
            .snapshot
            .sequence(),
        before
    );
    let host_ref = &host;
    let events = async_stream::stream! {
        yield Ok(Event::Key(key(KeyCode::Char('c'))));
        wait(host_ref, |v| v.snapshot.model_at(v.snapshot.head()).unwrap() == &model("b")).await;
        settle().await;
        yield Ok(Event::Paste("next".into()));
        yield Ok(Event::Key(key(KeyCode::Enter)));
        wait(host_ref, |v| v.snapshot.turns().len() == 2 && v.active_turn.is_none()).await;
        yield Ok(Event::Key(ctrl('d')));
    };
    drive(&host, app, events).await;
    let streams = f.provider.streams();
    let request = &streams.last().unwrap().request;
    assert_eq!(request.input.model, model("b"));
    assert!(request.artifacts.references().is_empty());
    let text = serde_json::to_string(&request.input.items).unwrap();
    assert!(text.contains("image omitted for this model: image/png, 13 bytes, sha256"));
    assert!(!text.contains("\"kind\":\"image\""));
    host.shutdown().await;
}

#[tokio::test]
async fn dialogs_render_and_map_keys_without_the_loop() {
    let f = Fixture::new(vec![reply("first answer"), reply("second answer")]);
    let host = f.host().await;
    turn(&host, "first prompt").await;
    turn(&host, "second prompt").await;
    let view = host.handle().await.view().await.unwrap();
    let second_head = view.snapshot.head().cloned();
    let fresh = || {
        let mut app = host.app();
        app.on_subscription(SubscriptionItem::Reset(ClientView::from(view.clone())));
        app
    };

    // Branch tree: depths, the current marker, models, and Enter -> SelectHead.
    let mut app = fresh();
    app.on_key(ctrl('t'));
    let tree = render(&mut app, 80, 24);
    assert!(tree.contains("branches"), "{tree}");
    assert!(tree.contains("first prompt") && tree.contains("second prompt"));
    assert!(tree.contains("mock/a") && tree.contains('*'));
    let actions = app.on_key(key(KeyCode::Enter));
    assert_eq!(actions.len(), 1, "{actions:?}");
    let Action::Command(Command::SelectHead { head }) = &actions[0] else {
        panic!("unexpected action: {actions:?}")
    };
    assert!(head.as_ref() != second_head.as_ref());

    // Selecting the branch that is already head is a silent no-op, not a
    // `head_unchanged` rejection.
    let mut app = fresh();
    app.on_key(ctrl('t'));
    app.on_key(key(KeyCode::Down));
    assert!(app.on_key(key(KeyCode::Enter)).is_empty());
    assert!(render(&mut app, 80, 24).contains("branches"));

    // An open dialog swallows paste; after `e` closes it, paste is ordinary
    // prompt editing again, and the message text is already prefilled.
    let mut app = fresh();
    app.on_key(ctrl('t'));
    app.on_paste(" ignored");
    assert!(!render(&mut app, 80, 24).contains("ignored"));
    app.on_key(key(KeyCode::Char('e')));
    let prefilled = render(&mut app, 80, 24);
    assert!(prefilled.contains("second prompt"), "{prefilled}");
    app.on_paste(" edited");
    let edited = render(&mut app, 80, 24);
    // The cursor block sits at the prefill's end, so the paste follows it.
    assert!(edited.contains("edited"), "{edited}");
    assert!(!edited.contains("ignored"));

    // Model list: sourced limits and capabilities, Enter -> Reject first.
    let mut app = fresh();
    app.on_key(ctrl('l'));
    let models = render(&mut app, 80, 24);
    assert!(models.contains("models"), "{models}");
    assert!(models.contains("mock/a") && models.contains("mock/b"));
    assert!(models.contains("context Some(128000) output Some(8192)"));
    assert!(models.contains("tools=true"));
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        vec![Action::Command(Command::SwitchModel {
            model: model("a"),
            policy: Portability::Reject
        })]
    );

    // Session picker: rows render, a busy row is refused in place, and the
    // session this client already owns is labelled as current.
    let mut entry = f.root().sessions().unwrap().remove(0);
    entry.title = Some("second prompt".into());
    entry.status = CatalogStatus::Busy;
    let mut app = fresh();
    assert_eq!(app.on_key(ctrl('o')), vec![Action::ListSessions]);
    app.on_sessions(vec![entry.clone()]);
    let picker = render(&mut app, 80, 24);
    assert!(picker.contains("sessions"), "{picker}");
    assert!(picker.contains("current"), "{picker}");
    // The live view supplies the title of the session this client owns.
    assert!(picker.contains("first prompt"), "{picker}");
    assert!(app.on_key(key(KeyCode::Enter)).is_empty());
    assert!(render(&mut app, 80, 24).contains("already open"));
    let mut foreign = entry;
    foreign.session_id = SessionId::new();
    let mut app = fresh();
    app.on_key(ctrl('o'));
    app.on_sessions(vec![foreign]);
    let picker = render(&mut app, 80, 24);
    assert!(picker.contains("Busy") && picker.contains("session_locked"));
    assert!(picker.contains("second prompt"), "{picker}");
    assert!(app.on_key(key(KeyCode::Enter)).is_empty());
    assert!(render(&mut app, 80, 24).contains("busy: session_locked"));

    // Tiny areas never panic for any dialog.
    render(&mut app, 20, 5);
    render(&mut app, 1, 1);
    host.shutdown().await;
}

#[path = "support/navigation_cases.rs"]
mod cases;
