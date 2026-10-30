//! Offline end-to-end client tests: real journals/tools, injected scripted inference and I/O.
use harness::ask::{self, Io, Options, Terminal};
use harness_core::{
    context::ModelLimits,
    model::{
        ProviderEvent,
        scripted::{Script, ScriptedProvider, Step},
    },
    runtime::DriverConfig,
    session::*,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Cursor, Write},
    path::Path,
    sync::Arc,
    time::Duration,
};

struct Fixture {
    temp: tempfile::TempDir,
    options: Options,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("note"), "hello").unwrap();
        let model = ModelRef {
            provider_instance: "test-codex".into(),
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
        Self {
            options: Options {
                data_root: temp.path().join("state"),
                workspace,
                session: None,
                model,
                explicit_model: false,
                summary_model: None,
                prompt: "hello".into(),
                approve_read_only: false,
                driver,
                sandbox: None,
                codex: true,
            },
            temp,
        }
    }
    fn snapshot(&self, stderr: &str) -> SessionSnapshot {
        let id: SessionId = stderr
            .lines()
            .find_map(|l| l.strip_prefix("session: "))
            .unwrap()
            .parse()
            .unwrap();
        JournalStore::inspect(
            &DataRoot::open(&self.options.data_root, false).unwrap(),
            &id,
        )
        .unwrap()
        .snapshot
        .unwrap()
    }
    async fn run(
        &self,
        provider: Arc<ScriptedProvider>,
        terminal: Option<&mut dyn Terminal>,
    ) -> (u8, Vec<u8>, String) {
        run(
            self.options.clone(),
            provider,
            terminal,
            Cursor::new(Vec::new()),
            None,
        )
        .await
    }
}
async fn run(
    options: Options,
    provider: Arc<ScriptedProvider>,
    terminal: Option<&mut dyn Terminal>,
    mut stdin: Cursor<Vec<u8>>,
    tx: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
) -> (u8, Vec<u8>, String) {
    let (_sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = tokio::time::timeout(
        Duration::from_secs(10),
        ask::run(
            options,
            provider,
            Io {
                stdin: &mut stdin,
                stdout: &mut stdout,
                stderr: &mut stderr,
                stderr_is_tty: false,
                terminal,
                interrupts: tx.unwrap_or(receiver),
            },
        ),
    )
    .await
    .expect("client did not finish (including cancellation)");
    (code, stdout, String::from_utf8(stderr).unwrap())
}
fn reply(text: &str) -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, text),
        Step::completed(FinishReason::Stop),
    ])
}
fn proposal(name: &str, args: Value) -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::Event(ProviderEvent::ToolCallStarted {
            block_index: 0,
            name: name.into(),
            provider_call_id: None,
        }),
        Step::Event(ProviderEvent::ToolArgumentsDelta {
            block_index: 0,
            fragment: args.to_string(),
        }),
        Step::Event(ProviderEvent::ToolCallReady { block_index: 0 }),
        Step::completed(FinishReason::ToolCalls),
    ])
}
struct FakeTerminal {
    allow: bool,
    proposals: Vec<String>,
}
impl FakeTerminal {
    fn new(allow: bool) -> Self {
        Self {
            allow,
            proposals: Vec::new(),
        }
    }
}
impl Terminal for FakeTerminal {
    fn begin(&mut self, p: &str) -> io::Result<()> {
        self.proposals.push(p.into());
        Ok(())
    }
    fn poll(&mut self) -> io::Result<Option<bool>> {
        Ok(Some(self.allow))
    }
}
fn result_status(snapshot: &SessionSnapshot) -> ToolStatus {
    snapshot
        .entries()
        .find_map(|(_, s)| match &s.entry.payload {
            EntryPayload::ToolResult(r) => Some(r.result.status),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn exact_stdout_streamed_answer_and_reasoning_only_on_stderr() {
    let f = Fixture::new();
    let p = Arc::new(ScriptedProvider::new([Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, "po"),
        Step::Event(ProviderEvent::ReasoningSummaryDelta {
            block_index: 1,
            text: "summary\nnot raw".into(),
        }),
        Step::text(0, "ng\n"),
        Step::provider_state(2, "opaque", b"ENCRYPTED-NOT-OUTPUT".to_vec()),
        Step::Event(ProviderEvent::UsageUpdated {
            usage: Usage {
                input_tokens: Some(3),
                output_tokens: Some(2),
                cached_input_tokens: None,
                reasoning_tokens: None,
            },
        }),
        Step::completed(FinishReason::Stop),
    ])]));
    let (code, out, err) = f.run(p, None).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, b"pong\n");
    assert!(
        err.contains("reasoning: summary not raw") && err.contains("usage:"),
        "{err}"
    );
    assert!(!err.contains("ENCRYPTED") && !err.contains('\x1b'));
    assert_eq!(err.matches("shell unavailable:").count(), 1);
}

#[tokio::test]
async fn tool_proposal_approved_via_fake_terminal() {
    let f = Fixture::new();
    let p = Arc::new(ScriptedProvider::new([
        proposal("write", json!({"path":"new", "text":"yes"})),
        reply("done"),
    ]));
    let mut tty = FakeTerminal::new(true);
    let (code, out, err) = f.run(p, Some(&mut tty)).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, b"done");
    assert_eq!(tty.proposals.len(), 1);
    assert!(tty.proposals[0].contains("write") && tty.proposals[0].contains("new"));
    assert_eq!(
        fs::read_to_string(f.options.workspace.join("new")).unwrap(),
        "yes"
    );
    assert_eq!(result_status(&f.snapshot(&err)), ToolStatus::Succeeded);
}

#[tokio::test]
async fn tool_proposal_denied_via_fake_terminal() {
    let f = Fixture::new();
    let mut tty = FakeTerminal::new(false);
    let (code, _, err) = f
        .run(
            Arc::new(ScriptedProvider::new([
                proposal("write", json!({"path":"new", "text":"no"})),
                reply("denied"),
            ])),
            Some(&mut tty),
        )
        .await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(tty.proposals.len(), 1);
    assert!(!f.options.workspace.join("new").exists());
    assert_eq!(result_status(&f.snapshot(&err)), ToolStatus::Denied);
}

#[tokio::test]
async fn no_terminal_denies_every_call_even_approve_read_only() {
    for name in ["read", "write"] {
        let mut f = Fixture::new();
        f.options.approve_read_only = true;
        let args = if name == "read" {
            json!({"path":"note"})
        } else {
            json!({"path":"new","text":"no"})
        };
        let (code, _, err) = f
            .run(
                Arc::new(ScriptedProvider::new([
                    proposal(name, args),
                    reply("denied"),
                ])),
                None,
            )
            .await;
        assert_eq!(code, 0, "{err}");
        assert!(err.contains("no controlling terminal"));
        assert_eq!(
            result_status(&f.snapshot(&err)),
            ToolStatus::Denied,
            "{name}"
        );
        assert!(!f.options.workspace.join("new").exists());
    }
}

#[tokio::test]
async fn approve_read_only_uses_registry_but_never_auto_approves_mutation() {
    let mut f = Fixture::new();
    f.options.approve_read_only = true;
    let mut tty = FakeTerminal::new(false);
    let p = Arc::new(ScriptedProvider::new([
        proposal("read", json!({"path":"note"})),
        proposal("write", json!({"path":"new","text":"no"})),
        reply("done"),
    ]));
    let (code, _, err) = f.run(p, Some(&mut tty)).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(tty.proposals.len(), 1);
    assert!(tty.proposals[0].contains("write"));
    let snapshot = f.snapshot(&err);
    let statuses: Vec<_> = snapshot
        .entries()
        .filter_map(|(_, s)| match &s.entry.payload {
            EntryPayload::ToolResult(r) => Some(r.result.status),
            _ => None,
        })
        .collect();
    assert!(statuses.contains(&ToolStatus::Succeeded) && statuses.contains(&ToolStatus::Denied));
}

#[tokio::test]
async fn continue_session_preserves_history_without_reprinting_old_answer() {
    let mut f = Fixture::new();
    let (code, out, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("first")])), None)
        .await;
    assert_eq!(code, 0);
    assert_eq!(out, b"first");
    let before = f.snapshot(&err);
    f.options.session = Some(before.session_id().clone());
    let p = Arc::new(ScriptedProvider::new([reply("second")]));
    let (code, out, err) = f.run(p.clone(), None).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, b"second");
    assert!(f.snapshot(&err).sequence() > before.sequence());
    assert!(format!("{:?}", p.streams()[0].request.input.items).contains("first"));
}

#[tokio::test]
async fn stdin_dash_is_prompt_not_approval_input() {
    let mut f = Fixture::new();
    f.options.prompt = "-".into();
    let p = Arc::new(ScriptedProvider::new([
        proposal("write", json!({"path":"new","text":"no"})),
        reply("stdin reply"),
    ]));
    let mut tty = FakeTerminal::new(false);
    let (code, out, err) = run(
        f.options.clone(),
        p.clone(),
        Some(&mut tty),
        Cursor::new(b"piped prompt\ny\n".to_vec()),
        None,
    )
    .await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, b"stdin reply");
    assert!(format!("{:?}", p.streams()[0].request.input.items).contains("piped prompt"));
    assert_eq!(tty.proposals.len(), 1);
    assert_eq!(result_status(&f.snapshot(&err)), ToolStatus::Denied);
}

// Send the first interrupt only after a real text write, proving streaming
// precedes completion and cancellation isn't a startup timing assumption.
struct InterruptingOutput {
    bytes: Vec<u8>,
    signal: tokio::sync::mpsc::UnboundedSender<()>,
}
impl Write for InterruptingOutput {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(b);
        self.signal.send(()).unwrap();
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[tokio::test]
async fn cancellation_returns_130_and_durable_cancelled_turn() {
    let f = Fixture::new();
    let p = Arc::new(ScriptedProvider::new([Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, "partial"),
        Step::AwaitCancel,
        Step::cancelled(),
    ])]));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut out = InterruptingOutput {
        bytes: Vec::new(),
        signal: tx,
    };
    let mut err = Vec::new();
    let code = tokio::time::timeout(
        Duration::from_secs(5),
        ask::run(
            f.options.clone(),
            p.clone(),
            Io {
                stdin: &mut io::empty(),
                stdout: &mut out,
                stderr: &mut err,
                stderr_is_tty: false,
                terminal: None,
                interrupts: rx,
            },
        ),
    )
    .await
    .expect("Ctrl-C was ignored");
    assert_eq!(code, 130);
    assert_eq!(out.bytes, b"partial");
    let snapshot = f.snapshot(&String::from_utf8(err).unwrap());
    assert!(snapshot.events().any(|(_,_, e)| matches!(e, JournalEvent::TurnFinished(f) if f.outcome == TurnOutcome::Cancelled)));
    assert!(p.streams()[0].cancelled && p.streams()[0].dropped);
}

#[tokio::test]
async fn authentication_failure_is_3_with_codex_hint_and_secret_free_storage() {
    let f = Fixture::new();
    let p = Arc::new(ScriptedProvider::new([Script::new([Step::failed(
        ModelError {
            code: ModelErrorCode::Authentication,
            message: "provider authentication failed; run `codex login`".into(),
            provider_code: None,
            retry_after_ms: None,
        },
    )])]));
    let (code, out, err) = f.run(p, None).await;
    assert_eq!(code, 3, "{err}");
    assert!(out.is_empty());
    assert!(err.contains("authentication") && err.contains("run `codex login`"));
    scan_no_secret(f.temp.path(), b"SENTINEL-CREDENTIAL-ASK");
}
fn scan_no_secret(path: &Path, secret: &[u8]) {
    for item in fs::read_dir(path).unwrap() {
        let path = item.unwrap().path();
        if path.is_dir() {
            scan_no_secret(&path, secret);
        } else {
            assert!(
                !fs::read(&path)
                    .unwrap()
                    .windows(secret.len())
                    .any(|w| w == secret),
                "secret in {}",
                path.display()
            );
        }
    }
}

#[tokio::test]
async fn budget_failure_is_1_without_opening_provider() {
    let mut f = Fixture::new();
    f.options.driver.runtime.context.catalog.insert(
        f.options.model.clone(),
        ModelLimits {
            context_tokens: Some(1),
            max_output_tokens: None,
        },
    );
    let p = Arc::new(ScriptedProvider::new([]));
    let (code, out, err) = f.run(p.clone(), None).await;
    assert_eq!(code, 1, "{err}");
    assert!(out.is_empty());
    assert!(err.contains("context_budget_exceeded"));
    assert!(p.streams().is_empty());
}

#[tokio::test]
async fn argument_summary_is_bounded_single_line_and_terminal_errors_deny() {
    struct Broken;
    impl Terminal for Broken {
        fn begin(&mut self, _: &str) -> io::Result<()> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn poll(&mut self) -> io::Result<Option<bool>> {
            unreachable!()
        }
    }
    let f = Fixture::new();
    let (code, _, err) = f
        .run(
            Arc::new(ScriptedProvider::new([
                proposal(
                    "write",
                    json!({"path":"new","text":"\n\u{1b}[31m".repeat(500)}),
                ),
                reply("done"),
            ])),
            Some(&mut Broken),
        )
        .await;
    assert_eq!(code, 0, "{err}");
    assert!(
        err.lines()
            .find(|s| s.starts_with("tool: "))
            .unwrap()
            .chars()
            .count()
            < 420
    );
    assert!(!err.contains('\x1b'));
    assert!(!f.options.workspace.join("new").exists());
    assert_eq!(result_status(&f.snapshot(&err)), ToolStatus::Denied);
}

#[tokio::test]
async fn continuation_refuses_auth_owner_or_summary_policy_change() {
    let mut f = Fixture::new();
    let (_, _, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("first")])), None)
        .await;
    f.options.session = Some(f.snapshot(&err).session_id().clone());
    f.options.model.provider_instance = "other".into();
    let (code, _, err) = f.run(Arc::new(ScriptedProvider::new([])), None).await;
    assert_eq!(code, 2);
    assert!(err.contains("session_model_mismatch"));
    f.options.model.provider_instance = "test-codex".into();
    f.options.summary_model = Some(f.options.model.clone());
    let (code, _, err) = f.run(Arc::new(ScriptedProvider::new([])), None).await;
    assert_eq!(code, 2);
    assert!(err.contains("session_policy_mismatch"));
}

#[tokio::test]
async fn lag_resets_preserve_exact_stdout_without_duplicates() {
    let mut f = Fixture::new();
    f.options.driver.event_capacity = 1;
    let mut steps = vec![Step::Event(ProviderEvent::Started)];
    for _ in 0..256 {
        steps.push(Step::text(0, "é"));
    }
    steps.push(Step::completed(FinishReason::Stop));
    let (code, out, err) = f
        .run(Arc::new(ScriptedProvider::new([Script::new(steps)])), None)
        .await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "é".repeat(256).as_bytes());
}

#[tokio::test]
async fn summary_model_opt_in_is_independent_and_persisted() {
    let mut f = Fixture::new();
    let summary = ModelRef {
        provider_instance: "test-codex".into(),
        model: "cheap".into(),
    };
    f.options.summary_model = Some(summary.clone());
    let (code, _, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("done")])), None)
        .await;
    assert_eq!(code, 0, "{err}");
    let snapshot = f.snapshot(&err);
    let root = DataRoot::open(&f.options.data_root, false).unwrap();
    let configuration: Value =
        serde_json::from_slice(&root.read_artifact(snapshot.configuration(), 16384).unwrap())
            .unwrap();
    assert_eq!(configuration["compaction"]["automatic"], true);
    assert_eq!(
        configuration["compaction"]["summary_model"]["model"],
        "cheap"
    );
    assert_ne!(summary, f.options.model);
}
