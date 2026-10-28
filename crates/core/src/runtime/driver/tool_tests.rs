//! End-to-end tools through the production handle and real storage worker.
use super::*;
use crate::tools::{
    AuthorizedOperation, CancelToken, ExecutionOutcome, Executor, FileFacts, Refusal,
    WorkspaceIdentity, WorkspacePath, WorkspaceView, tests::FakeWorkspace,
};
use serde_json::{Value, json};
use std::sync::{Condvar, Mutex, atomic::AtomicBool};

#[path = "tool_recovery_tests.rs"]
mod recovery;

const POLICY: &[u8] = br#"{"policy_revision":"initial","enabled_tools":["read","write","edit","bash"],"workspace_read":"allow","workspace_mutation":"ask","process":"ask","network":"deny","protected_paths":[],"allowances":[]}"#;
const DENY: &[u8] = br#"{"policy_revision":"initial","enabled_tools":["read","write","edit","bash"],"workspace_read":"deny","workspace_mutation":"deny","process":"deny","network":"deny","protected_paths":[],"allowances":[]}"#;

#[derive(Default)]
struct Hold {
    entered: AtomicBool,
    released: Mutex<bool>,
    wake: Condvar,
    saw_cancel: AtomicBool,
}
impl Hold {
    fn wait(&self, cancel: Option<&CancelToken>, cancel_releases: bool) {
        self.entered.store(true, Ordering::Release);
        let mut released = self.released.lock().unwrap();
        while !*released {
            if cancel.is_some_and(|c| c.is_cancelled()) {
                self.saw_cancel.store(true, Ordering::Release);
                if cancel_releases {
                    break;
                }
            }
            released = self
                .wake
                .wait_timeout(released, Duration::from_millis(1))
                .unwrap()
                .0;
        }
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
    async fn entered(&self) {
        bounded(async {
            while !self.entered.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
}

struct Workspace {
    identity: WorkspaceIdentity,
    fake: Mutex<FakeWorkspace>,
    hold: Option<Arc<Hold>>,
    panic: bool,
}
impl Workspace {
    fn new() -> Self {
        let mut fake = FakeWorkspace::new();
        fake.identity = WorkspaceIdentity("/workspace".into());
        Self {
            identity: fake.identity.clone(),
            fake: Mutex::new(fake),
            hold: None,
            panic: false,
        }
    }
}
impl WorkspaceView for Workspace {
    fn identity(&self) -> &WorkspaceIdentity {
        &self.identity
    }
    fn inspect(&self, path: &WorkspacePath) -> Result<FileFacts, Refusal> {
        assert_eq!(std::thread::current().name(), Some("harness-tools"));
        if let Some(hold) = &self.hold {
            hold.wait(None, false);
        }
        assert!(!self.panic, "planning panic requested");
        self.fake.lock().unwrap().inspect(path)
    }
    fn read_text(&self, path: &WorkspacePath, max: u64) -> Result<String, Refusal> {
        self.fake.lock().unwrap().read_text(path, max)
    }
}
struct Stub {
    backend: ExecutionBackend,
    counted: crate::runtime::tests::pipeline::StubExecutor,
    hold: Option<Arc<Hold>>,
    cancel_releases: bool,
    panic: AtomicBool,
    wrong_backend: AtomicBool,
}
impl Stub {
    fn new(backend: ExecutionBackend) -> Self {
        Self {
            backend,
            counted: Default::default(),
            hold: None,
            cancel_releases: true,
            panic: AtomicBool::new(false),
            wrong_backend: AtomicBool::new(false),
        }
    }
    fn count(&self) -> usize {
        self.counted.invocations.lock().unwrap().len()
    }
}
impl Executor for Stub {
    fn backend(&self) -> ExecutionBackend {
        if self.wrong_backend.load(Ordering::Acquire) {
            ExecutionBackend::Bubblewrap
        } else {
            self.backend
        }
    }
    fn execute(&self, operation: &AuthorizedOperation, cancel: &CancelToken) -> ExecutionOutcome {
        assert_eq!(std::thread::current().name(), Some("harness-tools"));
        let success = self.counted.execute(operation, cancel);
        if let Some(hold) = &self.hold {
            hold.wait(Some(cancel), self.cancel_releases);
        }
        assert!(
            !self.panic.load(Ordering::Acquire),
            "execution panic requested"
        );
        let cancelled = cancel.is_cancelled() && self.cancel_releases;
        if cancelled { outcome(true) } else { success }
    }
}
fn outcome(cancelled: bool) -> ExecutionOutcome {
    crate::runtime::tests::pipeline::outcome(
        if cancelled {
            ToolStatus::Cancelled
        } else {
            ToolStatus::Succeeded
        },
        cancelled.then_some(ResultCode::Cancelled),
    )
}
fn host(workspace: Workspace, executors: Vec<Arc<dyn Executor>>) -> ToolHost {
    ToolHost {
        workspace: Arc::new(workspace),
        executors,
    }
}
async fn fixture(
    host: ToolHost,
    policy: &'static [u8],
    calls: &[(&str, Value)],
) -> (Fixture, Arc<ScriptedProvider>) {
    let mut config = config();
    config.tools = host;
    let provider = Arc::new(ScriptedProvider::new([
        proposal(calls),
        reply("done"),
        proposal(&[("read", json!({"path":"a"}))]),
        reply("done again"),
    ]));
    let fixture = Fixture::with_configuration(
        config,
        WorkerOptions::default(),
        Some(provider.clone()),
        policy,
    )
    .await;
    (fixture, provider)
}
fn proposal(calls: &[(&str, Value)]) -> Script {
    let mut steps = vec![Step::Event(ProviderEvent::Started)];
    for (i, (name, args)) in calls.iter().enumerate() {
        let block_index = i as u32;
        steps.extend([
            Step::Event(ProviderEvent::ToolCallStarted {
                block_index,
                name: (*name).into(),
                provider_call_id: None,
            }),
            Step::Event(ProviderEvent::ToolArgumentsDelta {
                block_index,
                fragment: args.to_string(),
            }),
            Step::Event(ProviderEvent::ToolCallReady { block_index }),
        ]);
    }
    steps.push(Step::completed(FinishReason::ToolCalls));
    Script::new(steps)
}
// Real-time bound: paused Tokio time must not auto-advance while workers run.
async fn bounded<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("bounded tool test timed out")
}
async fn start(f: &Fixture) -> TurnId {
    let Accepted::TurnStarted { turn_id, .. } = bounded(f.handle.command(Fixture::command(1)))
        .await
        .unwrap()
    else {
        panic!("start")
    };
    turn_id
}
async fn approve(f: &Fixture, remember: bool, decision: Decision) {
    let view = bounded(f.wait(|v| v.pending_approval.is_some())).await;
    let p = view.pending_approval.unwrap();
    assert!(matches!(
        bounded(f.handle.command(Command::SubmitApproval {
            approval_id: p.approval_id,
            plan_digest: p.plan_digest,
            decision,
            remember
        }))
        .await,
        Ok(Accepted::ApprovalSubmitted { .. })
    ));
}
fn results(view: &RuntimeView) -> Vec<&ToolResult> {
    view.snapshot
        .entries()
        .filter_map(|(_, stored)| match &stored.entry.payload {
            EntryPayload::ToolResult(r) => Some(&r.result),
            _ => None,
        })
        .collect()
}
async fn inject(handle: &SessionHandle, input: Input) {
    let (send, recv) = oneshot::channel();
    handle
        .commands
        .send(Message::Inject(input, send))
        .await
        .unwrap();
    recv.await.unwrap();
}
async fn fault(handle: &SessionHandle) {
    inject(handle, Input::DriverInvariant { code: "test_fault" }).await;
}

#[tokio::test(flavor = "current_thread")]
async fn approved_write_events_are_durable_and_result_reaches_next_inference() {
    let executor = Arc::new(Stub::new(ExecutionBackend::TrustedFs));
    let (f, provider) = fixture(
        host(Workspace::new(), vec![executor.clone()]),
        POLICY,
        &[("write", json!({"path":"new","text":"hello"}))],
    )
    .await;
    let mut subscription = f.handle.subscribe();
    subscription.next().await.unwrap();
    start(&f).await;
    let pending = bounded(f.wait(|v| v.pending_approval.is_some())).await;
    let mut late = f.handle.subscribe();
    let Some(SubscriptionItem::Reset(late_view)) = bounded(late.next()).await else {
        panic!("reset")
    };
    assert_eq!(late_view.view().pending_approval, pending.pending_approval);
    approve(&f, false, Decision::Allow).await;
    let view = bounded(f.wait(idle)).await;
    assert_eq!(executor.count(), 1);
    assert_eq!(results(&view)[0].status, ToolStatus::Succeeded);
    assert!(provider.streams()[1].request.input.items.iter().any(|i| matches!(i, ContextItem::ToolResult { result, .. } if result.content == outcome(false).content)));
    let mut seen = Vec::new();
    let mut durable = ClientView::from(pending.clone());
    // Subscribe from before start, and verify facts against the last Committed snapshot.
    while seen.len() < 4 {
        let Some(SubscriptionItem::Event(event)) = bounded(subscription.next()).await else {
            panic!("event")
        };
        if let EventKind::Committed { snapshot, .. } = &event.kind {
            durable = ClientView::from(RuntimeView {
                snapshot: snapshot.clone(),
                ..pending.clone()
            });
        }
        match &event.kind {
            EventKind::ApprovalRequested { approval_id, .. } => {
                assert!(durable.view().snapshot.events().any(|(_, _, e)| matches!(e, JournalEvent::ApprovalRequested(p) if &p.approval_id == approval_id)));
                seen.push("requested");
            }
            EventKind::ApprovalResolved { .. } => {
                assert!(durable.view().snapshot.events().any(|(_, _, e)| matches!(e, JournalEvent::AuthorizationDecided(d) if d.approval_id.is_some())));
                seen.push("resolved");
            }
            EventKind::ToolStarted { invocation_id, .. } => {
                assert!(
                    durable
                        .view()
                        .snapshot
                        .unresolved_invocations()
                        .iter()
                        .any(|i| &i.intent.invocation_id == invocation_id)
                );
                seen.push("started");
            }
            EventKind::ToolFinished { .. } => {
                assert_eq!(results(durable.view()).len(), 1);
                seen.push("finished");
            }
            _ => {}
        }
    }
    assert_eq!(seen, ["requested", "resolved", "started", "finished"]);
    f.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn remembered_write_and_bubblewrap_execute_sequentially() {
    let fs = Arc::new(Stub::new(ExecutionBackend::TrustedFs));
    let shell = Arc::new(Stub::new(ExecutionBackend::Bubblewrap));
    let write = json!({"path":"new","text":"hello"});
    let (f, _) = fixture(
        host(Workspace::new(), vec![fs.clone(), shell.clone()]),
        POLICY,
        &[
            ("write", write.clone()),
            ("write", write),
            ("bash", json!({"command":"true"})),
        ],
    )
    .await;
    start(&f).await;
    approve(&f, true, Decision::Allow).await;
    let p = bounded(f.wait(|v| {
        v.pending_approval
            .as_ref()
            .is_some_and(|p| serde_json::from_slice::<Value>(&p.plan).unwrap()["name"] == "bash")
    }))
    .await;
    assert_eq!(fs.count(), 2);
    assert_eq!(shell.count(), 0);
    assert!(p.pending_approval.is_some());
    approve(&f, false, Decision::Allow).await;
    let view = bounded(f.wait(idle)).await;
    assert_eq!(shell.count(), 1);
    assert_eq!(results(&view).len(), 3);
    f.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn denied_unknown_and_policy_denied_never_execute() {
    for (policy, name, args, code) in [
        (
            POLICY,
            "write",
            json!({"path":"new","text":"hi"}),
            ResultCode::ApprovalDenied,
        ),
        (POLICY, "unknown", json!({}), ResultCode::UnknownTool),
        (DENY, "read", json!({"path":"a"}), ResultCode::PolicyDenied),
    ] {
        let executor = Arc::new(Stub::new(ExecutionBackend::TrustedFs));
        let (f, _) = fixture(
            host(Workspace::new(), vec![executor.clone()]),
            policy,
            &[(name, args)],
        )
        .await;
        start(&f).await;
        if code == ResultCode::ApprovalDenied {
            approve(&f, false, Decision::Deny).await;
        }
        let view = bounded(f.wait(idle)).await;
        assert_eq!(
            results(&view)[0].metadata.code.as_deref(),
            Some(code.as_str())
        );
        assert_eq!(executor.count(), 0);
        f.handle.shutdown().await;
    }
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn approval_real_timer_expires_without_execution() {
    let executor = Arc::new(Stub::new(ExecutionBackend::TrustedFs));
    let (f, _) = fixture(
        host(Workspace::new(), vec![executor.clone()]),
        POLICY,
        &[("write", json!({"path":"new","text":"hi"}))],
    )
    .await;
    start(&f).await;
    f.wait(|v| v.pending_approval.is_some()).await;
    tokio::time::advance(Duration::from_secs(301)).await;
    let view = f.wait(idle).await;
    assert_eq!(
        results(&view)[0].metadata.code.as_deref(),
        Some(ResultCode::ApprovalExpired.as_str())
    );
    assert_eq!(executor.count(), 0);
    f.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_while_executing_reaches_token_and_records_cancelled() {
    let hold = Arc::new(Hold::default());
    let mut stub = Stub::new(ExecutionBackend::TrustedFs);
    stub.hold = Some(hold.clone());
    let executor = Arc::new(stub);
    let (f, _) = fixture(
        host(Workspace::new(), vec![executor.clone()]),
        POLICY,
        &[("read", json!({"path":"a"}))],
    )
    .await;
    let turn_id = start(&f).await;
    hold.entered().await;
    bounded(f.handle.view()).await.unwrap();
    let mut sub = f.handle.subscribe();
    bounded(sub.next()).await.unwrap();
    assert!(matches!(
        bounded(f.handle.command(Command::Cancel { turn_id })).await,
        Ok(Accepted::Cancelling { .. })
    ));
    let view = bounded(f.wait(idle)).await;
    assert_eq!(results(&view)[0].status, ToolStatus::Cancelled);
    assert!(hold.saw_cancel.load(Ordering::Acquire));
    assert_eq!(executor.count(), 1);
    f.handle.shutdown().await;
}
#[tokio::test(flavor = "current_thread")]
async fn completed_execution_wins_cancel_race() {
    let hold = Arc::new(Hold::default());
    let mut stub = Stub::new(ExecutionBackend::TrustedFs);
    stub.hold = Some(hold.clone());
    stub.cancel_releases = false;
    let executor = Arc::new(stub);
    let (f, _) = fixture(
        host(Workspace::new(), vec![executor]),
        POLICY,
        &[("read", json!({"path":"a"}))],
    )
    .await;
    let turn_id = start(&f).await;
    hold.entered().await;
    hold.release();
    bounded(f.handle.command(Command::Cancel { turn_id }))
        .await
        .unwrap();
    let view = bounded(f.wait(idle)).await;
    assert_eq!(results(&view)[0].status, ToolStatus::Succeeded);
    f.handle.shutdown().await;
}
#[tokio::test(flavor = "current_thread")]
async fn slow_planning_does_not_block_views_subscriptions_or_cancel() {
    let hold = Arc::new(Hold::default());
    let mut workspace = Workspace::new();
    workspace.hold = Some(hold.clone());
    let executor = Arc::new(Stub::new(ExecutionBackend::TrustedFs));
    let (f, _) = fixture(
        host(workspace, vec![executor.clone()]),
        POLICY,
        &[("read", json!({"path":"a"}))],
    )
    .await;
    let turn_id = start(&f).await;
    hold.entered().await;
    bounded(f.handle.view()).await.unwrap();
    let mut sub = f.handle.subscribe();
    bounded(sub.next()).await.unwrap();
    bounded(f.handle.command(Command::Cancel { turn_id }))
        .await
        .unwrap();
    bounded(f.wait(idle)).await;
    hold.release();
    f.handle.shutdown().await;
    assert_eq!(executor.count(), 0);
}
#[tokio::test(flavor = "current_thread")]
async fn executor_lost_read_closes_by_d9_after_reopen_and_never_replayed() {
    let executor = Arc::new(Stub::new(ExecutionBackend::TrustedFs));
    executor.panic.store(true, Ordering::Release);
    let (f, _) = fixture(
        host(Workspace::new(), vec![executor.clone()]),
        POLICY,
        &[("read", json!({"path":"a"}))],
    )
    .await;
    start(&f).await;
    let faulted = bounded(f.wait(|v| v.phase == Phase::Faulted)).await;
    assert_eq!(results(&faulted).len(), 0);
    assert_eq!(executor.count(), 1);
    bounded(f.handle.reopen()).await.unwrap();
    let view = bounded(f.wait(idle)).await;
    assert!(view.awaiting.is_empty());
    assert_eq!(executor.count(), 1);
    assert_eq!(results(&view)[0].status, ToolStatus::Cancelled);
    assert_eq!(
        results(&view)[0].metadata.code.as_deref(),
        Some("process_restart")
    );
    assert!(view.snapshot.events().all(|(_, _, e)| !matches!(
        e,
        JournalEvent::ExecutionUncertain(_) | JournalEvent::RecoveryResolved(_)
    )));
    f.handle.shutdown().await;
}
#[tokio::test(flavor = "current_thread")]
async fn missing_executor_backend_keeps_intent_uncertain() {
    let executor = Arc::new(Stub::new(ExecutionBackend::TrustedFs));
    let (f, _) = fixture(
        host(Workspace::new(), vec![executor.clone()]),
        POLICY,
        &[("write", json!({"path":"new","text":"hello"}))],
    )
    .await;
    start(&f).await;
    bounded(f.wait(|v| v.pending_approval.is_some())).await;
    executor.wrong_backend.store(true, Ordering::Release);
    approve(&f, false, Decision::Allow).await;
    let view = bounded(f.wait(|v| v.phase == Phase::Faulted)).await;
    assert!(results(&view).is_empty());
    assert_eq!(view.snapshot.unresolved_invocations().len(), 1);
    assert_eq!(executor.count(), 0);
    f.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn planning_panic_is_an_honest_nonexecuted_refusal() {
    let mut workspace = Workspace::new();
    workspace.panic = true;
    let executor = Arc::new(Stub::new(ExecutionBackend::TrustedFs));
    let (f, _) = fixture(
        host(workspace, vec![executor.clone()]),
        POLICY,
        &[("read", json!({"path":"a"}))],
    )
    .await;
    start(&f).await;
    let view = bounded(f.wait(idle)).await;
    assert_eq!(
        results(&view)[0].metadata.code.as_deref(),
        Some(ResultCode::InfrastructureFailure.as_str())
    );
    assert_eq!(results(&view)[0].invocation_id, None);
    assert_eq!(executor.count(), 0);
    f.handle.shutdown().await;
}
#[tokio::test(flavor = "current_thread")]
async fn shutdown_waits_for_executor_cleanup_and_thread_exit() {
    let hold = Arc::new(Hold::default());
    let mut stub = Stub::new(ExecutionBackend::TrustedFs);
    stub.hold = Some(hold.clone());
    stub.cancel_releases = false;
    let executor = Arc::new(stub);
    let (f, _) = fixture(
        host(Workspace::new(), vec![executor.clone()]),
        POLICY,
        &[("read", json!({"path":"a"}))],
    )
    .await;
    start(&f).await;
    hold.entered().await;
    let handle = f.handle.clone();
    let shutdown = tokio::spawn(async move {
        handle.shutdown().await;
    });
    bounded(async {
        while !hold.saw_cancel.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(!shutdown.is_finished());
    hold.release();
    bounded(shutdown).await.unwrap();
    assert_eq!(executor.count(), 1);
    let (worker, _) = StorageWorker::open(f.root.clone(), f.session.clone())
        .await
        .unwrap();
    worker.shutdown().await;
}
#[tokio::test(flavor = "current_thread")]
async fn reopen_during_execution_waits_before_reporting_uncertainty() {
    let hold = Arc::new(Hold::default());
    let mut stub = Stub::new(ExecutionBackend::TrustedFs);
    stub.hold = Some(hold.clone());
    stub.cancel_releases = false;
    let executor = Arc::new(stub);
    let (f, _) = fixture(
        host(Workspace::new(), vec![executor.clone()]),
        POLICY,
        &[("write", json!({"path":"new","text":"hello"}))],
    )
    .await;
    start(&f).await;
    approve(&f, false, Decision::Allow).await;
    hold.entered().await;
    fault(&f.handle).await;
    let handle = f.handle.clone();
    let mut reopen = tokio::spawn(async move { handle.reopen().await });
    // Wait long enough to distinguish storage restart from executor cleanup.
    // Release even on mutation failure, so this test always fails cleanly.
    let early = tokio::time::timeout(Duration::from_millis(100), &mut reopen).await;
    hold.release();
    assert!(early.is_err(), "reopen must not race a live side effect");
    bounded(reopen).await.unwrap().unwrap();
    let view = bounded(f.wait(|v| v.phase == Phase::AwaitingResolution)).await;
    assert_eq!(view.awaiting.len(), 1);
    assert_eq!(executor.count(), 1);
    assert!(results(&view).is_empty());
    f.handle.shutdown().await;
}
#[tokio::test(flavor = "current_thread")]
async fn tools_survive_reopen() {
    let executor = Arc::new(Stub::new(ExecutionBackend::TrustedFs));
    let (f, _) = fixture(
        host(Workspace::new(), vec![executor.clone()]),
        POLICY,
        &[("read", json!({"path":"a"}))],
    )
    .await;
    start(&f).await;
    bounded(f.wait(idle)).await;
    assert_eq!(executor.count(), 1);
    fault(&f.handle).await;
    bounded(f.handle.reopen()).await.unwrap();
    bounded(f.wait(idle)).await;
    bounded(f.handle.command(Fixture::command(2)))
        .await
        .unwrap();
    let view = bounded(f.wait(idle)).await;
    assert_eq!(executor.count(), 2);
    assert_eq!(results(&view).len(), 2);
    f.handle.shutdown().await;
}
#[tokio::test(flavor = "current_thread")]
async fn stale_executor_answer_cannot_alias_current_runtime_tokens() {
    let hold = Arc::new(Hold::default());
    let mut workspace = Workspace::new();
    workspace.hold = Some(hold.clone());
    let executor = Arc::new(Stub::new(ExecutionBackend::TrustedFs));
    let (f, _) = fixture(
        host(workspace, vec![executor.clone()]),
        POLICY,
        &[("read", json!({"path":"a"}))],
    )
    .await;
    start(&f).await;
    hold.entered().await;
    let before = f.handle.view().await.unwrap();
    let (send, recv) = oneshot::channel();
    f.handle
        .commands
        .send(Message::ToolAnswer(
            Answer {
                epoch: RuntimeEpoch::new(),
                token: JobToken::Execute(ExecutionToken(1)),
                input: None,
            },
            send,
        ))
        .await
        .unwrap();
    recv.await.unwrap();
    let after = f.handle.view().await.unwrap();
    assert_eq!(after.phase, before.phase);
    assert_eq!(after.event_seq, before.event_seq);
    hold.release();
    bounded(f.wait(idle)).await;
    assert_eq!(executor.count(), 1);
    f.handle.shutdown().await;
}
#[tokio::test(flavor = "current_thread")]
async fn duplicate_backends_fail_open_and_create_before_io() {
    let f = Fixture::new(config()).await;
    f.handle.shutdown().await;
    let executor = Arc::new(Stub::new(ExecutionBackend::TrustedFs));
    let mut config = config();
    config.tools = host(Workspace::new(), vec![executor.clone(), executor]);
    f.root.io.test.lock().unwrap().trace.clear();
    let provider = Arc::new(ScriptedProvider::new([]));
    assert!(
        matches!(SessionHandle::open(f.root.clone(), f.session.clone(), provider.clone(), config.clone()).await, Err(PersistError::Rejected { code }) if code == "duplicate_executor_backend")
    );
    let header = SessionHeader {
        workspace: "/workspace".into(),
        initial_model: test_model(),
        policy_revision: "initial".into(),
        configuration: ArtifactRef {
            sha256: Digest::of(b"{}"),
            bytes: 2,
        },
    };
    assert!(
        matches!(SessionHandle::create(f.root.clone(), SessionId::new(), header, provider, config).await, Err(PersistError::Rejected { code }) if code == "duplicate_executor_backend")
    );
    assert!(f.root.io.test.lock().unwrap().trace.is_empty());
}
