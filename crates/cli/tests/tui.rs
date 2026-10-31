//! Offline TUI-model tests: subscriptions recorded from a real in-process
//! `ScriptedProvider` session, reduced and rendered with Ratatui's TestBackend.
//!
//! The app itself uses no terminal and no Tokio. The fixture drives the real
//! `SessionHandle` (which needs a Tokio test runtime and real storage) only to
//! record real `SubscriptionItem`s, exactly as a shell would.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harness::tui::{Action, App, SessionInfo};
use harness_core::{
    context::ModelLimits,
    model::{
        ProviderEvent,
        scripted::{Script, ScriptedProvider, Step},
    },
    runtime::*,
    session::*,
};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};

const SENTINEL: &str = "SENTINEL-OPAQUE-PAYLOAD-9f31";
const DEADLINE: Duration = Duration::from_secs(30);

fn model() -> ModelRef {
    ModelRef {
        provider_instance: "mock".into(),
        model: "scripted".into(),
    }
}

fn other_model() -> ModelRef {
    ModelRef {
        provider_instance: "mock".into(),
        model: "other".into(),
    }
}

fn summary_model() -> ModelRef {
    ModelRef {
        provider_instance: "summary".into(),
        model: "cheap".into(),
    }
}

fn policy(compaction: bool) -> Vec<u8> {
    let mut value = json!({
        "policy_revision": "tui-test",
        "enabled_tools": ["read", "write", "edit"],
        "workspace_read": "allow",
        "workspace_mutation": "ask",
        "process": "deny",
        "network": "deny",
        "protected_paths": [],
        "allowances": []
    });
    if compaction {
        value["compaction"] = json!({
            "summary_model": {"provider_instance": "summary", "model": "cheap"},
            "automatic": false
        });
    }
    serde_json::to_vec(&value).unwrap()
}

fn driver_config(compaction: bool) -> DriverConfig {
    let mut config = DriverConfig::default();
    for model in [model(), other_model()] {
        config.runtime.context.catalog.insert(
            model,
            ModelLimits {
                context_tokens: Some(128_000),
                max_output_tokens: None,
            },
        );
    }
    if compaction {
        config.runtime.context.catalog.insert(
            summary_model(),
            ModelLimits {
                context_tokens: Some(128_000),
                max_output_tokens: None,
            },
        );
    }
    config
}

fn reply(text: &str) -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, text),
        Step::completed(FinishReason::Stop),
    ])
}

fn proposal(name: &str, arguments: Value) -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::Event(ProviderEvent::ToolCallStarted {
            block_index: 0,
            name: name.into(),
            provider_call_id: None,
        }),
        Step::Event(ProviderEvent::ToolArgumentsDelta {
            block_index: 0,
            fragment: arguments.to_string(),
        }),
        Step::Event(ProviderEvent::ToolCallReady { block_index: 0 }),
        Step::completed(FinishReason::ToolCalls),
    ])
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: Arc<DataRoot>,
    host: ToolHostConfig,
    session: SessionId,
    header: SessionHeader,
    workspace: PathBuf,
}

impl Fixture {
    fn new(compaction: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
        let host = ToolHostConfig {
            workspace_root: workspace.clone(),
            data_root: root.path().into(),
            sandbox: None,
        };
        let configuration = root.put_artifact(&policy(compaction)).unwrap();
        let header = SessionHeader {
            workspace: workspace.to_str().unwrap().into(),
            initial_model: model(),
            policy_revision: "tui-test".into(),
            configuration,
        };
        Self {
            _temp: temp,
            root,
            host,
            session: SessionId::new(),
            header,
            workspace,
        }
    }

    fn info(&self) -> SessionInfo {
        SessionInfo {
            workspace: self.workspace.clone(),
            model: model(),
            context_limit: Some(128_000),
        }
    }

    async fn create(&self, scripts: impl IntoIterator<Item = Script>) -> Recorded {
        self.create_with(scripts, false).await
    }

    async fn create_with(
        &self,
        scripts: impl IntoIterator<Item = Script>,
        compaction: bool,
    ) -> Recorded {
        let provider = Arc::new(ScriptedProvider::new(scripts));
        let handle = tokio::time::timeout(
            DEADLINE,
            SessionHandle::create_with_tools(
                self.root.clone(),
                self.session.clone(),
                self.header.clone(),
                provider,
                driver_config(compaction),
                &self.host,
            ),
        )
        .await
        .expect("session creation deadline")
        .expect("session creation");
        Recorded::new(self.info(), handle).await
    }
}

/// One live subscription with everything it yielded, plus the reducer state.
struct Recorded {
    info: SessionInfo,
    handle: SessionHandle,
    subscription: Subscription,
    view: ClientView,
    items: Vec<SubscriptionItem>,
}

impl Recorded {
    async fn new(info: SessionInfo, handle: SessionHandle) -> Self {
        let mut subscription = handle.subscribe();
        let view = match tokio::time::timeout(DEADLINE, subscription.next())
            .await
            .expect("first subscription item deadline")
            .expect("subscription open")
        {
            SubscriptionItem::Reset(client) => client,
            SubscriptionItem::Event(_) => panic!("subscription must start with a reset"),
        };
        Self {
            info,
            handle,
            subscription,
            view: view.clone(),
            items: vec![SubscriptionItem::Reset(view)],
        }
    }

    fn app(&self) -> App {
        let mut app = App::new(self.info.clone());
        for item in &self.items {
            app.on_subscription(item.clone());
        }
        app
    }

    /// Keep reducing until `done` holds for the subscription's view.
    async fn pump(&mut self, mut done: impl FnMut(&RuntimeView) -> bool) {
        loop {
            if done(self.view.view()) {
                return;
            }
            let item = tokio::time::timeout(DEADLINE, self.subscription.next())
                .await
                .expect("subscription deadline")
                .expect("subscription closed");
            match &item {
                SubscriptionItem::Reset(client) => self.view = client.clone(),
                SubscriptionItem::Event(event) => {
                    // A manually recorded reset may already cover buffered
                    // events; the shell's own reducer ignores them the same way.
                    assert!(
                        matches!(
                            self.view.apply(event),
                            ApplyOutcome::Applied | ApplyOutcome::Duplicate
                        ),
                        "event {} needed a resync",
                        event.seq
                    );
                }
            }
            self.items.push(item);
        }
    }

    /// Record one item that the shell would receive after a lag or epoch change.
    async fn record_reset(&mut self) {
        let view = self.handle.view().await.expect("view");
        let client = ClientView::from(view);
        self.view = client.clone();
        self.items.push(SubscriptionItem::Reset(client));
    }

    async fn start(&mut self, text: &str) -> TurnId {
        let accepted = tokio::time::timeout(
            DEADLINE,
            self.handle.command(Command::StartTurn {
                command_id: CommandId::new(),
                content: vec![ContentBlock::Text { text: text.into() }],
            }),
        )
        .await
        .expect("start reply deadline")
        .expect("turn started");
        let Accepted::TurnStarted { turn_id, .. } = accepted else {
            panic!("expected TurnStarted, got {accepted:?}");
        };
        turn_id
    }

    async fn wait_finished(&mut self, turn: &TurnId) {
        self.pump(|view| {
            view.snapshot.events().any(|(_, _, event)| {
                matches!(event, JournalEvent::TurnFinished(finished) if finished.turn_id == *turn)
            }) && view.phase == Phase::Durable(RuntimeState::Idle)
                && view.active_turn.is_none()
        })
        .await;
    }

    async fn wait_approval(&mut self) -> PendingApproval {
        self.pump(|view| view.pending_approval.is_some()).await;
        self.view
            .view()
            .pending_approval
            .clone()
            .expect("approval requested")
    }

    async fn submit(&self, ticket: &PendingApproval, decision: Decision, remember: bool) {
        let accepted = tokio::time::timeout(
            DEADLINE,
            self.handle.command(Command::SubmitApproval {
                approval_id: ticket.approval_id.clone(),
                plan_digest: ticket.plan_digest.clone(),
                decision,
                remember,
            }),
        )
        .await
        .expect("approval reply deadline");
        assert!(
            matches!(accepted, Ok(Accepted::ApprovalSubmitted { .. })),
            "unexpected approval reply: {accepted:?}"
        );
    }

    async fn shutdown(self) {
        tokio::time::timeout(DEADLINE, self.handle.shutdown())
            .await
            .expect("shutdown deadline");
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn modified(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn render(app: &mut App, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    terminal.backend().buffer().clone()
}

fn text(buffer: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        app.on_key(key(KeyCode::Char(ch)));
    }
}

#[tokio::test]
async fn streamed_answer_renders_markdown_and_status() {
    let fixture = Fixture::new(false);
    let answer = "# Result\n\nSome **bold** and `inline` text with *emphasis*.\n\n- one\n- two\n\n> quoted\n\n```rust\nfn main() {}\n```\n";
    let mut recorded = fixture.create([reply(answer)]).await;
    let turn = recorded.start("explain").await;
    recorded.wait_finished(&turn).await;
    let buffer = render(&mut recorded.app(), 80, 24);
    let screen = text(&buffer);
    assert!(screen.contains("# Result"), "{screen}");
    assert!(screen.contains("bold"), "{screen}");
    assert!(screen.contains("inline"), "{screen}");
    assert!(screen.contains("• one"), "{screen}");
    assert!(screen.contains("• two"), "{screen}");
    assert!(screen.contains("│ quoted"), "{screen}");
    assert!(screen.contains("┌─ code rust"), "{screen}");
    assert!(screen.contains("fn main() {}"), "{screen}");
    assert!(screen.contains("turn: completed"), "{screen}");
    assert!(!screen.contains("streaming"), "{screen}");
    assert!(screen.contains("usage"), "{screen}");
    recorded.shutdown().await;
}

#[tokio::test]
async fn approval_modal_captures_a_d_r_and_esc() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture
        .create([
            proposal("write", json!({"path": "note.txt", "text": "hello"})),
            reply("wrote it"),
        ])
        .await;
    let turn = recorded.start("write a file").await;
    let ticket = recorded.wait_approval().await;
    let mut app = recorded.app();
    let buffer = render(&mut app, 80, 24);
    let screen = text(&buffer);
    assert!(screen.contains("approve tool call"), "{screen}");
    assert!(screen.contains("note.txt"), "{screen}");
    assert!(screen.contains("a allow"), "{screen}");

    for (code, decision, remember) in [
        (KeyCode::Char('a'), Decision::Allow, false),
        (KeyCode::Char('d'), Decision::Deny, false),
        (KeyCode::Char('r'), Decision::Allow, true),
        (KeyCode::Esc, Decision::Deny, false),
    ] {
        let actions = app.on_key(key(code));
        assert_eq!(actions.len(), 1, "{code:?}");
        let Action::Command(Command::SubmitApproval {
            approval_id,
            plan_digest,
            decision: actual_decision,
            remember: actual_remember,
        }) = &actions[0]
        else {
            panic!("expected SubmitApproval, got {:?}", actions[0]);
        };
        assert_eq!(approval_id, &ticket.approval_id);
        assert_eq!(plan_digest, &ticket.plan_digest);
        assert_eq!(*actual_decision, decision, "{code:?}");
        assert_eq!(*actual_remember, remember, "{code:?}");
    }

    recorded.submit(&ticket, Decision::Allow, false).await;
    recorded.wait_finished(&turn).await;
    let screen = text(&render(&mut recorded.app(), 80, 24));
    assert!(screen.contains("tool result: succeeded"), "{screen}");
    assert!(screen.contains("approval: allowed"), "{screen}");
    recorded.shutdown().await;
}

#[tokio::test]
async fn denied_tool_is_rendered_with_its_outcome() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture
        .create([
            proposal("write", json!({"path": "note.txt", "text": "hello"})),
            reply("the write was denied"),
        ])
        .await;
    let turn = recorded.start("write a file").await;
    let ticket = recorded.wait_approval().await;
    recorded.submit(&ticket, Decision::Deny, false).await;
    recorded.wait_finished(&turn).await;
    let screen = text(&render(&mut recorded.app(), 80, 24));
    assert!(screen.contains("tool result: denied"), "{screen}");
    assert!(screen.contains("approval: denied"), "{screen}");
    assert!(
        screen.contains("the write was denied"),
        "the next assistant reply must render: {screen}"
    );
    recorded.shutdown().await;
}

#[tokio::test]
async fn cancelled_turn_streams_then_finishes_cancelled() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture
        .create([Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, "partial answer"),
            Step::AwaitCancel,
            Step::cancelled(),
        ])])
        .await;
    let turn = recorded.start("keep going").await;
    recorded
        .pump(|view| {
            view.partial.as_ref().is_some_and(|partial| {
                partial
                    .blocks
                    .values()
                    .any(|block| matches!(block, PartialBlock::Text(text) if !text.is_empty()))
            })
        })
        .await;
    let streaming = text(&render(&mut recorded.app(), 80, 24));
    assert!(streaming.contains("partial answer"), "{streaming}");
    assert!(streaming.contains("streaming"), "{streaming}");
    assert!(
        matches!(
            recorded
                .handle
                .command(Command::Cancel {
                    turn_id: turn.clone()
                })
                .await,
            Ok(Accepted::Cancelling { .. })
        ),
        "cancel must be durable"
    );
    recorded.wait_finished(&turn).await;
    let screen = text(&render(&mut recorded.app(), 80, 24));
    assert!(screen.contains("turn: cancelled"), "{screen}");
    recorded.shutdown().await;
}

#[tokio::test]
async fn failed_turn_shows_its_code() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture
        .create([Script::new([
            Step::Event(ProviderEvent::Started),
            Step::failed(ModelError {
                code: ModelErrorCode::RateLimit,
                message: "too many requests".into(),
                provider_code: None,
                retry_after_ms: None,
            }),
        ])])
        .await;
    let turn = recorded.start("hello").await;
    recorded.wait_finished(&turn).await;
    let screen = text(&render(&mut recorded.app(), 80, 24));
    assert!(screen.contains("turn: failed (rate_limit)"), "{screen}");
    assert!(!screen.contains("too many requests"), "{screen}");
    recorded.shutdown().await;
}

#[tokio::test]
async fn compaction_summary_is_a_distinct_block() {
    let fixture = Fixture::new(true);
    let answer = "## Task and constraints\nKeep task.\n## Decisions\nRust.\n## Files and actions\nRead succeeded.\n## Outcomes\nDone.\n## Unresolved work\nNone.\n";
    let summary = Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, answer),
        Step::completed(FinishReason::Stop),
    ]);
    let mut recorded = fixture
        .create_with(
            [
                reply("one"),
                reply("two"),
                reply("three"),
                reply("four"),
                summary,
            ],
            true,
        )
        .await;
    for text in ["one", "two", "three", "four"] {
        let turn = recorded.start(text).await;
        recorded.wait_finished(&turn).await;
    }
    let Accepted::CompactionPreviewed { preview_id, .. } = recorded
        .handle
        .command(Command::PreviewCompaction)
        .await
        .expect("preview")
    else {
        panic!("expected a preview");
    };
    let accepted = recorded
        .handle
        .command(Command::CommitCompaction { preview_id })
        .await
        .expect("commit");
    assert!(matches!(accepted, Accepted::CompactionCommitted { .. }));
    recorded
        .pump(|view| {
            view.snapshot
                .entries()
                .any(|(_, entry)| matches!(entry.entry.payload, EntryPayload::Compaction(_)))
        })
        .await;
    let screen = text(&render(&mut recorded.app(), 80, 24));
    assert!(screen.contains("compaction summary"), "{screen}");
    assert!(screen.contains("Keep task."), "{screen}");
    recorded.shutdown().await;
}

#[tokio::test]
async fn model_switch_renders_its_portability() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture.create([reply("before")]).await;
    let turn = recorded.start("hello").await;
    recorded.wait_finished(&turn).await;
    let accepted = recorded
        .handle
        .command(Command::SwitchModel {
            model: other_model(),
            policy: Portability::Reject,
        })
        .await
        .expect("switch");
    assert!(matches!(accepted, Accepted::ModelSwitched { .. }));
    recorded
        .pump(|view| {
            view.snapshot
                .entries()
                .any(|(_, entry)| matches!(entry.entry.payload, EntryPayload::ModelChanged(_)))
        })
        .await;
    let screen = text(&render(&mut recorded.app(), 80, 24));
    assert!(
        screen.contains("model switched: mock/other (portability: reject)"),
        "{screen}"
    );
    recorded.shutdown().await;
}

#[tokio::test]
async fn mid_stream_reset_rebuilds_without_duplicates() {
    let fixture = Fixture::new(false);
    let (release, wait) = tokio::sync::oneshot::channel();
    let mut recorded = fixture
        .create([Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, "Hello "),
            Step::Wait(wait),
            Step::text(0, "world"),
            Step::completed(FinishReason::Stop),
        ])])
        .await;
    let turn = recorded.start("hi").await;
    recorded
        .pump(|view| {
            view.partial.as_ref().is_some_and(|partial| {
                partial.blocks.values().any(
                    |block| matches!(block, PartialBlock::Text(text) if text.contains("Hello")),
                )
            })
        })
        .await;
    assert!(
        text(&render(&mut recorded.app(), 80, 24)).contains("streaming"),
        "the partial response must be shown as streaming"
    );

    // Feed only the items before the shell's reset, then the reset itself.
    let reset_at = recorded.items.len();
    recorded.record_reset().await;
    let mut before = App::new(recorded.info.clone());
    for item in &recorded.items[..reset_at] {
        before.on_subscription(item.clone());
    }
    let before_screen = text(&render(&mut before, 80, 80));
    assert_eq!(
        before_screen.matches("Hello").count(),
        1,
        "the pre-reset transcript: {before_screen}"
    );

    let mut after = App::new(recorded.info.clone());
    for item in &recorded.items[..=reset_at] {
        after.on_subscription(item.clone());
    }
    let after_screen = text(&render(&mut after, 80, 80));
    assert_eq!(
        without_chrome(&after_screen),
        without_chrome(&before_screen),
        "a reset must rebuild the same transcript"
    );
    assert_eq!(
        after_screen.matches("Hello").count(),
        1,
        "a reset must not duplicate the transcript: {after_screen}"
    );

    release.send(()).unwrap();
    recorded.wait_finished(&turn).await;
    // Tall enough to show the whole transcript: a duplicate above the
    // viewport would otherwise be invisible at the tail.
    let screen = text(&render(&mut recorded.app(), 80, 80));
    assert_eq!(
        screen.matches("Hello").count(),
        1,
        "the completed transcript must not duplicate either: {screen}"
    );
    assert!(screen.contains("Hello world"), "{screen}");
    recorded.shutdown().await;
}

/// Everything above the prompt and status rows.
fn without_chrome(screen: &str) -> String {
    let lines: Vec<&str> = screen.lines().collect();
    lines[..lines.len().saturating_sub(2)].join("\n")
}

#[tokio::test]
async fn narrow_and_wide_areas_render_without_panicking() {
    let fixture = Fixture::new(false);
    let long = "word ".repeat(60);
    let mut recorded = fixture.create([reply(&long)]).await;
    let turn = recorded.start("hi").await;
    recorded.wait_finished(&turn).await;
    let mut app = recorded.app();
    // 20x5 with an approval-free transcript and the prompt.
    let narrow = render(&mut app, 20, 5);
    assert_eq!(narrow.area.width, 20);
    assert_eq!(narrow.area.height, 5);
    let wide = render(&mut app, 80, 24);
    for y in 0..wide.area.height {
        let mut width = 0usize;
        for x in 0..wide.area.width {
            if wide[(x, y)].symbol() != " " {
                width = x as usize + 1;
            }
        }
        assert!(width <= 80, "row {y} is {width} cells wide");
    }
    let screen = text(&wide);
    assert!(
        screen.contains("word word"),
        "wrapped text must be visible: {screen}"
    );
    recorded.shutdown().await;
}

#[tokio::test]
async fn every_decision_9_key_binding() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture
        .create([Script::new([
            Step::Event(ProviderEvent::Started),
            Step::Event(ProviderEvent::ReasoningSummaryDelta {
                block_index: 0,
                text: "first line\nsecond line\nthird line".into(),
            }),
            Step::text(1, "answer"),
            Step::AwaitCancel,
            Step::cancelled(),
        ])])
        .await;
    let turn = recorded.start("go").await;
    recorded
        .pump(|view| {
            view.partial.as_ref().is_some_and(|partial| {
                partial
                    .blocks
                    .values()
                    .any(|block| matches!(block, PartialBlock::Text(text) if !text.is_empty()))
            })
        })
        .await;

    // An idle app from the initial reset: Enter sends, newline chords insert.
    let mut idle = App::new(recorded.info.clone());
    idle.on_subscription(recorded.items[0].clone());
    assert!(idle.on_key(key(KeyCode::Enter)).is_empty(), "empty prompt");
    type_text(&mut idle, "hello");
    let actions = idle.on_key(key(KeyCode::Enter));
    assert_eq!(actions.len(), 1);
    let Action::Command(Command::StartTurn { content, .. }) = &actions[0] else {
        panic!("expected StartTurn, got {:?}", actions[0]);
    };
    assert_eq!(
        content,
        &vec![ContentBlock::Text {
            text: "hello".into()
        }]
    );
    assert!(
        idle.on_key(key(KeyCode::Enter)).is_empty(),
        "prompt cleared"
    );
    type_text(&mut idle, "a");
    idle.on_key(modified(KeyCode::Enter, KeyModifiers::SHIFT));
    idle.on_key(modified(KeyCode::Enter, KeyModifiers::ALT));
    let actions = idle.on_key(key(KeyCode::Enter));
    assert!(
        matches!(
            actions.as_slice(),
            [Action::Command(Command::StartTurn { content, .. })]
                if content == &vec![ContentBlock::Text {
                    text: "a\n\n".into()
                }]
        ),
        "Shift/Alt+Enter must insert newlines: {actions:?}"
    );

    // A busy app: Enter must not send while a turn is active.
    let mut app = recorded.app();
    type_text(&mut app, "busy");
    assert!(
        app.on_key(key(KeyCode::Enter)).is_empty(),
        "Enter must not send while a turn is active"
    );
    app.on_key(modified(KeyCode::Enter, KeyModifiers::SHIFT));
    app.on_key(modified(KeyCode::Enter, KeyModifiers::ALT));
    let screen = text(&render(&mut app, 80, 24));
    assert!(screen.contains("busy"), "{screen}");

    // Esc cancels the active turn.
    let actions = app.on_key(key(KeyCode::Esc));
    assert!(matches!(
        actions.as_slice(),
        [Action::Command(Command::Cancel { turn_id })] if turn_id == &turn
    ));

    // Ctrl-C cancels once, then quits within the same busy period.
    let actions = app.on_key(modified(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(matches!(
        actions.as_slice(),
        [Action::Command(Command::Cancel { turn_id })] if turn_id == &turn
    ));
    let actions = app.on_key(modified(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(matches!(
        actions.as_slice(),
        [Action::Quit { cancel: Some(turn_id) }] if turn_id == &turn
    ));

    // Scrolling is mouse-free in a small viewport.
    let before = text(&render(&mut app, 80, 8));
    app.on_key(key(KeyCode::PageUp));
    let after = text(&render(&mut app, 80, 8));
    assert_ne!(before, after, "PageUp must move the transcript");
    app.on_key(key(KeyCode::PageDown));

    // Reasoning summaries collapse to one line and toggle with Ctrl-R.
    let collapsed = text(&render(&mut app, 80, 24));
    assert!(collapsed.contains("reasoning: first line"), "{collapsed}");
    assert!(!collapsed.contains("second line"), "{collapsed}");
    app.on_key(modified(KeyCode::Char('r'), KeyModifiers::CONTROL));
    let expanded = text(&render(&mut app, 80, 24));
    assert!(expanded.contains("second line"), "{expanded}");
    app.on_key(modified(KeyCode::Char('r'), KeyModifiers::CONTROL));

    // Ctrl-C while idle asks before leaving; Ctrl-D on an empty prompt quits.
    recorded
        .handle
        .command(Command::Cancel {
            turn_id: turn.clone(),
        })
        .await
        .unwrap();
    recorded.wait_finished(&turn).await;
    let mut finished = recorded.app();
    let actions = finished.on_key(modified(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(actions.is_empty());
    assert!(text(&render(&mut finished, 80, 24)).contains("Quit harness?"));
    finished.on_key(key(KeyCode::Char('n')));
    assert!(
        finished
            .on_key(modified(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .is_empty()
    );
    finished.on_key(key(KeyCode::Char('y')));
    type_text(&mut finished, "x");
    assert!(
        finished
            .on_key(modified(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .is_empty()
    );
    finished.on_key(key(KeyCode::Backspace));
    assert!(matches!(
        finished
            .on_key(modified(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .as_slice(),
        [Action::Quit { cancel: None }]
    ));
    recorded.shutdown().await;
}

#[tokio::test]
async fn uncertain_modal_offers_the_core_resolutions() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture
        .create([
            proposal("write", json!({"path": "note.txt", "text": "hello"})),
            reply("done"),
        ])
        .await;
    let turn = recorded.start("write").await;
    let ticket = recorded.wait_approval().await;
    recorded.submit(&ticket, Decision::Allow, false).await;
    recorded.wait_finished(&turn).await;

    // The snapshot is real; resolution itself needs a crash between intent and
    // result, so the awaiting list is attached to the recorded view.
    let mut view = recorded.handle.view().await.unwrap();
    let call = view
        .snapshot
        .entries()
        .find_map(|(_, entry)| match &entry.entry.payload {
            EntryPayload::AssistantResponse(response) => {
                response.content.iter().find_map(|block| match block {
                    ContentBlock::ToolCall { call_id, .. } => Some(CallRef {
                        response_entry_id: entry.entry.id.clone(),
                        call_id: call_id.clone(),
                    }),
                    _ => None,
                })
            }
            _ => None,
        })
        .expect("recorded tool call");
    let invocation_id = InvocationId::new();
    view.phase = Phase::AwaitingResolution;
    view.awaiting = vec![UncertainInvocation {
        invocation_id: invocation_id.clone(),
        turn_id: turn.clone(),
        call,
        tool: "write".into(),
        plan_digest: Digest::of(b"plan"),
        intent_at: Timestamp::now(),
        hint: Some(harness_core::tools::RecoveryHint::MatchesPostImage),
    }];
    let mut app = App::new(recorded.info.clone());
    app.on_subscription(SubscriptionItem::Reset(ClientView::from(view)));
    let screen = text(&render(&mut app, 80, 24));
    assert!(screen.contains("uncertain invocation"), "{screen}");
    assert!(screen.contains("matches the post-image"), "{screen}");

    let actions = app.on_key(key(KeyCode::Enter));
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        Action::Command(Command::ResolveUncertain {
            invocation_id: chosen,
            resolution: UserResolution::AcknowledgedUnknown,
        }) if chosen == &invocation_id
    ));

    app.on_key(key(KeyCode::Right));
    type_text(&mut app, "file written");
    let actions = app.on_key(key(KeyCode::Enter));
    assert!(matches!(
        &actions[0],
        Action::Command(Command::ResolveUncertain {
            invocation_id: chosen,
            resolution: UserResolution::Observed {
                status: ToolStatus::Succeeded,
                text,
            },
        }) if chosen == &invocation_id && text == "file written"
    ));
    recorded.shutdown().await;
}

#[tokio::test]
async fn opaque_provider_state_sentinel_never_renders() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture
        .create([Script::new([
            Step::Event(ProviderEvent::Started),
            Step::provider_state(0, "openai.encrypted", SENTINEL.as_bytes().to_vec()),
            Step::text(1, "visible answer"),
            Step::completed(FinishReason::Stop),
        ])])
        .await;
    let turn = recorded.start("hello").await;
    recorded.wait_finished(&turn).await;
    let mut app = recorded.app();
    let wide = text(&render(&mut app, 80, 24));
    assert!(!wide.contains(SENTINEL), "sentinel leaked: {wide}");
    assert!(wide.contains("provider state"), "{wide}");
    let narrow = text(&render(&mut app, 20, 5));
    assert!(!narrow.contains(SENTINEL), "sentinel leaked: {narrow}");
    app.on_key(modified(KeyCode::Char('r'), KeyModifiers::CONTROL));
    let screen = text(&render(&mut app, 80, 24));
    assert!(!screen.contains(SENTINEL), "sentinel leaked: {screen}");
    assert!(screen.contains("visible answer"), "{screen}");
    recorded.shutdown().await;
}
