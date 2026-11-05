//! The sessions screen's delete and export keys, through the production loop.
//!
//! The model only emits typed actions; every byte is read or removed by the
//! shell (ADR 0023). These tests use real journals in a temporary data root.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use harness::{
    tui::{
        Action, App,
        runtime::{self, Commands, Signal},
        runtime_nav::{NavigationReply, SessionCommands},
    },
    wiring::{self, SessionOptions},
};
use harness_core::{
    model::scripted::{Script, ScriptedProvider, Step},
    runtime::*,
    session::*,
};
use ratatui::{Terminal, backend::TestBackend};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};

/// The sessions screen's maintenance tests share one process with a Tokio
/// runtime that blocks on real journals; take turns so lock timing is exact.
/// An async mutex lets a serialized test hold the guard across its awaits.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL.lock().await
}

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
        Step::Event(harness_core::model::ProviderEvent::Started),
        Step::text(0, text),
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
        fs::create_dir(&workspace).unwrap();
        let mut driver = DriverConfig::default();
        driver.runtime.cancel_grace = Duration::from_millis(20);
        driver.runtime.context.catalog.insert(
            model("a"),
            harness_core::context::ModelLimits {
                context_tokens: Some(128_000),
                max_output_tokens: Some(8192),
            },
        );
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
    fn journal(&self, id: &SessionId) -> PathBuf {
        self.options
            .data_root
            .join("sessions")
            .join(format!("{id}.jsonl"))
    }
}
fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(width as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
/// The row the picker will show `id` on: catalog order, activity then ID.
fn row_index(f: &Fixture, id: &SessionId) -> usize {
    f.root()
        .sessions()
        .unwrap()
        .iter()
        .position(|entry| &entry.session_id == id)
        .expect("session in the catalog")
}

fn entry(id: &SessionId, title: &str, status: CatalogStatus) -> SessionCatalogEntry {
    SessionCatalogEntry {
        session_id: id.clone(),
        workspace: None,
        created_at: None,
        last_activity: None,
        model: Some(model("a")),
        state: Some(RuntimeState::Idle),
        title: Some(title.into()),
        status,
        reason: None,
    }
}
/// One bound for every stage, well inside the loop's own deadline: a stage
/// that cannot reach its state is a defect, not a slow machine.
const STAGE_DEADLINE: Duration = Duration::from_secs(5);

/// Drive one stage of keys through the production loop and return the app.
///
/// The catalog, the confirmation dialog and the deletion are all reached by
/// work the shell does between key presses. `runtime::drive` returns only once
/// every navigation future a stage dispatched has been applied to the app, so
/// the frames asserted after a stage are observed, never guessed from elapsed
/// time: each key is fed only when the state it acts on already exists.
/// `waiting_for` names that state, so a timeout says what never happened.
async fn stage(
    host: &SessionCommands,
    mut app: App,
    keys: impl IntoIterator<Item = KeyEvent>,
    waiting_for: &str,
) -> App {
    let events = futures_util::stream::iter(keys.into_iter().map(|key| Ok(Event::Key(key))));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    tokio::time::timeout(
        STAGE_DEADLINE,
        runtime::drive(
            &mut terminal,
            &mut app,
            events,
            host.subscriptions(),
            futures_util::stream::pending::<Signal>(),
            host,
            || {},
        ),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the loop did not reach {waiting_for} within {} s",
            STAGE_DEADLINE.as_secs()
        )
    })
    .unwrap_or_else(|error| panic!("the loop failed before {waiting_for}: {error}"));
    app
}

/// The picker only acts on rows it already holds, so a stage may not feed a
/// selection key before the listing has been applied. Titles are not unique,
/// so the wait is on the catalog's own shape: every entry is listed and the
/// empty-state marker is gone.
fn assert_catalog_loaded(app: &mut App, sessions: usize) {
    let screen = render(app, 80, 24);
    assert!(
        !screen.contains("(empty)") && screen.matches("(no message)").count() == sessions,
        "the session catalog did not list all {sessions} sessions:\n{screen}"
    );
}

#[tokio::test]
async fn delete_dialog_names_the_session_and_states_that_it_is_permanent() {
    let _serial = serial().await;
    let f = Fixture::new(vec![reply("answer")]);
    let host = f.host().await;
    let other = f.open().await;
    let id = other.session.clone();
    other.handle.shutdown().await;
    let mut app = host.app();
    app.on_subscription(SubscriptionItem::Reset(ClientView::from(
        host.handle().await.view().await.unwrap(),
    )));
    app.on_sessions(vec![entry(&id, "first title", CatalogStatus::Ok)]);
    assert_eq!(app.on_key(ctrl('o')), vec![Action::ListSessions]);
    app.on_sessions(vec![entry(&id, "first title", CatalogStatus::Ok)]);
    assert!(app.on_key(key(KeyCode::Char('d'))).is_empty());
    let screen = render(&mut app, 80, 24);
    assert!(screen.contains("delete session"), "{screen}");
    assert!(screen.contains("first title"), "{screen}");
    assert!(screen.contains(id.as_str()), "{screen}");
    assert!(screen.contains("permanent"), "{screen}");
    assert!(screen.contains("journal"), "{screen}");
    assert!(screen.contains("harness sessions gc"), "{screen}");
    // `n` cancels and sends nothing; `y` sends exactly the typed action.
    assert!(app.on_key(key(KeyCode::Char('n'))).is_empty());
    assert!(render(&mut app, 80, 24).contains("sessions ·"));
    assert!(app.on_key(key(KeyCode::Char('d'))).is_empty());
    assert_eq!(
        app.on_key(key(KeyCode::Char('y'))),
        vec![Action::DeleteSession { id: id.clone() }]
    );
    assert!(f.journal(&id).exists());
    host.shutdown().await;
}

#[tokio::test]
async fn delete_with_confirm_removes_the_other_journal_and_refreshes_the_list() {
    let _serial = serial().await;
    let f = Fixture::new(vec![reply("answer")]);
    let host = f.host().await;
    let current = host.session().await;
    let other = f.open().await;
    let other_id = other.session.clone();
    other.handle.shutdown().await;
    let before = f.root().sessions().unwrap().len();
    assert_eq!(before, 2);
    let down = row_index(&f, &other_id);
    // `d` only reaches a row once the catalog listing has been applied, so the
    // list is waited for before the selection keys are fed.
    let mut app = stage(&host, host.app(), [ctrl('o')], "the session catalog").await;
    assert_catalog_loaded(&mut app, before);
    let keys = (0..down)
        .map(|_| key(KeyCode::Down))
        .chain([key(KeyCode::Char('d'))]);
    let mut app = stage(&host, app, keys, "the delete confirmation").await;
    let screen = render(&mut app, 80, 24);
    assert!(
        screen.contains("delete session") && screen.contains(other_id.as_str()),
        "the delete confirmation did not name {other_id}:\n{screen}"
    );
    let mut app = stage(
        &host,
        app,
        [key(KeyCode::Char('y')), ctrl('d')],
        "the deletion result",
    )
    .await;
    let screen = render(&mut app, 80, 24);
    assert!(screen.contains("deleted session"), "{screen}");
    // The frame wraps its lines, so match the phrase, not the full command.
    assert!(screen.contains("reclaim space"), "{screen}");
    assert_eq!(f.root().sessions().unwrap().len(), before - 1);
    assert!(
        !f.journal(&other_id).exists(),
        "the selected journal survived"
    );
    assert!(
        f.journal(&current).exists(),
        "the current journal was removed"
    );
    host.shutdown().await;
}

#[tokio::test]
async fn the_current_session_cannot_be_deleted_from_the_sessions_screen() {
    let _serial = serial().await;
    let f = Fixture::new(vec![reply("answer")]);
    let host = f.host().await;
    let current = host.session().await;
    let mut app = host.app();
    app.on_subscription(SubscriptionItem::Reset(ClientView::from(
        host.handle().await.view().await.unwrap(),
    )));
    app.on_key(ctrl('o'));
    app.on_sessions(vec![entry(&current, "current title", CatalogStatus::Ok)]);
    // The model refuses before any confirmation dialog can send a deletion.
    assert!(app.on_key(key(KeyCode::Char('d'))).is_empty());
    let screen = render(&mut app, 80, 24);
    assert!(
        screen.contains("current session cannot be deleted"),
        "{screen}"
    );
    assert!(screen.contains("Switch to another"), "{screen}");
    // The shell refuses the same action independently, and removes nothing.
    let error = host
        .navigation(Action::DeleteSession {
            id: current.clone(),
        })
        .await
        .err()
        .expect("the current session must not be deletable");
    assert!(error.contains("current session"), "{error}");
    assert!(f.journal(&current).exists());
    host.shutdown().await;
}

#[tokio::test]
async fn a_locked_session_shows_the_refusal_code_verbatim() {
    let _serial = serial().await;
    let f = Fixture::new(vec![reply("answer")]);
    let host = f.host().await;
    let other = f.open().await;
    let id = other.session.clone();
    other.handle.shutdown().await;
    // Another engine holds the writer lock on that journal.
    let writer = JournalStore::open(Arc::new(f.root()), &id).unwrap();
    let before = f.root().sessions().unwrap().len();
    assert_eq!(before, 2);
    let down = row_index(&f, &id);
    let mut app = stage(&host, host.app(), [ctrl('o')], "the session catalog").await;
    assert_catalog_loaded(&mut app, before);
    let keys = (0..down)
        .map(|_| key(KeyCode::Down))
        .chain([key(KeyCode::Char('d'))]);
    let mut app = stage(&host, app, keys, "the delete confirmation").await;
    let screen = render(&mut app, 80, 24);
    assert!(
        screen.contains("delete session") && screen.contains(id.as_str()),
        "the delete confirmation did not name {id}:\n{screen}"
    );
    let mut app = stage(
        &host,
        app,
        [key(KeyCode::Char('y')), ctrl('d')],
        "the deletion result",
    )
    .await;
    let screen = render(&mut app, 80, 24);
    // `session_locked` appears either as the shell's refusal or on the row.
    assert!(screen.contains("session_locked"), "{screen}");
    assert!(f.journal(&id).exists());
    drop(writer);
    host.shutdown().await;
}

#[tokio::test]
async fn export_prompt_writes_markdown_and_refuses_to_overwrite() {
    let _serial = serial().await;
    let f = Fixture::new(vec![reply("answer")]);
    let host = f.host().await;
    let current = host.session().await;
    // Export the session this client owns: reading needs only a shared lock.
    let target = f.options.data_root.with_file_name("export.md");
    let mut app = host.app();
    app.on_subscription(SubscriptionItem::Reset(ClientView::from(
        host.handle().await.view().await.unwrap(),
    )));
    app.on_key(ctrl('o'));
    app.on_sessions(vec![entry(&current, "current title", CatalogStatus::Ok)]);
    assert!(app.on_key(key(KeyCode::Char('e'))).is_empty());
    let screen = render(&mut app, 80, 24);
    assert!(screen.contains("export session"), "{screen}");
    assert!(
        screen.contains(&format!("./harness-{}.md", &current.as_str()[..8])),
        "{screen}"
    );
    // Clear the prefill and type an absolute path with the paste handler.
    for _ in 0..40 {
        app.on_key(key(KeyCode::Backspace));
    }
    app.on_paste(target.to_str().unwrap());
    let actions = app.on_key(key(KeyCode::Enter));
    assert_eq!(
        actions,
        vec![Action::ExportSession {
            id: current.clone(),
            path: target.clone()
        }]
    );
    let reply = host.navigation(actions[0].clone()).await.unwrap();
    let NavigationReply::Maintenance { message, .. } = reply else {
        panic!("unexpected reply");
    };
    assert!(message.contains("wrote Markdown export to"), "{message}");
    let written = fs::read_to_string(&target).unwrap();
    assert!(
        written.starts_with(&format!("# Session {current}")),
        "{written}"
    );
    // A second export to the same path is refused with the stable code.
    let error = host
        .navigation(Action::ExportSession {
            id: current.clone(),
            path: target.clone(),
        })
        .await
        .err()
        .expect("an existing export must be refused");
    assert_eq!(error, "export_exists");
    assert_eq!(fs::read_to_string(&target).unwrap(), written);
    fs::remove_file(&target).unwrap();
    host.shutdown().await;
}

#[tokio::test]
async fn the_help_reference_documents_the_sessions_delete_and_export_keys() {
    let _serial = serial().await;
    let f = Fixture::new(vec![reply("answer")]);
    let host = f.host().await;
    let mut app = host.app();
    app.on_subscription(SubscriptionItem::Reset(ClientView::from(
        host.handle().await.view().await.unwrap(),
    )));
    app.on_key(key(KeyCode::F(1)));
    // The first render tells the dialog how many lines a page holds.
    render(&mut app, 100, 40);
    for _ in 0..2 {
        app.on_key(key(KeyCode::PageDown));
    }
    let screen = render(&mut app, 100, 40);
    assert!(screen.contains("Sessions d / e"), "{screen}");
    assert!(screen.contains("delete the selected session"), "{screen}");
    assert!(screen.contains("journal only, permanent"), "{screen}");
    host.shutdown().await;
}
