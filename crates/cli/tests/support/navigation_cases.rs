use super::*;
use futures_util::future::BoxFuture;
use harness::tui::runtime_nav::{NavigationFuture, NavigationReply};
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::test]
async fn busy_writer_retried_once_then_shows_busy() {
    let f = Fixture::new(vec![]);
    let host = f.host().await;
    let other = f.open().await;
    let id = other.session.clone();
    other.handle.shutdown().await;
    // The picker saw a healthy row, then another writer won the race.
    let entry = f
        .root()
        .sessions()
        .unwrap()
        .into_iter()
        .find(|e| e.session_id == id)
        .unwrap();
    let writer = JournalStore::open(Arc::new(f.root()), &id).unwrap();
    let mut app = reset(&host).await;
    app.on_key(ctrl('o'));
    app.on_sessions(vec![entry]);
    let start = std::time::Instant::now();
    let events = async_stream::stream! { yield Ok(Event::Key(key(KeyCode::Enter))); settle().await; yield Ok(Event::Key(ctrl('d'))); };
    let mut app = drive(&host, app, events).await;
    let screen = render(&mut app, 80, 24);
    assert!(screen.contains("after one retry"), "{screen}");
    assert!(start.elapsed() >= Duration::from_millis(30));
    // No unbounded retry: the old session was recovered and remains usable.
    assert_ne!(host.session().await, id);
    drop(writer);
    host.shutdown().await;
}

#[tokio::test]
async fn transient_session_lock_succeeds_on_the_single_retry() {
    let f = Fixture::new(vec![]);
    let host = f.host().await;
    let other = f.open().await;
    let id = other.session.clone();
    other.handle.shutdown().await;
    let writer = JournalStore::open(Arc::new(f.root()), &id).unwrap();
    let release = async {
        tokio::time::sleep(Duration::from_millis(15)).await;
        drop(writer);
    };
    let open = host.navigation(Action::OpenSession(id.clone()));
    let (result, ()) = tokio::join!(open, release);
    assert!(matches!(result, Ok(NavigationReply::Opened { .. })));
    assert_eq!(host.session().await, id);
    host.shutdown().await;
}

#[tokio::test]
async fn needs_repair_and_other_workspace_are_listed_but_not_opened() {
    use std::io::Write;
    for repair in [true, false] {
        let mut f = Fixture::new(vec![]);
        let host = f.host().await;
        let old = host.session().await;
        if !repair {
            let foreign = f._temp.path().join("foreign");
            std::fs::create_dir(&foreign).unwrap();
            f.options.workspace = foreign;
        }
        let other = f.open().await;
        let id = other.session.clone();
        other.handle.shutdown().await;
        if repair {
            let path = f
                .options
                .data_root
                .join("sessions")
                .join(format!("{id}.jsonl"));
            std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .unwrap()
                .write_all(b"{torn")
                .unwrap();
        }
        let endpoint = CountedScans::new(&host);
        let waiter = endpoint.clone();
        let events = async_stream::stream! { yield Ok(Event::Key(ctrl('o'))); waiter.wait_for(1).await; yield Ok(Event::Key(key(KeyCode::Enter))); settle().await; yield Ok(Event::Key(ctrl('d'))); };
        let mut app = drive_with(&endpoint, host.subscriptions(), host.app(), events).await;
        let screen = render(&mut app, 80, 24);
        assert!(
            screen.contains(if repair {
                "harness journal repair"
            } else {
                "different workspace"
            }),
            "{screen}"
        );
        assert_eq!(host.session().await, old);
        host.shutdown().await;
    }
}

struct DelayedCatalog {
    host: SessionCommands,
    listed: Arc<AtomicBool>,
}
impl Commands for DelayedCatalog {
    fn command(&self, command: Command) -> BoxFuture<'static, Result<Accepted, Rejection>> {
        self.host.command(command)
    }
    fn navigation(&self, action: Action) -> NavigationFuture {
        let host = self.host.clone();
        let listed = self.listed.clone();
        Box::pin(async move {
            if matches!(action, Action::ListSessions) {
                tokio::time::sleep(Duration::from_millis(120)).await;
                let result = host.navigation(action).await;
                listed.store(true, Ordering::SeqCst);
                result
            } else {
                assert!(
                    listed.load(Ordering::SeqCst),
                    "list/open ran concurrently: shared catalog lock may still be held"
                );
                host.navigation(action).await
            }
        })
    }
}

#[tokio::test]
async fn lock_contention_list_and_open_are_strictly_sequenced() {
    let f = Fixture::new(vec![]);
    let host = f.host().await;
    let other = f.open().await;
    let id = other.session.clone();
    other.handle.shutdown().await;
    let entry = f
        .root()
        .sessions()
        .unwrap()
        .into_iter()
        .find(|e| e.session_id == id)
        .unwrap();
    let mut app = reset(&host).await;
    app.on_sessions(vec![entry]);
    let endpoint = DelayedCatalog {
        host: host.clone(),
        listed: Arc::new(AtomicBool::new(false)),
    };
    let observed = host.clone();
    let events = async_stream::stream! {
        yield Ok(Event::Key(ctrl('o'))); yield Ok(Event::Key(key(KeyCode::Enter)));
        wait(&observed,|v|v.snapshot.session_id()==&id).await; settle().await; yield Ok(Event::Key(ctrl('d')));
    };
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let stats = tokio::time::timeout(
        Duration::from_secs(10),
        runtime::drive(
            &mut terminal,
            &mut app,
            events,
            host.subscriptions(),
            futures_util::stream::pending::<Signal>(),
            &endpoint,
            || {},
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(stats.ticks >= 5, "catalog latency blocked render ticks");
    assert!(endpoint.listed.load(Ordering::SeqCst));
    host.shutdown().await;
}

#[tokio::test]
async fn edit_select_parent_precedes_start_and_core_refusal_stops_sequence() {
    let f = Fixture::new(vec![reply("original"), reply("edited")]);
    let host = f.host().await;
    turn(&host, "original prompt").await;
    let original = host.handle().await.view().await.unwrap().snapshot;
    let events = async_stream::stream! { yield Ok(Event::Key(ctrl('t'))); yield Ok(Event::Key(key(KeyCode::Char('e')))); yield Ok(Event::Paste(" sibling".into())); yield Ok(Event::Key(key(KeyCode::Enter))); settle().await; yield Ok(Event::Key(ctrl('d'))); };
    let app = drive(&host, host.app(), events).await;
    let s = &app.view().unwrap().snapshot;
    assert_eq!(s.turns().len(), 2);
    let user_ids: Vec<_> = s.turns().map(|(_, t)| t.user_entry_id.clone()).collect();
    for user in &user_ids {
        assert!(s.entry(user).unwrap().entry.parent_id.is_none());
    }
    for (_, e) in original.entries() {
        assert_eq!(s.entry(&e.entry.id).unwrap().entry, e.entry);
    }
    let before = s.sequence();
    assert!(matches!(
        host.sequence(vec![
            Command::SelectHead {
                head: Some(EntryId::new())
            },
            Command::StartTurn {
                command_id: CommandId::new(),
                content: vec![ContentBlock::Text {
                    text: "must not run".into()
                }]
            }
        ])
        .await,
        Err(Rejection::Invalid("unknown_head"))
    ));
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
    host.shutdown().await;
}

#[tokio::test]
async fn navigation_renders_80x24_evidence_and_tiny_buffers() {
    let f = Fixture::new(vec![opaque()]);
    let host = f.host().await;
    turn(&host, "Explain branches").await;
    let other = f.open().await;
    other.handle.shutdown().await;
    let mut app = reset(&host).await;
    app.on_key(ctrl('o'));
    app.on_sessions(f.root().sessions().unwrap());
    println!("PICKER 80x24\n{}", render(&mut app, 80, 24));
    app.on_key(key(KeyCode::Esc));
    app.on_key(ctrl('t'));
    println!("BRANCH TREE 80x24\n{}", render(&mut app, 80, 24));
    render(&mut app, 20, 5);
    // The real loop supplies the classified switch report (no synthetic payload).
    app.on_key(key(KeyCode::Esc));
    let events = async_stream::stream! { yield Ok(Event::Key(ctrl('l'))); yield Ok(Event::Key(key(KeyCode::Down))); yield Ok(Event::Key(key(KeyCode::Enter))); settle().await; yield Ok(Event::Key(ctrl('d'))); };
    let mut app = drive(&host, app, events).await;
    let screen = render(&mut app, 80, 24);
    println!("SWITCH REPORT 80x24\n{screen}");
    assert!(screen.contains("native.test.v1"));
    assert!(!screen.contains("OPAQUE_SENTINEL"));
    render(&mut app, 20, 5);
    render(&mut app, 1, 1);
    host.shutdown().await;
}
