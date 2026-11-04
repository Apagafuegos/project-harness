//! `harness sessions undo`: the durable undo through the CLI, its exit codes,
//! its lock behaviour and its refusal text. Real storage and a real workspace.
use harness_core::{
    context::ModelLimits,
    model::{
        ProviderEvent,
        scripted::{Script, ScriptedProvider, Step},
    },
    runtime::Command as EngineCommand,
    runtime::*,
    session::*,
};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

fn model() -> ModelRef {
    ModelRef {
        provider_instance: "mock".into(),
        model: "scripted".into(),
    }
}
fn config() -> DriverConfig {
    let mut config = DriverConfig::default();
    config.runtime.context.catalog.insert(
        model(),
        ModelLimits {
            context_tokens: Some(64_000),
            max_output_tokens: None,
        },
    );
    config
}
fn policy() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "policy_revision": "p1",
        "enabled_tools": ["read", "write"],
        "workspace_read": "allow",
        "workspace_mutation": "ask",
        "process": "deny",
        "network": "deny",
        "protected_paths": [],
        "allowances": []
    }))
    .unwrap()
}

fn run(state: &Path, id: &SessionId, yes: bool) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_harness"));
    command
        .arg("sessions")
        .arg("undo")
        .arg(id.to_string())
        .arg("--data-root")
        .arg(state);
    if yes {
        command.arg("--yes");
    }
    command.output().unwrap()
}
fn stdout(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}
fn stderr(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

struct Fixture {
    _temp: tempfile::TempDir,
    state: PathBuf,
    workspace: PathBuf,
    host: ToolHostConfig,
    root: Arc<DataRoot>,
    session: SessionId,
    header: SessionHeader,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("workspace")).unwrap();
        let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
        let host = ToolHostConfig {
            workspace_root: temp.path().join("workspace"),
            data_root: root.path().into(),
            sandbox: None,
        };
        let header = SessionHeader {
            workspace: host.workspace_root.to_str().unwrap().into(),
            initial_model: model(),
            policy_revision: "p1".into(),
            configuration: root.put_artifact(&policy()).unwrap(),
        };
        let state = root.path().to_path_buf();
        Self {
            _temp: temp,
            state,
            workspace: host.workspace_root.clone(),
            host,
            root,
            session: SessionId::new(),
            header,
        }
    }

    /// Write `after` over `a`, through a real captured write turn.
    fn write_turn(&self) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let script = Script::new([
                Step::Event(ProviderEvent::Started),
                Step::Event(ProviderEvent::ToolCallStarted {
                    block_index: 0,
                    name: "write".into(),
                    provider_call_id: None,
                }),
                Step::Event(ProviderEvent::ToolArgumentsDelta {
                    block_index: 0,
                    fragment: json!({"path":"a","text":"after","overwrite":true}).to_string(),
                }),
                Step::Event(ProviderEvent::ToolCallReady { block_index: 0 }),
                Step::completed(FinishReason::ToolCalls),
            ]);
            let reply = Script::new([
                Step::Event(ProviderEvent::Started),
                Step::text(0, "done"),
                Step::completed(FinishReason::Stop),
            ]);
            let handle = SessionHandle::create_with_tools(
                self.root.clone(),
                self.session.clone(),
                self.header.clone(),
                Arc::new(ScriptedProvider::new([script, reply])),
                config(),
                &self.host,
            )
            .await
            .unwrap();
            handle
                .command(EngineCommand::StartTurn {
                    command_id: CommandId::new(),
                    content: vec![ContentBlock::Text { text: "go".into() }],
                })
                .await
                .unwrap();
            let ticket = loop {
                let view = handle.view().await.unwrap();
                if let Some(ticket) = view.pending_approval {
                    break ticket;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            handle
                .command(EngineCommand::SubmitApproval {
                    approval_id: ticket.approval_id,
                    plan_digest: ticket.plan_digest,
                    decision: Decision::Allow,
                    remember: false,
                })
                .await
                .unwrap();
            for _ in 0..2000 {
                let view = handle.view().await.unwrap();
                if view.phase == Phase::Durable(RuntimeState::Idle) && view.active_turn.is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            handle.shutdown().await;
        });
    }
}

#[test]
fn undo_restores_the_file_and_prints_the_report() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("a"), "before").unwrap();
    fixture.write_turn();
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("a")).unwrap(),
        "after"
    );

    let output = run(&fixture.state, &fixture.session, true);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("Undo agent turn"), "{text}");
    assert!(text.contains("restore: a"), "{text}");
    assert!(text.contains("undo succeeded"), "{text}");
    assert!(text.contains("restored a"), "{text}");
    assert!(
        text.contains("The model context records this undo through a user note."),
        "{text}"
    );
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("a")).unwrap(),
        "before"
    );

    // A second undo has nothing left to revert: a stable refusal, exit 2.
    let again = run(&fixture.state, &fixture.session, true);
    assert_eq!(again.status.code(), Some(2));
    assert!(
        stderr(&again).contains("undo_nothing"),
        "{}",
        stderr(&again)
    );
}

#[test]
fn a_stale_target_refuses_with_its_path_and_exit_two() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("a"), "before").unwrap();
    fixture.write_turn();
    fs::write(fixture.workspace.join("a"), "user edit").unwrap();
    let output = run(&fixture.state, &fixture.session, true);
    assert_eq!(output.status.code(), Some(2));
    let text = stderr(&output);
    assert!(text.contains("undo_conflict"), "{text}");
    assert!(text.contains("changed a"), "{text}");
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("a")).unwrap(),
        "user edit"
    );
}

#[test]
fn without_a_terminal_the_command_requires_yes() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("a"), "before").unwrap();
    fixture.write_turn();
    let output = run(&fixture.state, &fixture.session, false);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("confirmation_required"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("a")).unwrap(),
        "after",
        "--yes was not given, so nothing was undone"
    );
}

#[test]
fn a_live_engine_holds_the_lock_and_the_command_refuses() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("a"), "before").unwrap();
    fixture.write_turn();
    // A live engine owns the journal for the whole command, so the undo
    // refuses with `session_locked` instead of waiting on the lock.
    let (ready, opened) = std::sync::mpsc::channel::<()>();
    let (stop, held) = std::sync::mpsc::channel::<()>();
    let (root, session, host) = (
        fixture.root.clone(),
        fixture.session.clone(),
        fixture.host.clone(),
    );
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let handle = SessionHandle::open_with_tools(
                root,
                session,
                Arc::new(ScriptedProvider::new([])),
                config(),
                &host,
            )
            .await
            .unwrap();
            ready.send(()).unwrap();
            let _ = held.recv();
            handle.shutdown().await;
        });
    });
    opened.recv().unwrap();
    let output = run(&fixture.state, &fixture.session, true);
    stop.send(()).unwrap();
    thread.join().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("session_locked"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("a")).unwrap(),
        "after",
        "a locked session is never changed"
    );
}
