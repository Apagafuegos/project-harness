//! Offline end-to-end client tests: real journals/tools, injected scripted inference and I/O.
use harness::ask::{self, Io, Options, Terminal};
use harness_core::{
    context::{ContextItem, ModelCapabilities, ModelInfo, ModelLimits},
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
                explicit_provider: false,
                switch_policy: Portability::Reject,
                known_instances: Vec::new(),
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

/// One completed response carrying owner-scoped native state, so a later
/// switch has something optional to lose.
fn native_reply(text: &str) -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, text),
        Step::provider_state(1, "mock.state.v1", b"state!!".to_vec()),
        Step::completed(FinishReason::Stop),
    ])
}

/// Catalog the switch targets, and tell the client which instances exist.
fn switching_fixture(policy: Portability) -> Fixture {
    let mut f = Fixture::new();
    let other = ModelRef {
        provider_instance: "other".into(),
        model: "cheap".into(),
    };
    let text_only = ModelRef {
        provider_instance: "other".into(),
        model: "text-only".into(),
    };
    for (model, capabilities) in [
        (other.clone(), ModelCapabilities::default()),
        (
            text_only,
            ModelCapabilities {
                tools: true,
                ..Default::default()
            },
        ),
    ] {
        f.options.driver.runtime.context.catalog.insert(
            model,
            ModelInfo {
                limits: ModelLimits {
                    context_tokens: Some(128_000),
                    max_output_tokens: None,
                },
                capabilities,
            },
        );
    }
    f.options.known_instances = vec!["test-codex".into(), "other".into()];
    f.options.switch_policy = policy;
    f
}

/// `--switch-policy reject` (the default) refuses a continuation that names
/// another provider when native state would be lost: the report goes to stderr,
/// the exit code is 2, and nothing is written.
#[tokio::test]
async fn continuation_switch_is_rejected_by_default_with_the_portability_report() {
    let mut f = switching_fixture(Portability::Reject);
    let (code, out, err) = f
        .run(
            Arc::new(ScriptedProvider::new([native_reply("first")])),
            None,
        )
        .await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, b"first");
    let before = f.snapshot(&err);
    f.options.session = Some(before.session_id().clone());
    f.options.model = ModelRef {
        provider_instance: "other".into(),
        model: "cheap".into(),
    };
    f.options.explicit_provider = true;
    let provider = Arc::new(ScriptedProvider::new([]));
    let (code, out, err) = f.run(provider.clone(), None).await;
    assert_eq!(code, 2, "{err}");
    assert!(out.is_empty(), "a refused switch runs no turn");
    assert!(err.contains("error: portability_loss"), "{err}");
    assert!(
        err.contains("switch: refused from test-codex/scripted to other/cheap"),
        "{err}"
    );
    assert!(
        err.contains("switch: omits entry=") && err.contains("part=block[1]"),
        "{err}"
    );
    assert!(
        err.contains("owner=test-codex/scripted schema=mock.state.v1 bytes=7"),
        "{err}"
    );
    // The report never carries the payload.
    assert!(!err.contains("state!!"), "{err}");
    // Keep the transcript visible in the test log: it is the user-facing output.
    for line in err.lines().filter(|line| line.starts_with("switch: ")) {
        println!("{line}");
    }
    assert_eq!(provider.streams().len(), 0, "no inference was opened");
    assert_eq!(
        f.snapshot(&err).sequence(),
        before.sequence(),
        "a refusal writes nothing"
    );
}

/// `--switch-policy omit-optional` prints the same report and then runs the
/// turn on the target model.
#[tokio::test]
async fn continuation_switch_omits_with_an_explicit_policy_and_runs_the_turn() {
    let mut f = switching_fixture(Portability::OmitOptional);
    let (_, _, err) = f
        .run(
            Arc::new(ScriptedProvider::new([native_reply("first")])),
            None,
        )
        .await;
    f.options.session = Some(f.snapshot(&err).session_id().clone());
    f.options.model = ModelRef {
        provider_instance: "other".into(),
        model: "cheap".into(),
    };
    f.options.explicit_model = true;
    let provider = Arc::new(ScriptedProvider::new([reply("second")]));
    let (code, out, err) = f.run(provider.clone(), None).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, b"second");
    assert!(
        err.contains("switch: switched from test-codex/scripted to other/cheap"),
        "{err}"
    );
    assert!(
        err.contains("owner=test-codex/scripted schema=mock.state.v1 bytes=7"),
        "{err}"
    );
    let request = &provider.streams()[0].request;
    for line in err.lines().filter(|line| line.starts_with("switch: ")) {
        println!("{line}");
    }
    assert_eq!(request.input.model.provider_instance, "other");
    assert!(request.input.continuation.is_none());
    assert!(request.input.items.iter().all(|item| {
        match item {
            ContextItem::Assistant { content, .. } => !content
                .iter()
                .any(|block| matches!(block, ContentBlock::ProviderState { .. })),
            _ => true,
        }
    }));
}

/// Without an override a continuation keeps the branch's model: choosing a
/// different default provider must not silently switch.
#[tokio::test]
async fn continuation_without_an_override_keeps_the_saved_model() {
    let mut f = switching_fixture(Portability::Reject);
    let (_, _, err) = f
        .run(
            Arc::new(ScriptedProvider::new([native_reply("first")])),
            None,
        )
        .await;
    f.options.session = Some(f.snapshot(&err).session_id().clone());
    f.options.model = ModelRef {
        provider_instance: "other".into(),
        model: "cheap".into(),
    };
    let provider = Arc::new(ScriptedProvider::new([reply("second")]));
    let (code, out, err) = f.run(provider.clone(), None).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, b"second");
    assert!(!err.contains("switch:"), "{err}");
    assert_eq!(
        provider.streams()[0].request.input.model,
        ModelRef {
            provider_instance: "test-codex".into(),
            model: "scripted".into()
        }
    );
}

/// A required incompatibility cannot be omitted under either policy. The
/// history holds a tool call and its result, so a target without tool support is
/// refused even with `omit-optional`.
#[tokio::test]
async fn a_required_incompatibility_is_refused_under_both_policies() {
    for policy in [Portability::Reject, Portability::OmitOptional] {
        let mut f = switching_fixture(policy);
        let no_tools = ModelRef {
            provider_instance: "other".into(),
            model: "no-tools".into(),
        };
        f.options.driver.runtime.context.catalog.insert(
            no_tools.clone(),
            ModelInfo {
                limits: ModelLimits {
                    context_tokens: Some(128_000),
                    max_output_tokens: None,
                },
                capabilities: ModelCapabilities {
                    tools: false,
                    ..Default::default()
                },
            },
        );
        let (code, _, err) = f
            .run(
                Arc::new(ScriptedProvider::new([
                    proposal("read", json!({"path": "note"})),
                    reply("denied"),
                ])),
                None,
            )
            .await;
        assert_eq!(code, 0, "{err}");
        assert_eq!(result_status(&f.snapshot(&err)), ToolStatus::Denied);
        f.options.session = Some(f.snapshot(&err).session_id().clone());
        f.options.model = no_tools;
        f.options.explicit_provider = true;
        let (code, out, err) = f.run(Arc::new(ScriptedProvider::new([])), None).await;
        assert_eq!(code, 2, "{err}");
        assert!(out.is_empty());
        assert!(err.contains("error: portability_loss"), "{err}");
        assert!(
            err.contains("switch: refused from test-codex/scripted to other/no-tools"),
            "{err}"
        );
        assert!(
            err.contains("switch: requires entry=") && err.contains("capability=tools"),
            "{err}"
        );
    }
}

/// A target instance the process cannot serve is a configuration error before
/// anything is opened, not a failed inference.
#[tokio::test]
async fn an_unregistered_target_instance_is_refused_before_opening() {
    let mut f = switching_fixture(Portability::Reject);
    let (_, _, err) = f
        .run(
            Arc::new(ScriptedProvider::new([native_reply("first")])),
            None,
        )
        .await;
    f.options.session = Some(f.snapshot(&err).session_id().clone());
    f.options.known_instances = vec!["test-codex".into()];
    f.options.model = ModelRef {
        provider_instance: "missing".into(),
        model: "cheap".into(),
    };
    f.options.explicit_provider = true;
    let (code, out, err) = f.run(Arc::new(ScriptedProvider::new([])), None).await;
    assert_eq!(code, 2, "{err}");
    assert!(out.is_empty());
    assert!(err.contains("provider_not_configured"), "{err}");
}

#[tokio::test]
async fn continuation_refuses_a_summary_policy_change() {
    let mut f = Fixture::new();
    let (_, _, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("first")])), None)
        .await;
    f.options.session = Some(f.snapshot(&err).session_id().clone());
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
