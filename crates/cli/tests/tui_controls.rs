//! Context-control acceptance for the terminal client: pure `App` buffers for
//! the tree markers and the dimmed excluded turns, and the production loop with
//! real JSONL storage and a scripted provider for the controls themselves.
//!
//! Nothing here opens a network stream: every inference is a `ScriptedProvider`
//! script.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures_util::Stream;
use harness::tui::{
    Action, App, SessionInfo,
    runtime::{self, Signal},
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
use ratatui::{Terminal, backend::TestBackend, style::Modifier};
use serde_json::json;
use std::{io, sync::Arc, time::Duration};

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
fn tree(app: &mut App) {
    app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
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
    async fn new(scripts: Vec<Script>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let configuration = root
            .put_artifact(
                &serde_json::to_vec(&json!({
                    "instructions": [{"level":"system", "text":"Controls fixture instructions"}],
                    "compaction": {"summary_model": model("summary"), "automatic": false}
                }))
                .unwrap(),
            )
            .unwrap();
        let mut config = DriverConfig::default();
        // A provider that ignores cancellation must not stall a durable quit.
        config.runtime.cancel_grace = Duration::from_millis(20);
        for name in ["session", "summary"] {
            config.runtime.context.catalog.insert(
                model(name),
                ModelLimits {
                    context_tokens: Some(128_000),
                    max_output_tokens: None,
                },
            );
        }
        let provider = Arc::new(ScriptedProvider::new(scripts));
        let session = SessionId::new();
        let handle = SessionHandle::create(
            root.clone(),
            session.clone(),
            SessionHeader {
                workspace: workspace.to_str().unwrap().into(),
                initial_model: model("session"),
                policy_revision: "controls".into(),
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
    /// Every `context_control` in the journal, in append order.
    fn journal_controls(&self) -> Vec<(String, String)> {
        let text = String::from_utf8(self.journal()).unwrap();
        let mut controls = Vec::new();
        for line in text.lines() {
            let record: serde_json::Value = serde_json::from_str(line).unwrap();
            let entries = record["record"]["payload"]["entries"]
                .as_array()
                .into_iter()
                .flatten();
            for entry in entries {
                let payload = &entry["payload"];
                if payload["kind"] == "context_control" {
                    controls.push((
                        payload["turn_id"].as_str().unwrap().to_owned(),
                        payload["mode"].as_str().unwrap().to_owned(),
                    ));
                }
            }
        }
        controls
    }
    async fn reset(&self, app: &mut App) {
        app.on_subscription(SubscriptionItem::Reset(ClientView::from(
            self.handle.view().await.unwrap(),
        )));
    }
    async fn fresh(&self) -> App {
        let mut app = self.app();
        self.reset(&mut app).await;
        app
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
    /// The known turns of the branch, in journal order.
    async fn turns(&self) -> Vec<TurnId> {
        self.handle
            .view()
            .await
            .unwrap()
            .snapshot
            .turns()
            .map(|(id, _)| id.clone())
            .collect()
    }
    async fn control(&self, turn: &TurnId, mode: ContextMode) {
        self.handle
            .command(Command::SetContextControl {
                turn_id: turn.clone(),
                mode,
            })
            .await
            .unwrap();
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
fn control_entries(view: &RuntimeView) -> Vec<(TurnId, ContextMode)> {
    view.snapshot
        .entries()
        .filter_map(|(_, stored)| match &stored.entry.payload {
            EntryPayload::ContextControl(control) => Some((control.turn_id.clone(), control.mode)),
            _ => None,
        })
        .collect()
}
/// Wait until the journal's effective controls satisfy the predicate.
async fn wait_controls(f: &Fixture, count: usize) {
    wait(&f.handle, |v| control_entries(v).len() == count).await;
}
fn subscriptions(handle: &SessionHandle) -> impl Stream<Item = SubscriptionItem> + use<> {
    let mut sub = handle.subscribe();
    async_stream::stream! { while let Some(item) = sub.next().await { yield item; } }
}
async fn drive(
    f: &Fixture,
    app: &mut App,
    events: impl Stream<Item = io::Result<Event>>,
) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        runtime::drive(
            &mut terminal,
            app,
            events,
            subscriptions(&f.handle),
            futures_util::stream::pending::<Signal>(),
            &f.handle,
            || {},
        ),
    )
    .await
    .expect("loop deadline")
    .unwrap();
    terminal
}
fn screen(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
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
/// Every rendered row with the text it shows and whether all of its visible
/// cells are dimmed, so a dimmed-but-readable turn can be asserted directly.
fn rows_with_dim(app: &mut App, width: u16, height: u16) -> Vec<(String, bool)> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            let mut text = String::new();
            let mut dim = true;
            for x in 0..buffer.area.width {
                let cell = buffer.cell((x, y)).expect("in-area cell");
                text.push_str(cell.symbol());
                if !cell.symbol().trim().is_empty() {
                    dim &= cell.modifier.contains(Modifier::DIM);
                }
            }
            (text, dim)
        })
        .collect()
}
/// Commit one compaction and return the entry ids it covers.
async fn commit_compaction(f: &Fixture) -> Vec<EntryId> {
    let Accepted::CompactionPreviewed {
        preview_id,
        preview,
    } = f.handle.command(Command::PreviewCompaction).await.unwrap()
    else {
        panic!("preview expected")
    };
    let covered = match &preview.entry.payload {
        EntryPayload::Compaction(summary) => summary.covered_entry_ids.clone(),
        _ => panic!("compaction payload expected"),
    };
    match f
        .handle
        .command(Command::CommitCompaction { preview_id })
        .await
        .unwrap()
    {
        Accepted::CompactionCommitted { .. } => covered,
        _ => panic!("commit expected"),
    }
}

// ----- pure markers, dimming and key mapping --------------------------------

#[tokio::test]
async fn tree_shows_each_turns_effective_mode_at_80x24_and_20x5() {
    let fixture = Fixture::new(vec![reply("first answer"), reply("second answer")]).await;
    let f = &fixture;
    f.turn("first question").await;
    f.turn("second question").await;
    let turns = f.turns().await;
    f.control(&turns[0], ContextMode::Pinned).await;
    f.control(&turns[1], ContextMode::Excluded).await;
    let mut app = f.fresh().await;
    tree(&mut app);
    let wide = render(&mut app, 80, 24);
    println!("TREE 80x24\n{wide}");
    let pinned = wide
        .lines()
        .find(|line| line.contains("first question"))
        .expect("pinned row");
    let excluded = wide
        .lines()
        .find(|line| line.contains("second question"))
        .expect("excluded row");
    assert!(
        pinned.contains("[pin]") && !pinned.contains("[excl]"),
        "{wide}"
    );
    assert!(
        excluded.contains("[excl]") && !excluded.contains("[pin]"),
        "{wide}"
    );
    assert!(
        wide.contains("p pin") && wide.contains("x exclude"),
        "{wide}"
    );
    // The marker sits before the title, so it survives a 20×5 terminal.
    let narrow = render(&mut app, 20, 5);
    println!("TREE 20x5\n{narrow}");
    assert!(narrow.contains("[pin]"), "{narrow}");
    render(&mut app, 1, 1);
    f.handle.shutdown().await;
}

#[tokio::test]
async fn excluded_turn_is_tagged_and_dimmed_but_still_readable() {
    let fixture = Fixture::new(vec![reply("first answer"), reply("second answer")]).await;
    let f = &fixture;
    f.turn("first question").await;
    f.turn("second question").await;
    let turns = f.turns().await;
    f.control(&turns[1], ContextMode::Excluded).await;
    let mut app = f.fresh().await;
    let rows = rows_with_dim(&mut app, 80, 40);
    let rendered = rows
        .iter()
        .map(|(text, _)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    println!("TRANSCRIPT 80x40\n{rendered}");
    assert!(
        rendered.contains("excluded turn (kept in history; never sent to the model)"),
        "{rendered}"
    );
    // Still readable: the excluded turn's text is on screen, not hidden.
    assert!(rendered.contains("second question") && rendered.contains("second answer"));
    let row = |needle: &str| {
        rows.iter()
            .find(|(text, _)| text.contains(needle))
            .unwrap_or_else(|| panic!("missing row {needle}: {rendered}"))
    };
    assert!(
        !row("first question").1,
        "the untouched turn must not be dimmed: {rendered}"
    );
    for needle in ["second question", "second answer", "excluded turn"] {
        assert!(row(needle).1, "{needle} is not dimmed: {rendered}");
    }
    // The durable one-line markers from m6-01 are unchanged.
    assert!(rendered.contains("Excluded turn"), "{rendered}");
    f.handle.shutdown().await;
}

#[tokio::test]
async fn tree_keys_toggle_each_turns_effective_mode() {
    let fixture = Fixture::new(vec![reply("first answer"), reply("second answer")]).await;
    let f = &fixture;
    f.turn("first question").await;
    f.turn("second question").await;
    let turns = f.turns().await;
    let turn = turns[0].clone();

    // Outside the tree, `p` and `x` stay ordinary prompt characters.
    let mut app = f.fresh().await;
    assert!(press(&mut app, KeyCode::Char('p')).is_empty());
    assert!(press(&mut app, KeyCode::Char('x')).is_empty());
    assert!(render(&mut app, 80, 24).contains("> px"));

    // `p` on default pins, `p` on pinned resets, `x` on pinned excludes and
    // `x` on excluded resets: one control command each, following the mode.
    for (pressed, expected) in [
        ('p', ContextMode::Pinned),
        ('p', ContextMode::Default),
        ('p', ContextMode::Pinned),
        ('x', ContextMode::Excluded),
        ('x', ContextMode::Default),
    ] {
        let mut app = f.fresh().await;
        tree(&mut app);
        assert_eq!(
            press(&mut app, KeyCode::Char(pressed)),
            vec![Action::Command(Command::SetContextControl {
                turn_id: turn.clone(),
                mode: expected,
            })],
            "key {pressed}"
        );
        f.control(&turn, expected).await;
    }
    assert_eq!(
        f.journal_controls(),
        [
            ("pinned".to_owned()),
            ("default".to_owned()),
            ("pinned".to_owned()),
            ("excluded".to_owned()),
            ("default".to_owned())
        ]
        .map(|mode| (turn.to_string(), mode))
    );
    // Reset leaves no effective mode, so the tree shows no marker again.
    let mut app = f.fresh().await;
    tree(&mut app);
    let tree_rows = render(&mut app, 80, 24);
    assert!(
        !tree_rows.contains("[pin]") && !tree_rows.contains("[excl]"),
        "{tree_rows}"
    );
    f.handle.shutdown().await;
}

// ----- the production loop ---------------------------------------------------

#[tokio::test]
async fn loop_pin_journals_the_control_and_the_inspector_shows_pinned() {
    let fixture = Fixture::new(vec![reply("first answer"), reply("second answer")]).await;
    let f = &fixture;
    f.turn("first question").await;
    f.turn("second question").await;
    let turns = f.turns().await;
    let mut app = f.app();
    let events = async_stream::stream! {
        // Keep the inspector open while the tree sets the control: its report
        // identity changes with the head the control entry advances.
        yield Ok(ctrl('k'));
        frame_tick().await;
        yield Ok(ctrl('t'));
        frame_tick().await;
        yield Ok(key(KeyCode::Down));
        yield Ok(key(KeyCode::Char('p')));
        wait_controls(f, 1).await;
        frame_tick().await;
        yield Ok(key(KeyCode::Esc));
        frame_tick().await;
        frame_tick().await;
        yield Ok(ctrl('d'));
    };
    let terminal = drive(f, &mut app, events).await;
    assert_eq!(
        f.journal_controls(),
        vec![(turns[1].to_string(), "pinned".to_owned())]
    );
    let panel = screen(&terminal);
    println!("INSPECTOR 80x24\n{panel}");
    assert!(
        panel.contains("Controls (effective on this branch)"),
        "{panel}"
    );
    assert!(panel.contains("pinned"), "{panel}");
    assert!(panel.contains("second question"), "{panel}");
    f.handle.shutdown().await;
}

#[tokio::test]
async fn loop_exclude_shows_user_excluded_and_the_request_omits_the_turn() {
    let fixture = Fixture::new(vec![
        reply("first answer"),
        reply("second answer"),
        reply("third answer"),
    ])
    .await;
    let f = &fixture;
    f.turn("first question").await;
    f.turn("second question").await;
    let turns = f.turns().await;
    let mut app = f.app();
    let events = async_stream::stream! {
        yield Ok(ctrl('t'));
        yield Ok(key(KeyCode::Char('x')));
        wait_controls(f, 1).await;
        frame_tick().await;
        yield Ok(key(KeyCode::Esc));
        frame_tick().await;
        yield Ok(Event::Paste("third question".into()));
        yield Ok(key(KeyCode::Enter));
        wait(&f.handle, |v| v.snapshot.turns().len() == 3 && v.active_turn.is_none()).await;
        frame_tick().await;
        yield Ok(ctrl('k'));
        frame_tick().await;
        yield Ok(ctrl('d'));
    };
    let terminal = drive(f, &mut app, events).await;
    assert_eq!(
        f.journal_controls(),
        vec![(turns[0].to_string(), "excluded".to_owned())]
    );
    let panel = screen(&terminal);
    println!("INSPECTOR 80x24\n{panel}");
    assert!(panel.contains("user_excluded"), "{panel}");
    // The excluded turn is never sent to a model, including the next request.
    let streams = f.provider.streams();
    assert_eq!(streams.len(), 3);
    let items = serde_json::to_string(&streams.last().unwrap().request.input.items).unwrap();
    assert!(!items.contains("first question"), "{items}");
    assert!(!items.contains("first answer"), "{items}");
    assert!(items.contains("second question"), "{items}");
    assert!(items.contains("third question"), "{items}");
    f.handle.shutdown().await;
}

#[tokio::test]
async fn loop_reset_returns_the_turn_to_default() {
    let fixture = Fixture::new(vec![reply("first answer")]).await;
    let f = &fixture;
    f.turn("first question").await;
    let turns = f.turns().await;
    let mut app = f.app();
    let events = async_stream::stream! {
        yield Ok(ctrl('t'));
        yield Ok(key(KeyCode::Char('p')));
        wait_controls(f, 1).await;
        frame_tick().await;
        yield Ok(key(KeyCode::Char('p')));
        wait_controls(f, 2).await;
        frame_tick().await;
        yield Ok(key(KeyCode::Esc));
        frame_tick().await;
        yield Ok(ctrl('k'));
        frame_tick().await;
        yield Ok(ctrl('d'));
    };
    let terminal = drive(f, &mut app, events).await;
    // Undo is another control entry; nothing was rewritten or removed.
    assert_eq!(
        f.journal_controls(),
        vec![
            (turns[0].to_string(), "pinned".to_owned()),
            (turns[0].to_string(), "default".to_owned())
        ]
    );
    let panel = screen(&terminal);
    println!("INSPECTOR 80x24\n{panel}");
    assert!(
        panel.contains("Controls (effective on this branch)"),
        "{panel}"
    );
    assert!(!panel.contains("pinned"), "{panel}");
    assert!(!panel.contains("user_excluded"), "{panel}");
    f.handle.shutdown().await;
}

#[tokio::test]
async fn loop_covered_turn_refusal_is_shown_verbatim_and_writes_nothing() {
    let fixture = Fixture::new(vec![
        reply("one"),
        reply("two"),
        reply("three"),
        reply(SUMMARY),
    ])
    .await;
    let f = &fixture;
    f.three_turns().await;
    let covered = commit_compaction(f).await;
    assert!(!covered.is_empty(), "the fixture must cover a turn");
    let turns = f.turns().await;
    let first = &turns[0];
    let snapshot = f.handle.view().await.unwrap().snapshot;
    assert!(
        snapshot.entries().any(
            |(id, stored)| stored.entry.turn_id.as_ref() == Some(first) && covered.contains(id)
        ),
        "the first turn must be covered by the committed summary"
    );
    let before = f.journal();
    let mut app = f.app();
    let events = async_stream::stream! {
        yield Ok(ctrl('t'));
        yield Ok(key(KeyCode::Char('p')));
        frame_tick().await;
        frame_tick().await;
        yield Ok(ctrl('d'));
    };
    let terminal = drive(f, &mut app, events).await;
    let banner = screen(&terminal);
    println!("REFUSAL 80x24\n{banner}");
    assert!(banner.contains("context_control_covered"), "{banner}");
    assert_eq!(f.journal(), before);
    assert!(f.journal_controls().is_empty());
    f.handle.shutdown().await;
}

#[tokio::test]
async fn loop_refusal_on_the_active_turn_sends_nothing() {
    let fixture = Fixture::new(vec![
        reply("done"),
        Script::new([Step::Event(ProviderEvent::Started), Step::Stall]),
    ])
    .await;
    let f = &fixture;
    f.turn("previous question").await;
    f.handle
        .command(Command::StartTurn {
            command_id: CommandId::new(),
            content: vec![ContentBlock::Text {
                text: "running question".into(),
            }],
        })
        .await
        .unwrap();
    wait(&f.handle, |v| v.partial.is_some()).await;
    let mut app = f.app();
    let events = async_stream::stream! {
        yield Ok(ctrl('t'));
        frame_tick().await;
        // The second row is the unfinished, active turn.
        yield Ok(key(KeyCode::Down));
        yield Ok(key(KeyCode::Char('p')));
        frame_tick().await;
        yield Ok(ctrl('d'));
    };
    let terminal = drive(f, &mut app, events).await;
    let banner = screen(&terminal);
    println!("ACTIVE REFUSAL 80x24\n{banner}");
    assert!(banner.contains("command rejected: busy"), "{banner}");
    assert!(f.journal_controls().is_empty());
    f.handle.shutdown().await;
}

#[tokio::test]
async fn loop_pinned_turn_becomes_excluded_with_one_control() {
    let fixture = Fixture::new(vec![reply("first answer")]).await;
    let f = &fixture;
    f.turn("first question").await;
    let turns = f.turns().await;
    let mut app = f.app();
    let events = async_stream::stream! {
        yield Ok(ctrl('t'));
        yield Ok(key(KeyCode::Char('p')));
        wait_controls(f, 1).await;
        frame_tick().await;
        yield Ok(key(KeyCode::Char('x')));
        wait_controls(f, 2).await;
        frame_tick().await;
        yield Ok(key(KeyCode::Esc));
        frame_tick().await;
        yield Ok(ctrl('d'));
    };
    let terminal = drive(f, &mut app, events).await;
    assert_eq!(
        f.journal_controls(),
        vec![
            (turns[0].to_string(), "pinned".to_owned()),
            (turns[0].to_string(), "excluded".to_owned())
        ]
    );
    // Both controls stay in the journal; the last one is the effective mode.
    let entries = control_entries(&f.handle.view().await.unwrap());
    assert_eq!(
        entries.last().cloned(),
        Some((turns[0].clone(), ContextMode::Excluded))
    );
    let transcript = screen(&terminal);
    assert!(
        transcript.contains("excluded turn (kept in history; never sent to the model)"),
        "{transcript}"
    );
    f.handle.shutdown().await;
}
