//! Offline end-to-end client tests: real journals/tools, injected scripted inference and I/O.
use harness::ask::{self, Io, Options, Terminal};
use harness_core::{
    context::{ContextItem, ModelCapabilities, ModelInfo, ModelLimits, StrategyId},
    model::{
        ProviderEvent,
        scripted::{Script, ScriptedProvider, Step},
    },
    runtime::{DriverConfig, SessionHandle},
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
                compaction_strategy: harness::wiring::creation_compaction_strategy(),
                explicit_strategy: false,
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
        proposal("list", json!({})),
        proposal("glob", json!({"pattern":"*"})),
        proposal("grep", json!({"pattern":"hello"})),
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
    assert_eq!(
        statuses,
        vec![
            ToolStatus::Succeeded,
            ToolStatus::Succeeded,
            ToolStatus::Succeeded,
            ToolStatus::Succeeded,
            ToolStatus::Denied
        ]
    );
    assert_eq!(
        recorded_configuration(&f, &err)["enabled_tools"],
        json!(["read", "write", "edit", "bash", "list", "glob", "grep"])
    );
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
    // Naming the branch's own model explicitly is not a switch either.
    let before = f.snapshot(&err);
    f.options.model = ModelRef {
        provider_instance: "test-codex".into(),
        model: "scripted".into(),
    };
    f.options.explicit_model = true;
    let (code, _, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("third")])), None)
        .await;
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("switch:"), "{err}");
    assert!(
        !f.snapshot(&err)
            .entries()
            .any(|(_, stored)| matches!(stored.entry.payload, EntryPayload::ModelChanged(_)))
    );
    assert!(f.snapshot(&err).sequence() > before.sequence());
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

#[tokio::test]
async fn explicit_compaction_strategy_is_recorded_and_the_new_session_default_is_masking() {
    let mut f = Fixture::new();
    f.options.compaction_strategy = StrategyId::StructuredState;
    f.options.explicit_strategy = true;
    f.options.summary_model = Some(ModelRef {
        provider_instance: f.options.model.provider_instance.clone(),
        model: "summary".into(),
    });
    let (code, _, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("done")])), None)
        .await;
    assert_eq!(code, 0, "{err}");
    let configuration = recorded_configuration(&f, &err);
    assert_eq!(configuration["compaction"]["strategy"], "structured_state");
    // The model-based selection keeps its independent summary model and enables
    // the automatic trigger exactly as before this step.
    assert_eq!(
        configuration["compaction"]["summary_model"]["model"],
        "summary"
    );
    assert_eq!(configuration["compaction"]["automatic"], true);

    // An unflagged new session records the shared client creation default, and
    // model-free masking enables no automatic model-based work.
    let f = Fixture::new();
    assert_eq!(
        f.options.compaction_strategy,
        StrategyId::ObservationMasking
    );
    let (code, _, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("done")])), None)
        .await;
    assert_eq!(code, 0, "{err}");
    let configuration = recorded_configuration(&f, &err);
    assert_eq!(
        configuration["compaction"]["strategy"],
        "observation_masking"
    );
    assert!(configuration["compaction"]["summary_model"].is_null());
    assert_eq!(configuration["compaction"]["automatic"], false);
}

#[tokio::test]
async fn continuation_refuses_a_compaction_strategy_change() {
    let mut f = Fixture::new();
    let (_, _, first) = f
        .run(Arc::new(ScriptedProvider::new([reply("first")])), None)
        .await;
    let session = f.snapshot(&first).session_id().clone();
    f.options.session = Some(session.clone());
    f.options.compaction_strategy = StrategyId::StructuredState;
    f.options.explicit_strategy = true;
    let before = recorded_configuration(&f, &first);
    let (code, out, err) = f.run(Arc::new(ScriptedProvider::new([])), None).await;
    assert_eq!(code, 2, "{err}");
    assert!(out.is_empty());
    assert!(err.contains("session_policy_mismatch"), "{err}");
    // A refused continuation writes nothing: the recorded policy is unchanged.
    assert_eq!(recorded_configuration(&f, &first), before);

    // Naming the recorded strategy (or none at all) continues normally; the
    // omitted flag keeps the recorded masking strategy, not the legacy default.
    f.options.compaction_strategy = StrategyId::ObservationMasking;
    let (code, _, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("second")])), None)
        .await;
    assert_eq!(code, 0, "{err}");
    f.options.explicit_strategy = false;
    f.options.compaction_strategy = harness::wiring::creation_compaction_strategy();
    let (code, _, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("third")])), None)
        .await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        recorded_configuration(&f, &err)["compaction"]["strategy"],
        "observation_masking"
    );
    // Neither successful continuation appended a configuration change; only
    // turns were recorded.
    assert_eq!(configuration_changes(&journal_bytes(&f, &session)), 0);
}

fn recorded_configuration(f: &Fixture, err: &str) -> Value {
    let snapshot = f.snapshot(err);
    let root = DataRoot::open(&f.options.data_root, false).unwrap();
    serde_json::from_slice(
        &root
            .read_artifact(snapshot.configuration(), 1024 * 1024)
            .unwrap(),
    )
    .unwrap()
}

fn journal_bytes(f: &Fixture, session: &SessionId) -> Vec<u8> {
    fs::read(
        DataRoot::open(&f.options.data_root, false)
            .unwrap()
            .path()
            .join("sessions")
            .join(format!("{session}.jsonl")),
    )
    .unwrap()
}

/// A pre-M6 session: its configuration has no `compaction` object at all, which
/// is exactly what a client wrote when the strategy field did not exist.
async fn legacy_session_without_a_strategy(f: &Fixture) -> SessionId {
    let root = Arc::new(DataRoot::open(&f.options.data_root, true).unwrap());
    let bytes = serde_json::to_vec(&json!({
        "policy_revision": "ask-v1",
        "enabled_tools": ["read", "write", "edit"],
        "workspace_read": "ask",
        "workspace_mutation": "ask",
        "process": "ask",
        "network": "deny",
        "protected_paths": [],
        "allowances": []
    }))
    .unwrap();
    let artifact = root.put_artifact(&bytes).unwrap();
    let session = SessionId::new();
    let handle = SessionHandle::create(
        root,
        session.clone(),
        SessionHeader {
            workspace: f.options.workspace.to_str().unwrap().into(),
            initial_model: f.options.model.clone(),
            policy_revision: "ask-v1".into(),
            configuration: artifact,
        },
        Arc::new(ScriptedProvider::new([])),
        f.options.driver.clone(),
    )
    .await
    .unwrap();
    handle.shutdown().await;
    session
}

#[tokio::test]
async fn pre_search_session_keeps_recorded_tools_and_rejects_new_tools() {
    let mut f = Fixture::new();
    let session = legacy_session_without_a_strategy(&f).await;
    let before = journal_bytes(&f, &session);
    f.options.session = Some(session.clone());
    let provider = Arc::new(ScriptedProvider::new([
        proposal("glob", json!({"pattern":"*"})),
        reply("continued"),
    ]));
    let (code, _, err) = f.run(provider.clone(), None).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        recorded_configuration(&f, &err)["enabled_tools"],
        json!(["read", "write", "edit"])
    );
    assert!(journal_bytes(&f, &session).starts_with(&before));
    assert_eq!(configuration_changes(&journal_bytes(&f, &session)), 0);
    assert!(provider.streams().iter().all(|stream| {
        stream
            .request
            .input
            .tools
            .iter()
            .all(|definition| !matches!(definition.name.as_str(), "list" | "glob" | "grep"))
    }));
    assert!(f.snapshot(&err).entries().any(|(_, entry)| matches!(&entry.entry.payload,
        EntryPayload::ToolResult(result) if result.result.metadata.code.as_deref() == Some("tool_disabled"))));
}

fn configuration_changes(bytes: &[u8]) -> usize {
    String::from_utf8_lossy(bytes)
        .matches("\"configuration_changed\"")
        .count()
}

/// Falsification target: the client creation default and the legacy decoding
/// default are deliberately different. Flipping either one must fail here.
#[tokio::test]
async fn creation_default_and_legacy_decoding_are_distinct() {
    use harness_core::context::{CompactionConfig, SessionConfiguration};
    assert_eq!(
        harness::wiring::creation_compaction_strategy(),
        StrategyId::ObservationMasking,
        "a new session no longer defaults to the chosen strategy"
    );
    assert_eq!(
        StrategyId::default(),
        StrategyId::RollingSummary,
        "the decoding default flipped; old journals would be reinterpreted"
    );
    assert_eq!(
        CompactionConfig::default().strategy,
        StrategyId::RollingSummary
    );
    assert_eq!(
        CompactionConfig::from_value(None).unwrap().strategy,
        StrategyId::RollingSummary
    );
    for absent in [
        &b"{}"[..],
        br#"{"compaction":{}}"#,
        br#"{"compaction":{"summary_model":{"provider_instance":"summary","model":"cheap"}}}"#,
    ] {
        let artifact = ArtifactRef {
            sha256: Digest::of(absent),
            bytes: absent.len() as u64,
        };
        assert_eq!(
            SessionConfiguration::parse(artifact, absent)
                .unwrap()
                .compaction()
                .strategy,
            StrategyId::RollingSummary,
            "an omitted strategy must keep the legacy rolling_summary reading"
        );
    }
}

/// A recorded legacy session continues unchanged: the client's new default does
/// not rewrite its configuration, and an omitted flag keeps the recorded
/// strategy even though it differs from what a new session would record.
#[tokio::test]
async fn recorded_legacy_session_continues_without_an_implicit_strategy_change() {
    let mut f = Fixture::new();
    // Old clients recorded the baseline; this is the old creation behavior.
    f.options.compaction_strategy = StrategyId::RollingSummary;
    let (code, _, first) = f
        .run(Arc::new(ScriptedProvider::new([reply("first")])), None)
        .await;
    assert_eq!(code, 0, "{first}");
    let session = f.snapshot(&first).session_id().clone();
    let before = journal_bytes(&f, &session);

    f.options.session = Some(session.clone());
    f.options.compaction_strategy = harness::wiring::creation_compaction_strategy();
    f.options.explicit_strategy = false;
    let (code, _, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("second")])), None)
        .await;
    assert_eq!(code, 0, "{err}");
    let after = journal_bytes(&f, &session);
    assert!(
        after.starts_with(&before),
        "the continuation rewrote earlier journal bytes"
    );
    assert_eq!(configuration_changes(&after), 0);
    assert_eq!(
        recorded_configuration(&f, &err)["compaction"]["strategy"],
        "rolling_summary"
    );
}

/// The on-disk legacy omission: an old configuration without any strategy keeps
/// decoding as `rolling_summary` and continues without an appended
/// configuration change. `harness context` and the builder read the same bytes.
#[tokio::test]
async fn omitted_strategy_session_continues_as_rolling_summary_untouched() {
    use harness_core::context::SessionConfiguration;
    let mut f = Fixture::new();
    let session = legacy_session_without_a_strategy(&f).await;
    let before = journal_bytes(&f, &session);
    f.options.session = Some(session.clone());
    assert_eq!(
        f.options.compaction_strategy,
        StrategyId::ObservationMasking,
        "the new-session default must not be what the continuation compares"
    );
    assert!(!f.options.explicit_strategy);
    let (code, _, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("continued")])), None)
        .await;
    assert_eq!(code, 0, "{err}");
    let snapshot = f.snapshot(&err);
    assert_eq!(snapshot.session_id(), &session);
    let after = journal_bytes(&f, &session);
    assert!(after.starts_with(&before));
    assert_eq!(configuration_changes(&after), 0);
    let root = DataRoot::open(&f.options.data_root, false).unwrap();
    let bytes = root
        .read_artifact(snapshot.configuration(), 1024 * 1024)
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&bytes).contains("\"strategy\""),
        "the continuation added a strategy to an old configuration"
    );
    let configuration = SessionConfiguration::parse(snapshot.configuration().clone(), &bytes)
        .expect("the untouched legacy configuration still decodes");
    assert_eq!(
        configuration.compaction().strategy,
        StrategyId::RollingSummary
    );
    assert!(!configuration.compaction().automatic());
}

#[tokio::test]
async fn new_session_records_base_and_root_agents_and_sends_instructions_first() {
    let f = Fixture::new();
    fs::write(
        f.options.workspace.join("AGENTS.md"),
        "Use the project conventions.\n",
    )
    .unwrap();
    fs::create_dir(f.options.workspace.join("nested")).unwrap();
    fs::write(
        f.options.workspace.join("nested/AGENTS.md"),
        "Nested conventions.",
    )
    .unwrap();
    let provider = Arc::new(ScriptedProvider::new([reply("done")]));
    let (code, _, err) = f.run(provider.clone(), None).await;
    assert_eq!(code, 0, "{err}");
    let configuration = recorded_configuration(&f, &err);
    let instructions = configuration["instructions"].as_array().unwrap();
    assert_eq!(instructions.len(), 3);
    assert_eq!(instructions[0]["level"], "system");
    let base = instructions[0]["text"].as_str().unwrap();
    assert!(base.contains(f.options.workspace.to_str().unwrap()));
    let request = &provider.streams()[0].request;
    let actual_tools = request
        .input
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    assert!(
        base.contains(&format!("Available tools in this session: {actual_tools}.")),
        "{base}"
    );
    assert_eq!(
        instructions[1],
        json!({"level":"developer", "text":"Project instructions from AGENTS.md:\nUse the project conventions.\n"})
    );
    let ContextItem::Instructions {
        instructions: first,
    } = &request.input.items[0]
    else {
        panic!("instructions must be first")
    };
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        configuration["instructions"]
    );
    assert!(matches!(request.input.items[1], ContextItem::User { .. }));
    assert_eq!(
        instructions[2],
        json!({"level":"developer", "text":"Project instructions from nested/AGENTS.md (apply to files under nested/):\nNested conventions."})
    );
    assert!(!err.contains("warning:"));
}

#[tokio::test]
async fn missing_agents_gets_base_only_and_unavailable_sandbox_never_mentions_shell() {
    let f = Fixture::new();
    let provider = Arc::new(ScriptedProvider::new([reply("done")]));
    let (code, _, err) = f.run(provider, None).await;
    assert_eq!(code, 0, "{err}");
    let configuration = recorded_configuration(&f, &err);
    let instructions = configuration["instructions"].as_array().unwrap();
    assert_eq!(instructions.len(), 1);
    let base = instructions[0]["text"].as_str().unwrap();
    assert!(!base.contains("bash") && !base.contains("shell"), "{base}");
    assert!(base.contains("read, write, edit"));
    assert!(!err.contains("warning:"));
}

#[tokio::test]
async fn oversized_agents_is_skipped_with_64_kib_warning_not_truncated() {
    let f = Fixture::new();
    fs::write(
        f.options.workspace.join("AGENTS.md"),
        "x".repeat(64 * 1024 + 1),
    )
    .unwrap();
    let (code, _, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("done")])), None)
        .await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        recorded_configuration(&f, &err)["instructions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        err.contains("warning: AGENTS.md skipped: exceeds 64 KiB cap"),
        "{err}"
    );
    assert_eq!(
        err.lines()
            .filter(|line| line.starts_with("warning:"))
            .count(),
        1
    );
}

#[tokio::test]
async fn invalid_utf8_agents_is_skipped_with_warning() {
    let f = Fixture::new();
    fs::write(f.options.workspace.join("AGENTS.md"), b"\xff").unwrap();
    let (code, _, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("done")])), None)
        .await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        recorded_configuration(&f, &err)["instructions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        err.contains("warning: AGENTS.md skipped: not valid UTF-8"),
        "{err}"
    );
}

#[tokio::test]
async fn escaping_agents_symlink_is_skipped_with_warning() {
    let f = Fixture::new();
    let outside = f.temp.path().join("outside-agents");
    fs::write(&outside, "OUTSIDE-MUST-NOT-BE-READ").unwrap();
    std::os::unix::fs::symlink(&outside, f.options.workspace.join("AGENTS.md")).unwrap();
    let (code, _, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("done")])), None)
        .await;
    assert_eq!(code, 0, "{err}");
    let configuration = recorded_configuration(&f, &err);
    assert_eq!(configuration["instructions"].as_array().unwrap().len(), 1);
    assert!(
        !configuration
            .to_string()
            .contains("OUTSIDE-MUST-NOT-BE-READ")
    );
    assert!(
        err.contains("warning: AGENTS.md skipped: descriptor-rooted read refused"),
        "{err}"
    );
}

#[tokio::test]
async fn edited_agents_continuation_keeps_recorded_instructions_and_opens_fine() {
    let mut f = Fixture::new();
    fs::write(
        f.options.workspace.join("AGENTS.md"),
        "original instructions",
    )
    .unwrap();
    fs::create_dir(f.options.workspace.join("nested")).unwrap();
    fs::write(
        f.options.workspace.join("nested/AGENTS.md"),
        "original nested",
    )
    .unwrap();
    let (code, _, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("first")])), None)
        .await;
    assert_eq!(code, 0, "{err}");
    let configuration = recorded_configuration(&f, &err);
    f.options.session = Some(f.snapshot(&err).session_id().clone());
    fs::write(f.options.workspace.join("AGENTS.md"), "edited instructions").unwrap();
    fs::write(
        f.options.workspace.join("nested/AGENTS.md"),
        "edited nested",
    )
    .unwrap();
    fs::create_dir_all(f.options.workspace.join("new/deep")).unwrap();
    fs::write(
        f.options.workspace.join("new/deep/AGENTS.md"),
        "new instructions",
    )
    .unwrap();
    let provider = Arc::new(ScriptedProvider::new([reply("second")]));
    let (code, _, err) = f.run(provider.clone(), None).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(recorded_configuration(&f, &err), configuration);
    let ContextItem::Instructions { instructions } = &provider.streams()[0].request.input.items[0]
    else {
        panic!("missing instructions")
    };
    assert!(instructions[1].text.contains("original instructions"));
    assert!(!instructions[1].text.contains("edited instructions"));
    assert_eq!(instructions.len(), 3);
    assert!(instructions[2].text.ends_with("original nested"));
    assert!(!err.contains("warning:"));
}

#[tokio::test]
async fn agents_exactly_64_kib_is_not_truncated_or_skipped() {
    let f = Fixture::new();
    let text = "é".repeat(32 * 1024);
    fs::write(f.options.workspace.join("AGENTS.md"), &text).unwrap();
    let (code, _, err) = f
        .run(Arc::new(ScriptedProvider::new([reply("done")])), None)
        .await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        recorded_configuration(&f, &err)["instructions"][1]["text"],
        format!("Project instructions from AGENTS.md:\n{text}")
    );
    assert!(!err.contains("warning:"));
}
