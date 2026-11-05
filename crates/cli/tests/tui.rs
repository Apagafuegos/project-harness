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
    policy_with_read(compaction, "allow")
}

fn policy_with_read(compaction: bool, workspace_read: &str) -> Vec<u8> {
    let mut value = json!({
        "policy_revision": "tui-test",
        "enabled_tools": ["read", "write", "edit", "list", "glob", "grep"],
        "workspace_read": workspace_read,
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

    /// A session whose read policy asks, so a `read` call produces a ticket.
    fn new_read_ask() -> Self {
        let mut fixture = Self::new(false);
        fixture.header.configuration = fixture
            .root
            .put_artifact(&policy_with_read(false, "ask"))
            .unwrap();
        fixture
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

    async fn open(&self, provider: Arc<ScriptedProvider>) -> Recorded {
        let handle = tokio::time::timeout(
            DEADLINE,
            SessionHandle::open_with_tools(
                self.root.clone(),
                self.session.clone(),
                provider,
                driver_config(false),
                &self.host,
            ),
        )
        .await
        .expect("session open deadline")
        .expect("session open");
        Recorded::new(self.info(), handle).await
    }

    /// Append a durable `undo_intent` with no result, as a crash leaves it.
    fn inject_pending_undo(&self) {
        let store = JournalStore::open(self.root.clone(), &self.session).unwrap();
        let snapshot = store.snapshot();
        let UndoSelection::Candidate {
            turn_id,
            targets,
            omitted_calls,
        } = snapshot.select_undo().unwrap()
        else {
            panic!("a candidate turn")
        };
        drop(store);
        let mut store = JournalStore::open(self.root.clone(), &self.session).unwrap();
        store
            .append(
                Commit {
                    command_id: None,
                    command_fingerprint: None,
                    head: HeadChange {
                        from: snapshot.head().cloned(),
                        to: snapshot.head().cloned(),
                        reason: HeadReason::Retain,
                    },
                    entries: Vec::new(),
                    events: vec![JournalEvent::UndoIntent(UndoIntent {
                        undo_id: UndoId::new(),
                        target_turn_id: turn_id,
                        anchor_head: snapshot.head().cloned(),
                        configuration: snapshot.configuration().clone(),
                        policy_revision: snapshot.policy_revision().into(),
                        targets,
                        omitted_calls,
                    })],
                },
                Timestamp::now(),
            )
            .unwrap();
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
        self.app_with(false)
    }

    /// Reduce the recorded items into an app whose read-only setting is already
    /// applied, exactly as the shell sets it before the first subscription item.
    fn app_with(&self, read_only_approval: bool) -> App {
        let mut app = App::new(self.info.clone());
        app.set_read_only_approval(read_only_approval);
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
    let mut recorded = fixture
        .create([Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, answer),
            Step::Event(ProviderEvent::UsageUpdated {
                usage: Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(20),
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                },
            }),
            Step::completed(FinishReason::Stop),
        ])])
        .await;
    let turn = recorded.start("explain").await;
    recorded.wait_finished(&turn).await;
    let buffer = render(&mut recorded.app(), 80, 24);
    let screen = text(&buffer);
    // M7-06: the heading is styled rather than hashed, and a code block is a
    // dim language label over a `│` gutter instead of a box frame.
    assert!(screen.contains("▌ Result"), "{screen}");
    assert!(!screen.contains("# Result"), "{screen}");
    assert!(screen.contains("bold"), "{screen}");
    assert!(screen.contains("inline"), "{screen}");
    assert!(!screen.contains('`'), "{screen}");
    assert!(screen.contains("• one"), "{screen}");
    assert!(screen.contains("• two"), "{screen}");
    assert!(screen.contains("│ quoted"), "{screen}");
    assert!(screen.contains("▌ rust"), "{screen}");
    assert!(screen.contains("│ fn main() {}"), "{screen}");
    assert!(!screen.contains("turn: completed"), "{screen}");
    assert!(!screen.contains("streaming"), "{screen}");
    assert!(screen.contains("10 in / 20 out"), "{screen}");
    recorded.shutdown().await;
}

#[tokio::test]
async fn context_control_entries_render_a_one_line_marker() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture.create([reply("done")]).await;
    let turn = recorded.start("first").await;
    recorded.wait_finished(&turn).await;
    for (mode, marker) in [
        (ContextMode::Pinned, "Pinned turn"),
        (ContextMode::Excluded, "Excluded turn"),
        (ContextMode::Default, "Reset turn"),
    ] {
        let accepted = tokio::time::timeout(
            DEADLINE,
            recorded.handle.command(Command::SetContextControl {
                turn_id: turn.clone(),
                mode,
            }),
        )
        .await
        .expect("control deadline")
        .expect("control accepted");
        assert!(matches!(accepted, Accepted::ContextControlSet { .. }));
        recorded
            .pump(|view| {
                let snapshot = &view.snapshot;
                snapshot
                    .entry(snapshot.head().unwrap())
                    .is_some_and(|stored| {
                        matches!(&stored.entry.payload,
                        EntryPayload::ContextControl(control) if control.mode == mode)
                    })
            })
            .await;
        let screen = text(&render(&mut recorded.app(), 100, 60));
        assert!(screen.contains(marker), "{marker} missing: {screen}");
    }
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
    // M7-06: one compact header carries the status mark and the summary.
    assert!(screen.contains("✓ write note.txt"), "{screen}");
    assert!(!screen.contains("tool result:"), "{screen}");
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
    assert!(screen.contains("⊘ write note.txt"), "{screen}");
    assert!(screen.contains("approval: denied"), "{screen}");
    assert!(
        screen.contains("the write was denied"),
        "the next assistant reply must render: {screen}"
    );
    recorded.shutdown().await;
}

#[tokio::test]
async fn read_only_approval_allows_read_without_a_key_press() {
    let fixture = Fixture::new_read_ask();
    fs::write(fixture.workspace.join("note"), "hello").unwrap();
    let mut recorded = fixture
        .create([proposal("read", json!({"path": "note"})), reply("read it")])
        .await;
    let turn = recorded.start("read the note").await;
    let ticket = recorded.wait_approval().await;

    let mut app = recorded.app_with(true);
    let actions = app.take_actions();
    assert_eq!(actions.len(), 1, "{actions:?}");
    let Action::Command(Command::SubmitApproval {
        approval_id,
        plan_digest,
        decision,
        remember,
    }) = &actions[0]
    else {
        panic!("expected SubmitApproval, got {:?}", actions[0]);
    };
    // The ordinary allow is bound to the ticket and never remembered.
    assert_eq!(approval_id, &ticket.approval_id);
    assert_eq!(plan_digest, &ticket.plan_digest);
    assert_eq!(*decision, Decision::Allow);
    assert!(!*remember);
    assert!(app.take_actions().is_empty(), "one ticket, one decision");

    // No modal opens, and the transcript says the call was auto-approved.
    let screen = text(&render(&mut app, 100, 40));
    assert!(!screen.contains("approve tool call"), "{screen}");
    assert!(screen.contains("auto-approved as read-only"), "{screen}");

    // The shell submits it exactly like a key press; the journal keeps an
    // ordinary allow and the session policy gains no grant.
    recorded.submit(&ticket, Decision::Allow, false).await;
    recorded.wait_finished(&turn).await;
    let screen = text(&render(&mut recorded.app(), 100, 40));
    assert!(screen.contains("approval: allowed"), "{screen}");
    let configuration_ref = recorded.view.view().snapshot.configuration().clone();
    let configuration: Value = serde_json::from_slice(
        &fixture
            .root
            .read_artifact(&configuration_ref, 1024 * 1024)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(configuration["allowances"], json!([]));
    assert!(configuration.get("approve_read_only").is_none());
    recorded.shutdown().await;
}

#[tokio::test]
async fn read_only_approval_still_shows_the_modal_for_write() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture
        .create([
            proposal("write", json!({"path": "note.txt", "text": "hello"})),
            reply("wrote it"),
        ])
        .await;
    recorded.start("write a file").await;
    recorded.wait_approval().await;
    let mut app = recorded.app_with(true);
    assert!(
        app.take_actions().is_empty(),
        "write must never be auto-approved"
    );
    let screen = text(&render(&mut app, 80, 24));
    assert!(screen.contains("approve tool call"), "{screen}");
    assert!(screen.contains("a allow"), "{screen}");
    recorded.shutdown().await;
}

#[tokio::test]
async fn read_only_approval_off_leaves_every_read_ticket_to_the_modal() {
    let fixture = Fixture::new_read_ask();
    fs::write(fixture.workspace.join("note"), "hello").unwrap();
    let mut recorded = fixture
        .create([proposal("read", json!({"path": "note"})), reply("read it")])
        .await;
    recorded.start("read the note").await;
    recorded.wait_approval().await;
    let mut app = recorded.app_with(false);
    assert!(app.take_actions().is_empty());
    let screen = text(&render(&mut app, 80, 24));
    assert!(screen.contains("approve tool call"), "{screen}");
    assert!(!screen.contains("auto-approved"), "{screen}");
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
    // M7-06 item 5: the stable code and the provider's message are both shown.
    assert!(
        screen.contains("error rate_limit: too many requests"),
        "{screen}"
    );
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
async fn diagnostics_and_faults_render_with_their_codes() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture.create([reply("ok")]).await;
    let turn = recorded.start("hi").await;
    recorded.wait_finished(&turn).await;
    let mut app = recorded.app();
    // A fault needs a storage failure the pure model never causes; the events
    // continue the recorded view so the reducer and banner are still exercised.
    let view = app.view().expect("recorded view").clone();
    let epoch = view.epoch.clone();
    let seq = view.event_seq;
    app.on_subscription(SubscriptionItem::Event(RuntimeEvent {
        epoch: epoch.clone(),
        seq: seq + 1,
        kind: EventKind::Diagnostic(RuntimeDiagnostic::MalformedStream {
            inference: InferenceId(1),
            reason: "bad_block",
        }),
    }));
    app.on_subscription(SubscriptionItem::Event(RuntimeEvent {
        epoch,
        seq: seq + 2,
        kind: EventKind::Fault(FaultCause::Internal("invariant_broken")),
    }));
    let screen = text(&render(&mut app, 80, 24));
    assert!(
        screen.contains("diagnostic: malformed provider stream (bad_block)"),
        "{screen}"
    );
    assert!(screen.contains("fault: invariant_broken"), "{screen}");
    assert!(
        screen.contains("error: fault: invariant_broken"),
        "{screen}"
    );
    recorded.shutdown().await;
}

/// A recorded view with `awaiting` attached: resolution itself needs a crash
/// between intent and result, which the app-level tests do not simulate.
async fn awaiting_view(
    recorded: &Recorded,
    turn: &TurnId,
    entries: &[(&str, bool)],
) -> RuntimeView {
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
    view.phase = Phase::AwaitingResolution;
    view.awaiting = entries
        .iter()
        .map(|(tool, captured)| UncertainInvocation {
            invocation_id: InvocationId::new(),
            turn_id: turn.clone(),
            call: call.clone(),
            tool: (*tool).into(),
            plan_digest: Digest::of(b"plan"),
            intent_at: Timestamp::now(),
            hint: Some(harness_core::tools::RecoveryHint::MatchesPostImage),
            file_effect_required: *captured,
        })
        .collect();
    view
}

/// v0.2.1 review finding 2: a captured mutation is resolved by stating what
/// happened to the file, and that statement is what is sent.
#[tokio::test]
async fn uncertain_modal_states_a_captured_mutations_file_effect() {
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
    let view = awaiting_view(&recorded, &turn, &[("write", true)]).await;
    let invocation_id = view.awaiting[0].invocation_id.clone();
    let mut app = App::new(recorded.info.clone());
    app.on_subscription(SubscriptionItem::Reset(ClientView::from(view)));
    let screen = text(&render(&mut app, 80, 24));
    assert!(screen.contains("uncertain invocation"), "{screen}");
    assert!(
        screen.contains("observed: the file change was applied"),
        "{screen}"
    );
    assert!(
        screen.contains("observed: the file change was not applied"),
        "{screen}"
    );
    assert!(
        !screen.contains("observed: succeeded"),
        "a captured mutation is never resolved by status alone: {screen}"
    );

    // Observed applied: the user's file statement and the call's status.
    app.on_key(key(KeyCode::Right));
    type_text(&mut app, "the file has the new text");
    let actions = app.on_key(key(KeyCode::Enter));
    assert!(matches!(
        &actions[0],
        Action::Command(Command::ResolveUncertain {
            invocation_id: chosen,
            resolution: UserResolution::Observed {
                status: ToolStatus::Succeeded,
                text,
                file_effect: Some(FileEffect::Applied),
            },
        }) if chosen == &invocation_id && text == "the file has the new text"
    ));

    // Observed not applied: the same dialog, the other statement.
    app.on_key(key(KeyCode::Right));
    let actions = app.on_key(key(KeyCode::Enter));
    assert!(matches!(
        &actions[0],
        Action::Command(Command::ResolveUncertain {
            invocation_id: chosen,
            resolution: UserResolution::Observed {
                status: ToolStatus::Failed,
                file_effect: Some(FileEffect::NotApplied),
                ..
            },
        }) if chosen == &invocation_id
    ));
    recorded.shutdown().await;
}

/// Another call keeps the status choices and sends no file effect, and moving
/// between a captured and an uncaptured invocation never keeps an answer the
/// new question cannot express.
#[tokio::test]
async fn uncertain_modal_keeps_file_effect_none_for_other_calls() {
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
    let view = awaiting_view(&recorded, &turn, &[("write", true), ("bash", false)]).await;
    let selected = view.awaiting[1].invocation_id.clone();
    let mut app = App::new(recorded.info.clone());
    app.on_subscription(SubscriptionItem::Reset(ClientView::from(view)));
    // The captured call is selected first: switch to the executed bash call.
    app.on_key(key(KeyCode::Down));
    let screen = text(&render(&mut app, 80, 24));
    assert!(screen.contains("observed: succeeded"), "{screen}");
    assert!(
        !screen.contains("the file change was"),
        "an uncaptured call is never asked about a file: {screen}"
    );
    app.on_key(key(KeyCode::Right));
    type_text(&mut app, "the command ran");
    let actions = app.on_key(key(KeyCode::Enter));
    assert!(matches!(
        &actions[0],
        Action::Command(Command::ResolveUncertain {
            invocation_id: chosen,
            resolution: UserResolution::Observed {
                status: ToolStatus::Succeeded,
                text,
                file_effect: None,
            },
        }) if chosen == &selected && text == "the command ran"
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

/// Explicit, offline evidence capture; never writes during the ordinary gate.
#[tokio::test]
#[ignore = "writes UX evidence only when explicitly requested"]
async fn ux_render_evidence() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture
        .create([
            reply("Hello! What would you like to work on today?"),
            Script::new([
                Step::Event(ProviderEvent::Started),
                Step::text(
                    0,
                    "Working through the UI foundations, one careful step at a time.",
                ),
                Step::AwaitCancel,
                Step::cancelled(),
            ]),
            proposal(
                "write",
                json!({"path":"notes.txt", "text":"UX foundations"}),
            ),
            reply("done"),
        ])
        .await;
    let mut output = String::new();
    fn capture(output: &mut String, label: &str, app: &mut App) {
        for (w, h) in [(80, 24), (120, 36), (20, 5)] {
            if app.view().is_some_and(|v| v.active_turn.is_some()) {
                app.on_action(harness::tui::UiAction::Tick(Duration::ZERO));
                app.on_action(harness::tui::UiAction::Tick(Duration::from_millis(12_100)));
            }
            output.push_str(&format!("\n### {label} {w}×{h}\n```text\n"));
            output.push_str(&text(&render(app, w, h)));
            output.push_str("```\n");
        }
    }
    let turn = recorded.start("Hello").await;
    recorded.wait_finished(&turn).await;
    capture(&mut output, "idle", &mut recorded.app());
    let mut help = recorded.app();
    help.on_key(key(KeyCode::F(1)));
    capture(&mut output, "help", &mut help);
    let turn = recorded.start("Improve the TUI.").await;
    recorded
        .pump(|v| {
            v.partial.as_ref().is_some_and(|p| {
                p.blocks
                    .values()
                    .any(|b| matches!(b, PartialBlock::Text(t) if !t.is_empty()))
            })
        })
        .await;
    capture(&mut output, "streaming", &mut recorded.app());
    recorded
        .handle
        .command(Command::Cancel {
            turn_id: turn.clone(),
        })
        .await
        .unwrap();
    recorded.wait_finished(&turn).await;
    let turn = recorded.start("Write a note.").await;
    let ticket = recorded.wait_approval().await;
    capture(&mut output, "approval", &mut recorded.app());
    recorded.submit(&ticket, Decision::Deny, false).await;
    recorded.wait_finished(&turn).await;
    recorded.shutdown().await;
    let path = std::env::var("HARNESS_UX_EVIDENCE").expect("explicit evidence output path");
    fs::write(path, output).unwrap();
}

#[tokio::test]
async fn no_color_covers_transcript_prompt_errors_status_and_all_dialog_frames() {
    use harness::tui::theme::Theme;
    use ratatui::style::Color;
    let fixture = Fixture::new(false);
    let mut recorded = fixture
        .create([
            reply("# Header\n\n**bold** `code`\n\n```rust\nlet x = 1;\n```"),
            proposal("write", json!({"path":"note","text":"x"})),
            reply("done"),
        ])
        .await;
    let turn = recorded.start("Hello").await;
    recorded.wait_finished(&turn).await;
    let mut app = App::with_theme(recorded.info.clone(), Theme::new(true, false));
    for item in &recorded.items {
        app.on_subscription(item.clone());
    }
    fn check(app: &mut App) {
        let buffer = render(app, 120, 36);
        for cell in &buffer.content {
            assert_eq!(cell.fg, Color::Reset, "{cell:?}");
            assert_eq!(cell.bg, Color::Reset, "{cell:?}");
        }
    }
    app.set_error("sample error");
    check(&mut app);
    for code in [
        key(KeyCode::F(1)),
        key(KeyCode::Esc),
        modified(KeyCode::Char('o'), KeyModifiers::CONTROL),
        key(KeyCode::Esc),
        modified(KeyCode::Char('t'), KeyModifiers::CONTROL),
        key(KeyCode::Esc),
        modified(KeyCode::Char('l'), KeyModifiers::CONTROL),
        key(KeyCode::Esc),
        modified(KeyCode::Char('k'), KeyModifiers::CONTROL),
        key(KeyCode::Esc),
        modified(KeyCode::Char('c'), KeyModifiers::CONTROL),
        key(KeyCode::Esc),
    ] {
        app.on_key(code);
        check(&mut app);
    }
    let at = recorded.items.len();
    let turn = recorded.start("tool").await;
    let ticket = recorded.wait_approval().await;
    for item in &recorded.items[at..] {
        app.on_subscription(item.clone());
    }
    check(&mut app);
    recorded.submit(&ticket, Decision::Deny, false).await;
    recorded.wait_finished(&turn).await;
    recorded.shutdown().await;
}

#[tokio::test]
async fn activity_spinner_elapsed_and_stream_size_use_only_injected_clock() {
    use harness::tui::{UiAction, theme::Theme};
    let fixture = Fixture::new(false);
    let mut recorded = fixture
        .create([Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, "partial answer"),
            Step::AwaitCancel,
            Step::cancelled(),
        ])])
        .await;
    let turn = recorded.start("work").await;
    recorded
        .pump(|v| {
            v.partial.as_ref().is_some_and(|p| {
                p.blocks
                    .values()
                    .any(|b| matches!(b,PartialBlock::Text(t) if !t.is_empty()))
            })
        })
        .await;
    let mut app = recorded.app();
    app.on_action(UiAction::Tick(Duration::from_secs(5)));
    app.on_action(UiAction::Tick(Duration::from_millis(17_100)));
    let screen = text(&render(&mut app, 200, 60));
    assert!(
        screen.contains("⠙ inferring 12s") && screen.contains("14 B"),
        "{screen}"
    );
    let unchanged = text(&render(&mut app, 200, 60));
    assert_eq!(screen, unchanged, "rendering cannot advance time");
    let mut ascii = App::with_theme(recorded.info.clone(), Theme::new(false, true));
    for item in &recorded.items {
        ascii.on_subscription(item.clone());
    }
    ascii.on_action(UiAction::Tick(Duration::from_secs(5)));
    ascii.on_action(UiAction::Tick(Duration::from_millis(17_100)));
    assert!(text(&render(&mut ascii, 200, 60)).contains("\\ inferring 12s"));
    recorded
        .handle
        .command(Command::Cancel {
            turn_id: turn.clone(),
        })
        .await
        .unwrap();
    recorded.wait_finished(&turn).await;
    recorded.shutdown().await;
}

#[tokio::test]
async fn prompt_history_seeds_current_branch_restores_draft_and_keeps_sent_prompts() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture.create([reply("one"), reply("two")]).await;
    let first = recorded.start("first prompt").await;
    recorded.wait_finished(&first).await;
    let first_head = recorded.view.view().snapshot.head().cloned();
    let second = recorded.start("second prompt").await;
    recorded.wait_finished(&second).await;
    let mut app = recorded.app();
    type_text(&mut app, "draft");
    app.on_key(key(KeyCode::Up));
    assert!(text(&render(&mut app, 80, 24)).contains("> second prompt"));
    app.on_key(key(KeyCode::Up));
    assert!(text(&render(&mut app, 80, 24)).contains("> first prompt"));
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Down));
    assert!(text(&render(&mut app, 80, 24)).contains("> draft"));
    app.on_key(key(KeyCode::Enter));
    app.on_key(key(KeyCode::Up));
    assert!(text(&render(&mut app, 80, 24)).contains("> draft"));
    // Reopened/historical branch history excludes later sibling messages.
    recorded
        .handle
        .command(Command::SelectHead { head: first_head })
        .await
        .unwrap();
    recorded.record_reset().await;
    let mut branch = App::new(recorded.info.clone());
    branch.on_subscription(recorded.items.last().unwrap().clone());
    branch.on_key(key(KeyCode::Up));
    assert!(text(&render(&mut branch, 80, 24)).contains("> first prompt"));
    recorded.shutdown().await;
}

#[tokio::test]
async fn word_editing_and_atomic_large_paste_remain_editable() {
    let fixture = Fixture::new(false);
    let recorded = fixture.create([]).await;
    let mut app = recorded.app();
    app.on_paste("one two 界 three");
    app.on_key(modified(KeyCode::Left, KeyModifiers::CONTROL));
    app.on_key(modified(KeyCode::Char('w'), KeyModifiers::CONTROL));
    app.on_key(modified(KeyCode::Right, KeyModifiers::CONTROL));
    app.on_key(modified(KeyCode::Char('u'), KeyModifiers::CONTROL));
    type_text(&mut app, "kept");
    let actions = app.on_key(key(KeyCode::Enter));
    assert!(
        matches!(actions.as_slice(),[Action::Command(Command::StartTurn{content,..})] if content==&vec![ContentBlock::Text{text:"kept".into()}]),
        "{actions:?}"
    );
    app.on_paste(&["line"; 21].join("\n"));
    let screen = text(&render(&mut app, 80, 24));
    assert!(screen.contains("[pasted 21 lines]"), "{screen}");
    app.on_key(key(KeyCode::Backspace));
    let actions = app.on_key(key(KeyCode::Enter));
    assert!(
        matches!(actions.as_slice(),[Action::Command(Command::StartTurn{content,..})] if matches!(&content[0],ContentBlock::Text{text} if text.lines().count()==21 && text.ends_with("lin")))
    );
    recorded.shutdown().await;
}

#[tokio::test]
async fn help_scrolls_with_more_indicator_and_empty_question_mark_only() {
    let fixture = Fixture::new(false);
    let recorded = fixture.create([]).await;
    let mut app = recorded.app();
    app.on_key(key(KeyCode::Char('?')));
    let before = text(&render(&mut app, 80, 24));
    assert!(
        before.contains("key reference") && before.contains("Editing") && before.contains("more ↓"),
        "{before}"
    );
    app.on_key(key(KeyCode::PageDown));
    let after = text(&render(&mut app, 80, 24));
    assert_ne!(before, after);
    for _ in 0..10 {
        app.on_key(key(KeyCode::PageDown));
        render(&mut app, 80, 24);
    }
    assert!(text(&render(&mut app, 80, 24)).contains("Dialogs"));
    app.on_key(key(KeyCode::Esc));
    type_text(&mut app, "why?");
    let actions = app.on_key(key(KeyCode::Enter));
    assert!(
        matches!(actions.as_slice(),[Action::Command(Command::StartTurn{content,..})] if content==&vec![ContentBlock::Text{text:"why?".into()}])
    );
    recorded.shutdown().await;
}

#[tokio::test]
async fn idle_error_is_wrapped_bounded_and_dismissed_welcome_disappears_after_send() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture.create([reply("welcome gone")]).await;
    let mut app = recorded.app();
    let welcome = text(&render(&mut app, 80, 24));
    for expected in ["Welcome to harness", "Workspace:", "Model:", "F1 help"] {
        assert!(welcome.contains(expected), "{welcome}");
    }
    app.set_error("error words ".repeat(40));
    let banner = text(&render(&mut app, 40, 24));
    assert!(
        banner.contains("error: error words") && banner.contains('…'),
        "{banner}"
    );
    app.on_key(key(KeyCode::Esc));
    assert!(!text(&render(&mut app, 80, 24)).contains("error:"));
    app.set_error("clear on send");
    app.on_paste("hello");
    app.on_key(key(KeyCode::Enter));
    assert!(!text(&render(&mut app, 80, 24)).contains("clear on send"));
    let turn = recorded.start("hello").await;
    recorded.wait_finished(&turn).await;
    let screen = text(&render(&mut recorded.app(), 200, 60));
    assert!(!screen.contains("Welcome to harness"));
    render(&mut recorded.app(), 20, 5);
    recorded.shutdown().await;
}

#[tokio::test]
async fn hello_conversation_has_gutters_metadata_full_status_and_real_cursor() {
    use harness::tui::UiAction;
    use ratatui::{
        backend::Backend,
        style::{Color, Modifier},
    };
    let fixture = Fixture::new(false);
    let (release, gate) = tokio::sync::oneshot::channel();
    let mut recorded = fixture
        .create([Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, "Hello! What would you like to work on today?"),
            Step::Event(ProviderEvent::UsageUpdated {
                usage: Usage {
                    input_tokens: Some(569),
                    output_tokens: Some(15),
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                },
            }),
            Step::Wait(gate),
            Step::completed(FinishReason::Stop),
        ])])
        .await;
    let turn = recorded.start("Hello").await;
    recorded
        .pump(|v| v.partial.as_ref().is_some_and(|p| p.usage.is_some()))
        .await;
    let mut app = recorded.app();
    app.on_action(UiAction::Tick(Duration::ZERO));
    app.on_action(UiAction::Tick(Duration::from_millis(1200)));
    let consumed = recorded.items.len();
    release.send(()).unwrap();
    recorded.wait_finished(&turn).await;
    for item in &recorded.items[consumed..] {
        app.on_subscription(item.clone());
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    let buffer = terminal.backend().buffer();
    let screen = text(buffer);
    assert!(
        !screen.contains("turn: completed") && !screen.contains('▏'),
        "{screen}"
    );
    for row in [
        "▌ you",
        "▌ Hello",
        "▌ assistant",
        "▌ Hello! What would you like to work on today?",
        "scripted · 1.2s · 569 in / 15 out",
        "> Ask anything · F1 help",
        " message ─",
        "harness · workspace · session",
        "idle · context",
    ] {
        assert!(screen.contains(row), "missing {row}: {screen}");
    }
    let user = screen.lines().position(|l| l.starts_with("▌ you")).unwrap() as u16;
    let assistant = screen
        .lines()
        .position(|l| l.starts_with("▌ assistant"))
        .unwrap() as u16;
    assert_eq!(buffer[(0, user)].fg, Color::Cyan);
    assert_eq!(buffer[(2, user)].fg, Color::Cyan);
    assert_eq!(buffer[(0, assistant)].fg, Color::Green);
    assert_eq!(buffer[(2, assistant)].fg, Color::Green);
    assert!(buffer[(2, user)].modifier.contains(Modifier::BOLD));
    assert!(buffer[(79, assistant)].modifier.contains(Modifier::DIM));
    assert!(buffer[(0, 0)].modifier.contains(Modifier::DIM));
    for x in 0..80 {
        assert!(buffer[(x, 23)].modifier.contains(Modifier::REVERSED));
    }
    assert_eq!(
        terminal.backend_mut().get_cursor_position().unwrap(),
        (2, 21).into()
    );
    let tiny = text(&render(&mut app, 20, 5));
    assert!(
        tiny.contains("today?"),
        "tail padding hid the message: {tiny}"
    );
    app.on_paste("界e\u{301}");
    terminal.draw(|f| app.render(f)).unwrap();
    assert_eq!(
        terminal.backend_mut().get_cursor_position().unwrap(),
        (5, 21).into()
    );
    app.on_paste(&"x".repeat(78));
    terminal.draw(|f| app.render(f)).unwrap();
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    assert!(cursor.x < 80 && cursor.y < 24);
    render(&mut app, 20, 5);
    render(&mut app, 200, 60);
    recorded.shutdown().await;
}

#[tokio::test]
async fn follow_mode_preserves_absolute_rows_and_counts_new_output_until_end() {
    let fixture = Fixture::new(false);
    let (release, gate) = tokio::sync::oneshot::channel();
    let body = (0..60)
        .map(|n| format!("row {n:02} visible paragraph"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut recorded = fixture
        .create([Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, &body),
            Step::Wait(gate),
            Step::text(0, "\n\nnew output arrives\n\nmore output arrives"),
            Step::AwaitCancel,
            Step::cancelled(),
        ])])
        .await;
    let turn = recorded.start("scroll").await;
    recorded
        .pump(|v| {
            v.partial.as_ref().is_some_and(|p| {
                p.blocks
                    .values()
                    .any(|b| matches!(b,PartialBlock::Text(t) if t.contains("row 59")))
            })
        })
        .await;
    let mut app = recorded.app();
    render(&mut app, 80, 24);
    app.on_key(key(KeyCode::PageUp));
    let before = text(&render(&mut app, 80, 24));
    let consumed = recorded.items.len();
    release.send(()).unwrap();
    recorded
        .pump(|v| {
            v.partial.as_ref().is_some_and(|p| {
                p.blocks
                    .values()
                    .any(|b| matches!(b,PartialBlock::Text(t) if t.contains("more output arrives")))
            })
        })
        .await;
    for item in &recorded.items[consumed..] {
        app.on_subscription(item.clone());
    }
    let after = text(&render(&mut app, 80, 24));
    assert_eq!(
        before.lines().take(17).collect::<Vec<_>>(),
        after.lines().take(17).collect::<Vec<_>>(),
        "scrolled-up anchor moved"
    );
    assert!(after.contains("↓ 4 new lines · End to follow"), "{after}");
    app.on_key(key(KeyCode::End));
    let followed = text(&render(&mut app, 80, 24));
    assert!(
        followed.contains("more output arrives") && followed.contains("100% · following"),
        "{followed}"
    );
    app.on_paste("draft");
    app.on_key(key(KeyCode::PageUp));
    render(&mut app, 80, 24);
    app.on_key(modified(KeyCode::End, KeyModifiers::CONTROL));
    assert!(text(&render(&mut app, 80, 24)).contains("100% · following"));
    app.set_error("active error");
    assert!(matches!(
        app.on_key(key(KeyCode::Esc)).as_slice(),
        [Action::Command(Command::Cancel { .. })]
    ));
    recorded
        .handle
        .command(Command::Cancel {
            turn_id: turn.clone(),
        })
        .await
        .unwrap();
    recorded.wait_finished(&turn).await;
    recorded.shutdown().await;
}

// ---------------------------------------------------------------------------
// M7-06: content rendering. Markdown, code, tool blocks and approval content.
// ---------------------------------------------------------------------------

/// The transcript only: no header, no prompt rule, no status bar.
fn body(screen: &str) -> String {
    let mut out = Vec::new();
    for line in screen.lines() {
        if line.starts_with(" message ") || line.starts_with(" approval ") {
            break;
        }
        if line.starts_with(" harness · ") {
            continue;
        }
        out.push(line.trim_end().to_owned());
    }
    while out.last().is_some_and(|line| line.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

const KITCHEN_SINK: &str = "\
# Title

Intro with **strong**, *emphasis*, ~~struck~~ and `inline code`.
A [link](https://example.com/docs) and an autolink <https://example.com>.

## Second

1. first
2. second
   - nested bullet
     - deeper bullet
- [x] done task
- [ ] open task

> quoted line
> > nested quote

---

| left | center | right |
| :--- | :----: | ----: |
| a | b | 1 |
| longer | cell | 22 |

![alt text](https://example.com/image.png)

<div>raw html</div>

A footnote reference.[^note]

```rust
fn main() {
    let x = 1;
}
```

```python
def greet(name):
    return f\"hi {name}\"
```

```
untagged
```

[^note]: The footnote body.
";

#[tokio::test]
async fn kitchen_sink_markdown_renders_the_same_projection_at_80_and_120_columns() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture.create([reply(KITCHEN_SINK)]).await;
    let turn = recorded.start("render").await;
    recorded.wait_finished(&turn).await;
    let mut app = recorded.app();
    let narrow = body(&text(&render(&mut app, 80, 200)));
    let wide = body(&text(&render(&mut app, 120, 200)));
    // Every element of item 1, in order, at 80 columns.
    assert_eq!(narrow, KITCHEN_SINK_GOLDEN_80);
    // The same projection only wraps differently at 120 columns.
    assert_eq!(wide, KITCHEN_SINK_GOLDEN_120);
    // Typography carries the structure a text golden cannot show.
    let buffer = render(&mut app, 120, 200);
    use ratatui::style::{Color, Modifier};
    let heading = cells_at(&buffer, "Title");
    assert!(
        heading.iter().all(|(_, modifier, color)| {
            modifier.contains(Modifier::BOLD)
                && modifier.contains(Modifier::UNDERLINED)
                && *color == Color::Magenta
        }),
        "H1 is bold, accent and underlined: {heading:?}"
    );
    let strong = cells_at(&buffer, "strong");
    assert!(
        strong
            .iter()
            .all(|(_, modifier, _)| modifier.contains(Modifier::BOLD)
                && !modifier.contains(Modifier::ITALIC)),
        "{strong:?}"
    );
    let emphasis = cells_at(&buffer, "emphasis");
    assert!(
        emphasis
            .iter()
            .all(|(_, modifier, _)| modifier.contains(Modifier::ITALIC)),
        "{emphasis:?}"
    );
    let struck = cells_at(&buffer, "struck");
    assert!(
        struck
            .iter()
            .all(|(_, modifier, _)| modifier.contains(Modifier::CROSSED_OUT)),
        "{struck:?}"
    );
    let link = cells_at(&buffer, "link");
    assert!(
        link.iter()
            .all(|(_, modifier, _)| modifier.contains(Modifier::UNDERLINED)),
        "{link:?}"
    );
    let inline = cells_at(&buffer, "inline code");
    assert!(
        inline
            .iter()
            .all(|(_, modifier, color)| *color == Color::Yellow
                && modifier.contains(Modifier::DIM)),
        "{inline:?}"
    );
    // The Rust block is highlighted with ANSI roles only.
    let keyword = cells_at(&buffer, "let x");
    assert_eq!(keyword[0].2, Color::Magenta, "`let` is a keyword");
    let number = cells_at(&buffer, "1;");
    assert!(
        number.iter().any(|(_, _, color)| *color == Color::Cyan),
        "the number is highlighted: {number:?}"
    );
    recorded.shutdown().await;
}

/// The cells of the first row containing `needle`, from its first glyph.
fn cells_at(
    buffer: &Buffer,
    needle: &str,
) -> Vec<(String, ratatui::style::Modifier, ratatui::style::Color)> {
    for y in 0..buffer.area.height {
        let line: String = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        if let Some(byte) = line.find(needle) {
            let column = line[..byte].chars().count();
            return (column..column + needle.chars().count())
                .map(|x| {
                    let cell = &buffer[(x as u16, y)];
                    (cell.symbol().to_owned(), cell.modifier, cell.fg)
                })
                .collect();
        }
    }
    panic!("no row contains {needle:?}");
}

const KITCHEN_SINK_GOLDEN_80: &str = r#"▌ you
▌ render

▌ assistant                                                             scripted
▌ Title

▌ Intro with strong, emphasis, struck and inline code. A link
▌ (https://example.com/docs) and an autolink https://example.com.

▌ Second

▌ 1. first
▌ 2. second
▌   ◦ nested bullet
▌     ▪ deeper bullet

▌ • [x] done task
▌ • [ ] open task

▌ │ quoted line

▌ │ │ nested quote

────────────────────────────────────────────────────────────────────────────────

▌ left   │ center │ right
▌ ───────┼────────┼──────
▌ a      │   b    │     1
▌ longer │  cell  │    22

▌ [image: alt text]

▌ <div>raw html</div> A footnote reference.[^1]

▌ rust
▌ │ fn main() {
▌ │     let x = 1;
▌ │ }

▌ python
▌ │ def greet(name):
▌ │     return f"hi {name}"

▌ code
▌ │ untagged

▌ [^1] The footnote body."#;
const KITCHEN_SINK_GOLDEN_120: &str = r#"▌ you
▌ render

▌ assistant                                                                                                     scripted
▌ Title

▌ Intro with strong, emphasis, struck and inline code. A link (https://example.com/docs) and an autolink
▌ https://example.com.

▌ Second

▌ 1. first
▌ 2. second
▌   ◦ nested bullet
▌     ▪ deeper bullet

▌ • [x] done task
▌ • [ ] open task

▌ │ quoted line

▌ │ │ nested quote

────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────

▌ left   │ center │ right
▌ ───────┼────────┼──────
▌ a      │   b    │     1
▌ longer │  cell  │    22

▌ [image: alt text]

▌ <div>raw html</div> A footnote reference.[^1]

▌ rust
▌ │ fn main() {
▌ │     let x = 1;
▌ │ }

▌ python
▌ │ def greet(name):
▌ │     return f"hi {name}"

▌ code
▌ │ untagged

▌ [^1] The footnote body."#;

#[tokio::test]
async fn no_color_has_no_colour_and_ascii_mode_has_no_wide_glyphs() {
    use harness::tui::theme::Theme;
    use ratatui::style::Color;
    let fixture = Fixture::new(false);
    let mut recorded = fixture.create([reply(KITCHEN_SINK)]).await;
    let turn = recorded.start("render").await;
    recorded.wait_finished(&turn).await;
    let mut plain = App::with_theme(recorded.info.clone(), Theme::new(true, false));
    for item in &recorded.items {
        plain.on_subscription(item.clone());
    }
    let buffer = render(&mut plain, 120, 200);
    for cell in &buffer.content {
        assert_eq!(cell.fg, Color::Reset, "{cell:?}");
        assert_eq!(cell.bg, Color::Reset, "{cell:?}");
    }
    // Colour on, ASCII mode: every glyph is ASCII and every colour is one of
    // the 16 ANSI variants the theme declares.
    let mut ascii = App::with_theme(recorded.info.clone(), Theme::new(false, true));
    for item in &recorded.items {
        ascii.on_subscription(item.clone());
    }
    let buffer = render(&mut ascii, 120, 200);
    for cell in &buffer.content {
        assert!(cell.symbol().is_ascii(), "non-ASCII glyph: {cell:?}");
        assert!(
            matches!(
                cell.fg,
                Color::Reset
                    | Color::Black
                    | Color::Red
                    | Color::Green
                    | Color::Yellow
                    | Color::Blue
                    | Color::Magenta
                    | Color::Cyan
                    | Color::Gray
                    | Color::DarkGray
                    | Color::LightRed
                    | Color::LightGreen
                    | Color::LightYellow
                    | Color::LightBlue
                    | Color::LightMagenta
                    | Color::LightCyan
                    | Color::White
            ),
            "non-ANSI colour: {cell:?}"
        );
    }
    recorded.shutdown().await;
}

#[tokio::test]
async fn user_text_keeps_indentation_and_wraps_long_lines_without_truncating() {
    let fixture = Fixture::new(false);
    let long = "x".repeat(1_000);
    let mut recorded = fixture.create([reply("ok")]).await;
    let text_in = format!("    indented first\n        deeply indented\n{long}");
    let turn = recorded.start(&text_in).await;
    recorded.wait_finished(&turn).await;
    let mut app = recorded.app();
    let screen = body(&text(&render(&mut app, 80, 200)));
    assert!(screen.contains("    indented first"), "{screen}");
    assert!(screen.contains("        deeply indented"), "{screen}");
    let xes = screen.chars().filter(|c| *c == 'x').count();
    assert_eq!(xes, 1_000, "the long line must not be truncated: {screen}");
    assert!(
        screen.lines().all(|line| line.chars().count() <= 80),
        "every physical row fits the pane: {screen}"
    );
    // A fenced block inside a user message is a code block, not Markdown.
    let fenced = "before\n```sh\ncargo test\n```\nafter";
    let fixture = Fixture::new(false);
    let mut recorded = fixture.create([reply("ok")]).await;
    let turn = recorded.start(fenced).await;
    recorded.wait_finished(&turn).await;
    let mut app = recorded.app();
    let screen = body(&text(&render(&mut app, 80, 200)));
    assert!(screen.contains("before"), "{screen}");
    assert!(screen.contains("▌ │ cargo test"), "{screen}");
    assert!(screen.contains("after"), "{screen}");
    assert!(!screen.contains("```"), "{screen}");
    recorded.shutdown().await;
}

#[tokio::test]
async fn search_blocks_expand_with_ctrl_e_at_80_columns() {
    use harness::tui::theme::Theme;
    let fixture = Fixture::new(false);
    for name in ["a.rs", "b.rs", "c.rs", "d.rs"] {
        fs::write(fixture.workspace.join(name), "hit\n").unwrap();
    }
    let mut recorded = fixture
        .create([
            proposal("list", json!({})),
            proposal("glob", json!({"pattern":"*.rs"})),
            proposal("grep", json!({"pattern":"hit","max_results":1})),
            reply("done"),
        ])
        .await;
    let turn = recorded.start("find code").await;
    recorded.wait_finished(&turn).await;
    for ascii in [false, true] {
        let mut app = App::with_theme(recorded.info.clone(), Theme::new(true, ascii));
        for item in &recorded.items {
            app.on_subscription(item.clone());
        }
        let screen = body(&text(&render(&mut app, 80, 100)));
        for expected in [
            "list . (4 results)",
            "glob *.rs (4 results)",
            "grep /hit/ . (1 results; truncated)",
            "Ctrl-E for full output",
        ] {
            assert!(screen.contains(expected), "{screen}");
        }
        assert!(!screen.contains("file d.rs"), "{screen}");
        app.on_key(modified(KeyCode::Char('e'), KeyModifiers::CONTROL));
        let buffer = render(&mut app, 80, 100);
        let expanded = text(&buffer);
        assert!(expanded.contains("file d.rs (4 B)"), "{expanded}");
        assert!(expanded.contains("a.rs:1: hit"), "{expanded}");
        if ascii {
            assert!(expanded.is_ascii());
        }
        assert!(
            buffer
                .content
                .iter()
                .all(|cell| cell.fg == ratatui::style::Color::Reset
                    && cell.bg == ratatui::style::Color::Reset)
        );
    }
    recorded.shutdown().await;
}

#[tokio::test]
async fn tool_headers_diffs_and_output_collapse_follow_the_toggles() {
    let fixture = Fixture::new(false);
    fs::write(
        fixture.workspace.join("notes.txt"),
        "line one\nline two\nline three\nline four\nline five\nline six\n",
    )
    .unwrap();
    let mut recorded = fixture
        .create([
            proposal("read", json!({"path": "notes.txt"})),
            reply("read it"),
            proposal(
                "edit",
                json!({"path": "notes.txt", "old_text": "line two", "new_text": "line 2"}),
            ),
        ])
        .await;
    let turn = recorded.start("read the notes").await;
    recorded.wait_finished(&turn).await;
    let mut app = recorded.app();
    let screen = body(&text(&render(&mut app, 80, 200)));
    assert!(
        screen.contains("✓ read notes.txt (bytes 0-1048576)"),
        "{screen}"
    );
    // Collapsed: the tool's own preamble plus the first two file lines.
    assert!(screen.contains("│ line one"), "{screen}");
    assert!(screen.contains("│ line two"), "{screen}");
    assert!(!screen.contains("│ line three"), "{screen}");
    assert!(
        screen.contains("… +4 lines (Ctrl-E for full output)"),
        "{screen}"
    );
    // One global switch expands every tool output.
    app.on_key(modified(KeyCode::Char('e'), KeyModifiers::CONTROL));
    assert!(app.full_output());
    let expanded = body(&text(&render(&mut app, 80, 200)));
    assert!(expanded.contains("│ line six"), "{expanded}");
    assert!(!expanded.contains("+4 lines"), "{expanded}");
    app.on_key(modified(KeyCode::Char('e'), KeyModifiers::CONTROL));
    assert!(!app.full_output());
    // The edit call needs an approval; its diff is in the modal already.
    let at = recorded.items.len();
    let turn = recorded.start("rename that line").await;
    let ticket = recorded.wait_approval().await;
    for item in &recorded.items[at..] {
        app.on_subscription(item.clone());
    }
    let seen = recorded.items.len();
    let modal = text(&render(&mut app, 80, 40));
    assert!(modal.contains("approve tool call"), "{modal}");
    assert!(modal.contains("path notes.txt"), "{modal}");
    assert!(modal.contains("- line two"), "{modal}");
    assert!(modal.contains("+ line 2"), "{modal}");
    recorded.submit(&ticket, Decision::Deny, false).await;
    recorded.wait_finished(&turn).await;
    for item in &recorded.items[seen..] {
        app.on_subscription(item.clone());
    }
    let screen = body(&text(&render(&mut app, 80, 200)));
    assert!(screen.contains("⊘ edit notes.txt"), "{screen}");
    assert!(screen.contains("approval: denied"), "{screen}");
    recorded.shutdown().await;
}

#[tokio::test]
async fn approval_modal_previews_write_and_toggles_raw_arguments() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture
        .create([proposal(
            "write",
            json!({"path": "out.txt", "text": "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven"}),
        )])
        .await;
    let turn = recorded.start("write it").await;
    let ticket = recorded.wait_approval().await;
    let mut app = recorded.app();
    let preview = text(&render(&mut app, 100, 60));
    assert!(preview.contains("approve tool call"), "{preview}");
    assert!(preview.contains("path out.txt"), "{preview}");
    assert!(preview.contains("size "), "{preview}");
    assert!(preview.contains("│ one"), "{preview}");
    assert!(preview.contains("│ ten"), "{preview}");
    assert!(!preview.contains("│ eleven"), "{preview}");
    assert!(preview.contains("+1 lines"), "{preview}");
    assert!(preview.contains("v raw"), "{preview}");
    assert!(preview.contains("plan "), "{preview}");
    // `v` switches to raw JSON and back without answering the ticket.
    assert!(app.on_key(key(KeyCode::Char('v'))).is_empty());
    let raw = text(&render(&mut app, 100, 60));
    assert!(raw.contains("\"text\": \"one\\ntwo"), "{raw}");
    assert!(!raw.contains("size "), "{raw}");
    assert!(app.on_key(key(KeyCode::Char('v'))).is_empty());
    assert!(
        text(&render(&mut app, 100, 60)).contains("size "),
        "readable again"
    );
    // The human-readable body never repeats the key hints.
    let hints = text(&render(&mut app, 100, 60))
        .lines()
        .filter(|line| line.contains("a allow"))
        .count();
    assert_eq!(hints, 2, "frame and footer only: {hints}");
    recorded.submit(&ticket, Decision::Deny, false).await;
    recorded.wait_finished(&turn).await;
    recorded.shutdown().await;
}

#[tokio::test]
async fn streaming_unclosed_fence_renders_as_code_without_flicker() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture
        .create([Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(
                0,
                "Here is the code:\n\n```rust\nfn main() {\n    println!(\"hi\");\n",
            ),
            Step::AwaitCancel,
            Step::cancelled(),
        ])])
        .await;
    let turn = recorded.start("show code").await;
    recorded
        .pump(|view| {
            view.partial.as_ref().is_some_and(|partial| {
                partial.blocks.values().any(
                    |block| matches!(block, PartialBlock::Text(text) if text.contains("fn main")),
                )
            })
        })
        .await;
    let mut app = recorded.app();
    let first = body(&text(&render(&mut app, 80, 200)));
    assert!(first.contains("▌ rust"), "{first}");
    assert!(first.contains("│ fn main() {"), "{first}");
    assert!(!first.contains("```"), "{first}");
    // Re-rendering the same partial is stable: no flicker between frames.
    let second = body(&text(&render(&mut app, 80, 200)));
    assert_eq!(first, second);
    recorded
        .handle
        .command(Command::Cancel {
            turn_id: turn.clone(),
        })
        .await
        .unwrap();
    recorded.wait_finished(&turn).await;
    recorded.shutdown().await;
}

#[tokio::test]
async fn completed_entries_render_once_and_only_the_partial_is_reparsed() {
    let fixture = Fixture::new(false);
    let (release, wait) = tokio::sync::oneshot::channel();
    let mut recorded = fixture
        .create([Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, "first "),
            Step::Wait(wait),
            Step::text(0, "second"),
            Step::completed(FinishReason::Stop),
        ])])
        .await;
    let turn = recorded.start("stream").await;
    recorded
        .pump(|view| {
            view.partial.as_ref().is_some_and(|partial| {
                partial.blocks.values().any(
                    |block| matches!(block, PartialBlock::Text(text) if text.contains("first")),
                )
            })
        })
        .await;
    let seen = recorded.items.len();
    let mut app = recorded.app();
    render(&mut app, 80, 40);
    let after_one = app.transcript_parses();
    assert_eq!(after_one, 1, "one completed user entry is rendered once");
    for _ in 0..5 {
        // Identical frames must not re-render the completed entry.
        render(&mut app, 80, 40);
        assert_eq!(app.transcript_parses(), after_one);
    }
    release.send(()).unwrap();
    recorded.wait_finished(&turn).await;
    for item in &recorded.items[seen..] {
        app.on_subscription(item.clone());
    }
    let screen = body(&text(&render(&mut app, 80, 60)));
    assert!(screen.contains("first second"), "{screen}");
    // The user entry is cached and only the newly committed assistant entry is
    // rendered, so the count is exactly three (user, assistant, outcome-free).
    assert_eq!(app.transcript_parses(), 2, "{screen}");
    // A toggle invalidates the cache once, then caches again.
    app.on_key(modified(KeyCode::Char('e'), KeyModifiers::CONTROL));
    render(&mut app, 80, 60);
    let after_toggle = app.transcript_parses();
    assert_eq!(after_toggle, 4);
    render(&mut app, 80, 60);
    assert_eq!(app.transcript_parses(), after_toggle);
    recorded.shutdown().await;
}

#[tokio::test]
async fn a_huge_code_block_renders_plain_with_one_cap_note() {
    let fixture = Fixture::new(false);
    let mut script = String::from("```rust\n");
    for n in 0..2_100 {
        script.push_str(&format!("let value_{n} = {n};\n"));
    }
    script.push_str("```\n");
    let mut recorded = fixture.create([reply(&script)]).await;
    let turn = recorded.start("big block").await;
    recorded.wait_finished(&turn).await;
    let mut app = recorded.app();
    // The cap note is the last transcript row, so a tall pane shows it.
    let screen = text(&render(&mut app, 120, 2_200));
    assert!(
        screen.contains("highlighting skipped: 2100 lines"),
        "cap note missing"
    );
    assert_eq!(screen.matches("highlighting skipped").count(), 1);
    recorded.shutdown().await;
}

/// Handoff evidence only: writes `TestBackend` dumps to an explicit path.
#[tokio::test]
#[ignore = "writes M7-06 render evidence only when explicitly requested"]
async fn m7_06_render_evidence() {
    fn capture(output: &mut String, label: &str, app: &mut App) {
        for (width, height) in [(80, 24), (120, 36)] {
            output.push_str(&format!("\n### {label} {width}×{height}\n```text\n"));
            output.push_str(&text(&render(app, width, height)));
            output.push_str("```\n");
        }
    }

    let fixture = Fixture::new(false);
    let mut output = String::new();
    let mut recorded = fixture.create([reply(KITCHEN_SINK)]).await;
    let turn = recorded.start("render the kitchen sink").await;
    recorded.wait_finished(&turn).await;
    capture(&mut output, "kitchen sink GFM", &mut recorded.app());
    recorded.shutdown().await;

    let fixture = Fixture::new(false);
    fs::write(
        fixture.workspace.join("notes.txt"),
        "alpha\nbeta\ngamma\ndelta\nepsilon\n",
    )
    .unwrap();
    let mut recorded = fixture
        .create([
            proposal("read", json!({"path": "notes.txt"})),
            proposal("bash", json!({"command": "cargo test", "cwd": "."})),
            proposal("read", json!({"path": "missing.txt"})),
            proposal(
                "edit",
                json!({"path": "notes.txt", "old_text": "beta", "new_text": "BETA"}),
            ),
            reply("done"),
        ])
        .await;
    let turn = recorded.start("show the tool blocks").await;
    let ticket = recorded.wait_approval().await;
    capture(
        &mut output,
        "approval modal for an edit",
        &mut recorded.app(),
    );
    recorded.submit(&ticket, Decision::Deny, false).await;
    recorded.wait_finished(&turn).await;
    capture(
        &mut output,
        "tool conversation (ok, denied, failed, diff, collapsed output)",
        &mut recorded.app(),
    );
    recorded.shutdown().await;

    let fixture = Fixture::new(false);
    let mut recorded = fixture
        .create([Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(
                0,
                "Here is the patch:\n\n```rust\nfn main() {\n    println!(\"streaming\");\n",
            ),
            Step::AwaitCancel,
            Step::cancelled(),
        ])])
        .await;
    let turn = recorded.start("stream it").await;
    recorded
        .pump(|view| {
            view.partial.as_ref().is_some_and(|partial| {
                partial.blocks.values().any(
                    |block| matches!(block, PartialBlock::Text(text) if text.contains("println")),
                )
            })
        })
        .await;
    capture(
        &mut output,
        "streaming with an unclosed fence",
        &mut recorded.app(),
    );
    recorded
        .handle
        .command(Command::Cancel {
            turn_id: turn.clone(),
        })
        .await
        .unwrap();
    recorded.wait_finished(&turn).await;
    recorded.shutdown().await;

    let path = std::env::var("HARNESS_M7_06_EVIDENCE").expect("explicit evidence output path");
    fs::write(path, output).unwrap();
}

/// Item 7: a newest turn whose mutations are all `not_applied` is a candidate
/// with no targets. The TUI offers its confirmation so the turn can be
/// consumed, instead of a dead-end "nothing to undo".
#[tokio::test]
async fn an_all_not_applied_candidate_reaches_the_confirm_modal() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture.create([reply("done")]).await;
    let turn = recorded.start("hello").await;
    recorded.wait_finished(&turn).await;
    let mut app = recorded.app();
    let view = app.view().cloned().expect("a reset arrived");
    let preview = UndoPreview {
        undo_id: UndoId::new(),
        target_turn_id: turn.clone(),
        head: view.snapshot.head().cloned(),
        sequence: view.snapshot.sequence(),
        configuration: Digest::of(b"configuration"),
        targets: Vec::new(),
        omitted: Vec::new(),
    };
    assert!(app.undo_reply(&Ok(Accepted::UndoPreviewed {
        preview: Box::new(preview)
    })));
    let screen = text(&render(&mut app, 80, 24));
    assert!(screen.contains("Undo agent turn"), "{screen}");
    assert!(
        screen.contains("No trusted file changes required restoration."),
        "{screen}"
    );
    assert!(screen.contains("y confirms"), "{screen}");
    recorded.shutdown().await;
}

/// Item 2: a refused resolution keeps the dialog and its draft, so the user
/// corrects the answer instead of losing it, and shows the stable code.
#[tokio::test]
async fn a_refused_undo_resolution_keeps_the_dialog_and_draft() {
    let fixture = Fixture::new(false);
    fs::write(fixture.workspace.join("a"), "before").unwrap();
    let mut recorded = fixture
        .create([
            proposal("write", json!({"path":"a","text":"after","overwrite":true})),
            reply("done"),
        ])
        .await;
    let turn = recorded.start("write it").await;
    let ticket = recorded.wait_approval().await;
    recorded.submit(&ticket, Decision::Allow, false).await;
    recorded.wait_finished(&turn).await;
    recorded.shutdown().await;
    fixture.inject_pending_undo();
    let mut reopened = fixture.open(Arc::new(ScriptedProvider::new([]))).await;
    reopened
        .pump(|view| view.phase == Phase::AwaitingResolution && view.undo.is_some())
        .await;
    let mut app = reopened.app();
    // The recovery dialog opens on the note, so a lower-case note that spells
    // the list's command keys is recorded exactly as typed (finding 1).
    type_text(&mut app, "restored unknown ax");
    let actions = app.on_key(key(KeyCode::Enter));
    assert!(
        actions
            .iter()
            .any(|action| matches!(action, Action::Command(Command::ResolveUndo { .. }))),
        "{actions:?}"
    );
    // The engine refuses the answer; the dialog and its draft survive.
    assert!(app.undo_reply(&Err(Rejection::Invalid("resolution_note_too_large"))));
    let screen = text(&render(&mut app, 80, 24));
    assert!(screen.contains("resolve interrupted undo"), "{screen}");
    assert!(screen.contains("note: restored unknown ax"), "{screen}");
    assert!(
        screen.contains("error: command rejected: resolution_note_too_large"),
        "{screen}"
    );
    reopened.shutdown().await;
}

/// v0.2.1 review finding 1: the note pane owns every printable character, so
/// typing a note can never change what the dialog would record; the row
/// commands work only while the file list has focus.
#[tokio::test]
async fn typing_a_note_never_changes_a_recorded_observation() {
    let fixture = Fixture::new(false);
    fs::write(fixture.workspace.join("a"), "before").unwrap();
    let mut recorded = fixture
        .create([
            proposal("write", json!({"path":"a","text":"after","overwrite":true})),
            reply("done"),
        ])
        .await;
    let turn = recorded.start("write it").await;
    let ticket = recorded.wait_approval().await;
    recorded.submit(&ticket, Decision::Allow, false).await;
    recorded.wait_finished(&turn).await;
    recorded.shutdown().await;
    fixture.inject_pending_undo();
    let mut reopened = fixture.open(Arc::new(ScriptedProvider::new([]))).await;
    reopened
        .pump(|view| view.phase == Phase::AwaitingResolution && view.undo.is_some())
        .await;
    let mut app = reopened.app();
    let screen = text(&render(&mut app, 80, 80));
    assert!(screen.contains("[note] note: (type a note)"), "{screen}");
    assert!(screen.contains("state: unknown"), "{screen}");

    // Every one of these letters is a row command when the list is focused.
    type_text(&mut app, "restored unknown ax");
    let screen = text(&render(&mut app, 80, 80));
    assert!(screen.contains("note: restored unknown ax"), "{screen}");
    assert!(
        screen.contains("(●) accept that which files changed is unknown"),
        "the mode never changed: {screen}"
    );
    assert!(
        screen.contains("state: unknown"),
        "no row took a new state: {screen}"
    );

    // Tab reaches the list, where the row commands do their job.
    app.on_key(key(KeyCode::Tab));
    let screen = text(&render(&mut app, 80, 80));
    assert!(screen.contains("[files]"), "{screen}");
    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::Char('r')));
    let screen = text(&render(&mut app, 80, 80));
    assert!(
        screen.contains("observed: state each file now"),
        "←→ still switches the mode: {screen}"
    );
    assert!(
        screen.contains("state: observed_restored"),
        "r still sets the selected row: {screen}"
    );
    // The note kept what was typed before the list had focus.
    assert!(screen.contains("note: restored unknown ax"), "{screen}");
    // And focus returns to the note without changing anything else.
    app.on_key(key(KeyCode::Tab));
    app.on_key(key(KeyCode::Char('x')));
    let screen = text(&render(&mut app, 80, 80));
    assert!(
        screen.contains("note: restored unknown axx"),
        "a command letter typed in the note is text: {screen}"
    );
    assert!(screen.contains("state: observed_restored"), "{screen}");
    reopened.shutdown().await;
}

/// Decision 17: the confirmation modal lists the calls the undo will not
/// reverse, including those of newer skipped turns.
#[tokio::test]
async fn the_undo_modal_lists_every_omitted_call() {
    let fixture = Fixture::new(false);
    let mut recorded = fixture.create([reply("done")]).await;
    let turn = recorded.start("hello").await;
    recorded.wait_finished(&turn).await;
    let mut app = recorded.app();
    let view = app.view().cloned().expect("a reset arrived");
    let call = harness_core::session::CallRef {
        response_entry_id: harness_core::session::EntryId::new(),
        call_id: harness_core::session::CallId::new(),
    };
    let preview = UndoPreview {
        undo_id: UndoId::new(),
        target_turn_id: turn,
        head: view.snapshot.head().cloned(),
        sequence: view.snapshot.sequence(),
        configuration: Digest::of(b"configuration"),
        targets: vec![harness_core::runtime::UndoPreviewTarget {
            path: "src/a.rs".into(),
            restores: true,
            bytes: Some(12),
        }],
        omitted: vec![harness_core::runtime::UndoOmittedCall {
            call: call.clone(),
            tool: "bash".into(),
            arguments: r#"{"command":"cargo test"}"#.into(),
            status: Some(ToolStatus::Succeeded),
        }],
    };
    assert!(app.undo_reply(&Ok(Accepted::UndoPreviewed {
        preview: Box::new(preview)
    })));
    let screen = text(&render(&mut app, 80, 24));
    assert!(screen.contains("Restore: \"src/a.rs\""), "{screen}");
    assert!(
        screen.contains(&format!("Not reversible: bash {}", call.call_id)),
        "{screen}"
    );
    assert!(screen.contains("cargo test"), "{screen}");
    recorded.shutdown().await;
}
