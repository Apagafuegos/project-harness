//! Crash fixture isolated from other targets' lock/spawn windows.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use harness::{
    tui::{App, SessionInfo, runtime},
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
    tools::*,
};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

fn options(base: &Path) -> SessionOptions {
    let model = ModelRef {
        provider_instance: "mock".into(),
        model: "scripted".into(),
    };
    let mut driver = DriverConfig::default();
    driver.runtime.context.catalog.insert(
        model.clone(),
        ModelLimits {
            context_tokens: Some(128_000),
            max_output_tokens: None,
        },
    );
    SessionOptions {
        data_root: base.join("state"),
        workspace: base.join("work"),
        session: None,
        model,
        explicit_model: false,
        explicit_provider: false,
        switch_policy: Portability::Reject,
        known_instances: vec![],
        summary_model: None,
        driver,
        sandbox: None,
    }
}
async fn wait(handle: &SessionHandle, predicate: impl Fn(&RuntimeView) -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if predicate(&handle.view().await.unwrap()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}
struct Hold(PathBuf);
impl Executor for Hold {
    fn backend(&self) -> ExecutionBackend {
        ExecutionBackend::TrustedFs
    }
    fn execute(&self, _: &AuthorizedOperation, _: &CancelToken) -> ExecutionOutcome {
        std::fs::write(&self.0, b"intent durable").unwrap();
        loop {
            std::thread::park();
        }
    }
}
#[test]
fn crash_child() {
    let Some(base) = std::env::var_os("HARNESS_TUI_CRASH_ROOT") else {
        return;
    };
    let base = PathBuf::from(base);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut options = options(&base);
            let opened = wiring::open_session(&options, Arc::new(ScriptedProvider::new([])))
                .await
                .unwrap();
            std::fs::write(base.join("session"), opened.session.to_string()).unwrap();
            opened.handle.shutdown().await;
            options.session = Some(opened.session.clone());
            let mut config = options.driver.clone();
            config.tools = ToolHost {
                workspace: Arc::new(
                    FsWorkspace::open(&options.workspace, &FsProtectedPaths::default()).unwrap(),
                ),
                executors: vec![Arc::new(Hold(base.join("ready")))],
            };
            let script = Script::new([
                Step::Event(ProviderEvent::Started),
                Step::Event(ProviderEvent::ToolCallStarted {
                    block_index: 0,
                    name: "write".into(),
                    provider_call_id: None,
                }),
                Step::Event(ProviderEvent::ToolArgumentsDelta {
                    block_index: 0,
                    fragment: json!({"path":"never-replayed", "text":"effect"}).to_string(),
                }),
                Step::Event(ProviderEvent::ToolCallReady { block_index: 0 }),
                Step::completed(FinishReason::ToolCalls),
            ]);
            let root = Arc::new(DataRoot::open(&options.data_root, false).unwrap());
            let handle = SessionHandle::open(
                root,
                opened.session,
                Arc::new(ScriptedProvider::new([script])),
                config,
            )
            .await
            .unwrap();
            handle
                .command(Command::StartTurn {
                    command_id: CommandId::new(),
                    content: vec![ContentBlock::Text {
                        text: "crash during write".into(),
                    }],
                })
                .await
                .unwrap();
            wait(&handle, |v| v.pending_approval.is_some()).await;
            let ticket = handle.view().await.unwrap().pending_approval.unwrap();
            handle
                .command(Command::SubmitApproval {
                    approval_id: ticket.approval_id,
                    plan_digest: ticket.plan_digest,
                    decision: Decision::Allow,
                    remember: false,
                })
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
}
struct WithStartup(SessionHandle, ModelRef);
impl runtime::Commands for WithStartup {
    fn command(
        &self,
        command: Command,
    ) -> futures_util::future::BoxFuture<'static, Result<Accepted, Rejection>> {
        runtime::Commands::command(&self.0, command)
    }
    fn startup(&self) -> Option<Command> {
        Some(Command::SwitchModel {
            model: self.1.clone(),
            policy: Portability::Reject,
        })
    }
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[tokio::test]
async fn actual_crash_uncertain_resolution_then_next_turn() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("work")).unwrap();
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_child", "--nocapture"])
            .env("HARNESS_TUI_CRASH_ROOT", temp.path())
            .spawn()
            .unwrap(),
    );
    tokio::time::timeout(Duration::from_secs(30), async {
        while !temp.path().join("ready").exists() {
            assert!(child.0.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let mut options = options(temp.path());
    options.session = Some(
        std::fs::read_to_string(temp.path().join("session"))
            .unwrap()
            .parse()
            .unwrap(),
    );
    let provider = Arc::new(ScriptedProvider::new([Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, "after recovery"),
        Step::completed(FinishReason::Stop),
    ])]));
    let opened = wiring::open_session(&options, provider.clone())
        .await
        .unwrap();
    let handle = &opened.handle;
    let mut sub = handle.subscribe();
    let subscriptions =
        async_stream::stream! { while let Some(item) = sub.next().await { yield item; } };
    let events = async_stream::stream! {
        wait(handle, |v| v.phase == Phase::AwaitingResolution).await;
        assert!(provider.streams().is_empty());
        tokio::time::sleep(Duration::from_millis(40)).await;
        yield Ok(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        wait(handle, |v| v.phase == Phase::Durable(RuntimeState::Idle)).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        yield Ok(Event::Paste("continue".into()));
        yield Ok(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        wait(handle, |v| v.snapshot.events().any(|(_,_,e)| matches!(e, JournalEvent::TurnFinished(f) if f.outcome == TurnOutcome::Completed))).await;
        yield Ok(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)));
    };
    // An explicit model override must not block the uncertainty modal while
    // its startup switch waits for recovery. Identical models are a no-op.
    let commands = WithStartup(handle.clone(), options.model.clone());
    let mut app = App::new(SessionInfo {
        workspace: options.workspace.clone(),
        model: options.model,
        context_limit: Some(128_000),
    });
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
    tokio::time::timeout(
        Duration::from_secs(30),
        runtime::drive(
            &mut terminal,
            &mut app,
            events,
            subscriptions,
            futures_util::stream::pending(),
            &commands,
            || {},
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let view = handle.view().await.unwrap();
    assert!(view.snapshot.entries().any(|(_,e)| matches!(&e.entry.payload, EntryPayload::ToolResult(r) if r.result.status == ToolStatus::Uncertain)));
    assert!(!options.workspace.join("never-replayed").exists());
    handle.shutdown().await;
    let root = DataRoot::open(&options.data_root, false).unwrap();
    assert!(
        JournalStore::inspect(&root, &opened.session)
            .unwrap()
            .diagnostic
            .is_none()
    );
}
