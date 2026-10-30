//! SessionHandle + real FsWorkspace/Sandbox integration. Runs in the S02 gate.
use harness_core::{
    context::ModelLimits,
    model::{
        ProviderEvent,
        scripted::{Script, ScriptedProvider, Step},
    },
    runtime::*,
    session::*,
    tools::{PolicySnapshot, SandboxConfig, ToolSet},
};
use serde_json::{Value, json};
use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
    process::{Child, Command as Process},
    sync::Arc,
    time::{Duration, Instant},
};

#[path = "sandbox/assertions.rs"]
mod assertions;
#[path = "../../../tests/support/loopback.rs"]
mod loopback;

fn assert_status(result: &ToolResult, expected: ToolStatus) {
    assertions::assert_status(
        result.status,
        expected,
        &result.metadata.code,
        &result.content,
    );
}

fn policy(paths: &[&str]) -> Vec<u8> {
    serde_json::to_vec(
        &json!({"policy_revision":"p1", "enabled_tools":["read","write","edit","bash"],
        "workspace_read":"allow", "workspace_mutation":"ask", "process":"ask", "network":"deny",
        "protected_paths":paths, "allowances":[]}),
    )
    .unwrap()
}
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
            context_tokens: Some(128_000),
            max_output_tokens: None,
        },
    );
    config
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
fn reply() -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, "done"),
        Step::completed(FinishReason::Stop),
    ])
}
async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .expect("tool host test deadline")
}
async fn wait(handle: &SessionHandle, predicate: impl Fn(&RuntimeView) -> bool) -> RuntimeView {
    bounded(async {
        loop {
            let view = handle.view().await.unwrap();
            assert_ne!(view.phase, Phase::Faulted, "unexpected fault: {view:?}");
            if predicate(&view) {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
}
fn idle(view: &RuntimeView) -> bool {
    view.phase == Phase::Durable(RuntimeState::Idle) && view.active_turn.is_none()
}
fn results(view: &RuntimeView) -> Vec<&ToolResult> {
    view.snapshot
        .entries()
        .filter_map(|(_, entry)| match &entry.entry.payload {
            EntryPayload::ToolResult(result) => Some(&result.result),
            _ => None,
        })
        .collect()
}
fn text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}
async fn start(handle: &SessionHandle) -> TurnId {
    let Accepted::TurnStarted { turn_id, .. } = bounded(handle.command(Command::StartTurn {
        command_id: CommandId::new(),
        content: vec![ContentBlock::Text { text: "go".into() }],
    }))
    .await
    .unwrap() else {
        panic!("turn not started")
    };
    turn_id
}
async fn approve(handle: &SessionHandle, remember: bool) {
    let pending = wait(handle, |v| v.pending_approval.is_some())
        .await
        .pending_approval
        .unwrap();
    assert!(matches!(
        bounded(handle.command(Command::SubmitApproval {
            approval_id: pending.approval_id,
            plan_digest: pending.plan_digest,
            decision: Decision::Allow,
            remember,
        }))
        .await,
        Ok(Accepted::ApprovalSubmitted { .. })
    ));
}
struct Fixture {
    _temp: tempfile::TempDir,
    root: Arc<DataRoot>,
    host: ToolHostConfig,
    session: SessionId,
    header: SessionHeader,
}
impl Fixture {
    fn new(paths: &[&str]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("workspace")).unwrap();
        let root = Arc::new(DataRoot::open(temp.path().join("data"), true).unwrap());
        let host = ToolHostConfig {
            workspace_root: temp.path().join("workspace"),
            data_root: root.path().into(),
            sandbox: Some(SandboxConfig::default()),
        };
        fs::create_dir(host.workspace_root.join(".git")).unwrap();
        fs::create_dir(host.workspace_root.join("private")).unwrap();
        fs::write(host.workspace_root.join(".git/config"), "git-secret").unwrap();
        fs::write(host.workspace_root.join("private/token"), "private-secret").unwrap();
        let header = SessionHeader {
            workspace: host.workspace_root.to_str().unwrap().into(),
            initial_model: model(),
            policy_revision: "p1".into(),
            configuration: root.put_artifact(&policy(paths)).unwrap(),
        };
        Self {
            _temp: temp,
            root,
            host,
            session: SessionId::new(),
            header,
        }
    }
    async fn create(&self, provider: Arc<ScriptedProvider>) -> SessionHandle {
        bounded(SessionHandle::create_with_tools(
            self.root.clone(),
            self.session.clone(),
            self.header.clone(),
            provider,
            config(),
            &self.host,
        ))
        .await
        .unwrap()
    }
    async fn open(&self, provider: Arc<ScriptedProvider>) -> SessionHandle {
        bounded(SessionHandle::open_with_tools(
            self.root.clone(),
            self.session.clone(),
            provider,
            config(),
            &self.host,
        ))
        .await
        .unwrap()
    }
    async fn run(&self, name: &str, args: Value) -> (SessionHandle, RuntimeView) {
        let handle = self
            .create(Arc::new(ScriptedProvider::new([
                proposal(name, args),
                reply(),
            ])))
            .await;
        assert!(
            handle.tool_availability().unwrap().shell.is_available(),
            "{:?}",
            handle.tool_availability()
        );
        start(&handle).await;
        approve(&handle, false).await;
        let view = wait(&handle, idle).await;
        (handle, view)
    }
}
const NAMESPACE: &str = "readlink /proc/self/ns/pid > namespace; ";
async fn wait_file(path: &Path) {
    bounded(async {
        while fs::read(path).unwrap_or_default().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
}
fn namespace_processes(root: &Path) -> Vec<PathBuf> {
    let namespace = PathBuf::from(
        fs::read_to_string(root.join("namespace"))
            .expect("workload started")
            .trim(),
    );
    assert!(!namespace.as_os_str().is_empty());
    fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|entry| {
            fs::read_link(entry.path().join("ns/pid"))
                .ok()
                .filter(|link| link == &namespace)
                .map(|_| entry.path())
        })
        .collect()
}
async fn assert_namespace_gone(root: &Path) {
    bounded(async {
        loop {
            let processes = namespace_processes(root);
            if processes.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(namespace_processes(root).is_empty());
}

#[tokio::test]
#[ignore = "S02 real Linux sandbox"]
async fn s02_edit_approval_and_remember_preserve_policy() {
    let f = Fixture::new(&["private"]);
    fs::write(f.host.workspace_root.join("a"), "before\n").unwrap();
    let handle = f
        .create(Arc::new(ScriptedProvider::new([
            proposal(
                "edit",
                json!({"path":"a","old_text":"before","new_text":"after"}),
            ),
            reply(),
            proposal(
                "edit",
                json!({"path":"a","old_text":"after","new_text":"again"}),
            ),
            reply(),
        ])))
        .await;
    start(&handle).await;
    assert_eq!(
        fs::read_to_string(f.host.workspace_root.join("a")).unwrap(),
        "before\n"
    );
    approve(&handle, true).await;
    let view = wait(&handle, idle).await;
    let result = results(&view)[0];
    assert_status(result, ToolStatus::Succeeded);
    assert!(text(result).contains(Digest::of(b"after\n").as_str()));
    let remembered: Value = serde_json::from_slice(
        &f.root
            .read_artifact(view.snapshot.configuration(), 1024 * 1024)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(remembered["protected_paths"], json!(["private"]));
    start(&handle).await;
    let view = wait(&handle, idle).await; // No second approval: remembered exact path.
    assert_eq!(results(&view).len(), 2);
    assert_eq!(
        fs::read_to_string(f.host.workspace_root.join("a")).unwrap(),
        "again\n"
    );
    handle.shutdown().await;
}

#[tokio::test]
#[ignore = "S02 real Linux sandbox"]
async fn s02_bash_approval_publishes_verified_capture() {
    let mut f = Fixture::new(&["private"]);
    // Assembly must overwrite this deliberately wrong sandbox workspace.
    f.host.sandbox.as_mut().unwrap().workspace_root = "/not/the/workspace".into();
    let (handle, view) = f
        .run("bash", json!({"command":"echo hi > out.txt; echo done"}))
        .await;
    assert_eq!(
        fs::read_to_string(f.host.workspace_root.join("out.txt")).unwrap(),
        "hi\n"
    );
    let result = results(&view)[0];
    assert_status(result, ToolStatus::Succeeded);
    assert!(text(result).contains("done"));
    let capture = &result.metadata.artifacts[0];
    let bytes = f.root.read_artifact(capture, 64 * 1024 * 1024).unwrap();
    assert_eq!(bytes, b"done\n");
    assert_eq!(capture.sha256, Digest::of(&bytes));
    handle.shutdown().await;
}

#[tokio::test]
#[ignore = "S02 real Linux sandbox"]
async fn s02_bash_protected_and_git_writes_fail_unchanged() {
    for target in [".git/config", "private/token"] {
        let f = Fixture::new(&["private"]);
        let original = fs::read(f.host.workspace_root.join(target)).unwrap();
        let (handle, view) = f
            .run(
                "bash",
                json!({"command":format!("echo changed > {target}")}),
            )
            .await;
        assert_status(results(&view)[0], ToolStatus::Failed);
        assert_eq!(
            fs::read(f.host.workspace_root.join(target)).unwrap(),
            original
        );
        handle.shutdown().await;
    }
}

#[tokio::test]
#[ignore = "S02 real Linux sandbox"]
async fn s02_bash_network_is_denied() {
    for address in ["127.0.0.1", "127.0.0.2"] {
        let listener = std::net::TcpListener::bind((address, 0)).unwrap();
        let side_probe = loopback::SideProbe::register(listener.local_addr().unwrap());
        let port = listener.local_addr().unwrap().port();
        let nonce = format!("leak-{}", loopback::nonce());
        let f = Fixture::new(&[]);
        let (handle, view) = f
            .run(
                "bash",
                json!({"command":format!("echo {nonce} > /dev/tcp/{address}/{port}")}),
            )
            .await;
        drop(side_probe);
        // Check the traffic independently, before checking the tool's result.
        loopback::assert_no_nonce(&listener, &nonce);
        assert_status(results(&view)[0], ToolStatus::Failed);
        let output = text(results(&view)[0]);
        assert!(
            output.contains("Connection refused") || output.contains("Network is unreachable"),
            "expected connect error: {output}"
        );
        handle.shutdown().await;
    }
}

#[tokio::test]
#[ignore = "S02 real Linux sandbox"]
async fn s02_bash_timeout_cleans_namespace() {
    let f = Fixture::new(&[]);
    let (handle, view) = f.run("bash", json!({"command":format!("{NAMESPACE}trap '' TERM; (trap '' TERM; sleep 30 & wait) & wait"), "timeout_ms":200})).await;
    assert_status(results(&view)[0], ToolStatus::TimedOut);
    assert_eq!(results(&view)[0].metadata.code.as_deref(), Some("timeout"));
    assert_namespace_gone(&f.host.workspace_root).await;
    handle.shutdown().await;
}

#[tokio::test]
#[ignore = "S02 real Linux sandbox"]
async fn s02_cancel_cleans_namespace_and_accepts_next_turn() {
    let f = Fixture::new(&[]);
    let handle = f.create(Arc::new(ScriptedProvider::new([
        proposal("bash", json!({"command":format!("{NAMESPACE}trap '' TERM; (trap '' TERM; sleep 30 & wait) & wait")})), reply(),
    ]))).await;
    let turn_id = start(&handle).await;
    approve(&handle, false).await;
    wait_file(&f.host.workspace_root.join("namespace")).await;
    let started = Instant::now();
    handle.command(Command::Cancel { turn_id }).await.unwrap();
    let view = wait(&handle, idle).await;
    assert_status(results(&view)[0], ToolStatus::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "kill grace plus scheduling allowance"
    );
    assert_namespace_gone(&f.host.workspace_root).await;
    start(&handle).await;
    wait(&handle, idle).await;
    handle.shutdown().await;
}

#[tokio::test]
#[ignore = "S02 real Linux sandbox"]
async fn s02_drop_is_graceful_cancellation_without_uncertainty() {
    let f = Fixture::new(&[]);
    let handle = f
        .create(Arc::new(ScriptedProvider::new([proposal(
            "bash",
            json!({"command":format!("{NAMESPACE}(sleep 30 & wait) & wait")}),
        )])))
        .await;
    start(&handle).await;
    approve(&handle, false).await;
    wait_file(&f.host.workspace_root.join("namespace")).await;
    let mut subscription = handle.subscribe();
    subscription.next().await.unwrap();
    drop(handle);
    bounded(async { while subscription.next().await.is_some() {} }).await;
    assert_namespace_gone(&f.host.workspace_root).await;
    let handle = f.open(Arc::new(ScriptedProvider::new([]))).await;
    let view = wait(&handle, idle).await;
    assert!(view.awaiting.is_empty());
    assert_status(results(&view)[0], ToolStatus::Cancelled);
    assert!(
        !view
            .snapshot
            .events()
            .any(|(_, _, e)| matches!(e, JournalEvent::ExecutionUncertain(_)))
    );
    handle.shutdown().await;
}

fn offered(f: &Fixture) -> Vec<String> {
    let bytes = f
        .root
        .read_artifact(&f.header.configuration, 1024 * 1024)
        .unwrap();
    let policy = PolicySnapshot::parse(f.header.configuration.clone(), &bytes).unwrap();
    let (host, _) = ToolHost::assemble(&f.host, &policy);
    ToolSet::new(host.executors.iter().map(|e| e.backend()))
        .definitions(&policy)
        .into_iter()
        .map(|d| d.name)
        .collect()
}
#[tokio::test]
#[ignore = "S02 real Linux sandbox"]
async fn s02_broken_bwrap_offers_only_files_and_refuses_bash() {
    let mut f = Fixture::new(&[]);
    f.host.sandbox.as_mut().unwrap().bwrap = "/not/a/bwrap".into();
    assert_eq!(offered(&f), ["read", "write", "edit"]);
    let handle = f
        .create(Arc::new(ScriptedProvider::new([
            proposal("bash", json!({"command":"echo must-not-run > marker"})),
            reply(),
        ])))
        .await;
    assert!(
        handle
            .tool_availability()
            .unwrap()
            .file_tools
            .is_available()
    );
    assert!(!handle.tool_availability().unwrap().shell.is_available());
    start(&handle).await;
    let view = wait(&handle, idle).await;
    assert_eq!(
        results(&view)[0].metadata.code.as_deref(),
        Some("tools_unavailable")
    );
    assert!(!f.host.workspace_root.join("marker").exists());
    handle.shutdown().await;
}
#[tokio::test]
#[ignore = "S02 real Linux sandbox"]
async fn s02_file_layer_unavailable_disables_working_shell() {
    let f = Fixture::new(&["alias/token"]);
    // Real startup failure, not an opener seam: openat2 refuses this ancestor.
    std::os::unix::fs::symlink("private", f.host.workspace_root.join("alias")).unwrap();
    assert!(offered(&f).is_empty());
    let handle = f
        .create(Arc::new(ScriptedProvider::new([
            proposal("bash", json!({"command":"echo must-not-run > marker"})),
            reply(),
        ])))
        .await;
    let report = serde_json::to_value(handle.tool_availability().unwrap()).unwrap();
    assert_eq!(report["file_tools"]["status"], "unavailable");
    assert_eq!(report["shell"]["status"], "unavailable");
    assert!(
        report["shell"]["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("planning_view_unavailable"))
    );
    // No probe failure: reasons consist of the planning marker and file reason.
    assert_eq!(
        report["shell"]["reasons"].as_array().unwrap().len(),
        report["file_tools"]["reasons"].as_array().unwrap().len() + 1
    );
    start(&handle).await;
    let view = wait(&handle, idle).await;
    assert_eq!(
        results(&view)[0].metadata.code.as_deref(),
        Some("tools_unavailable")
    );
    assert!(!f.host.workspace_root.join("marker").exists());
    handle.shutdown().await;
}

#[tokio::test]
async fn session_refuses_storage_overlap_before_assembly() {
    let mut f = Fixture::new(&[]);
    f.host.workspace_root = f._temp.path().into();
    f.header.workspace = f.host.workspace_root.to_str().unwrap().into();
    let error = SessionHandle::create_with_tools(
        f.root.clone(),
        f.session.clone(),
        f.header.clone(),
        Arc::new(ScriptedProvider::new([])),
        config(),
        &f.host,
    )
    .await
    .err()
    .unwrap();
    assert!(
        matches!(error, PersistError::Rejected { code } if code == "storage_overlaps_workspace")
    );
}
#[tokio::test]
async fn invalid_policy_assembles_nothing_and_fails_first_turn() {
    let mut f = Fixture::new(&[]);
    f.header.configuration = f.root.put_artifact(b"{}").unwrap();
    // Invalid configuration must skip the executable entirely, not merely
    // suppress registration after probing it.
    use std::os::unix::fs::PermissionsExt;
    let fake = f._temp.path().join("marker-bwrap");
    let marker = f._temp.path().join("probe-ran");
    fs::write(
        &fake,
        format!("#!/bin/sh\nprintf probe > '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    f.host.sandbox.as_mut().unwrap().bwrap = fake;
    let provider = Arc::new(ScriptedProvider::new([reply()]));
    let handle = f.create(provider.clone()).await;
    let report = serde_json::to_value(handle.tool_availability().unwrap()).unwrap();
    assert_eq!(report["shell"]["reasons"], json!(["policy_invalid"]));
    start(&handle).await;
    let view = wait(&handle, idle).await;
    assert!(view.snapshot.events().any(|(_,_,e)| matches!(e, JournalEvent::TurnFinished(f) if f.code.as_deref() == Some("policy_invalid"))));
    assert!(provider.streams().is_empty());
    assert!(!marker.exists(), "invalid policy must assemble nothing");
    handle.shutdown().await;
}
#[test]
fn availability_never_publishes_probe_output_or_environment_values() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new(&[]);
    let fake = f._temp.path().join("bwrap");
    fs::write(&fake, "#!/bin/sh\nprintf 'SECRET_OUTPUT_SENTINEL\\n'\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    f.host.sandbox = Some(SandboxConfig {
        bwrap: fake,
        path: "SECRET_PATH_SENTINEL".into(),
        ..SandboxConfig::default()
    });
    let bytes = policy(&[]);
    let policy = PolicySnapshot::parse(
        ArtifactRef {
            sha256: Digest::of(&bytes),
            bytes: bytes.len() as u64,
        },
        &bytes,
    )
    .unwrap();
    let (host, report) = ToolHost::assemble(&f.host, &policy);
    assert_eq!(host.executors.len(), 1);
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("SECRET_"), "{json}");
    assert!(json.contains("bwrap_version_unrecognized"), "{json}");
}
#[tokio::test]
async fn preinstalled_host_and_mismatched_data_root_are_rejected_before_create() {
    let mut f = Fixture::new(&[]);
    let bytes = policy(&[]);
    let p = PolicySnapshot::parse(
        ArtifactRef {
            sha256: Digest::of(&bytes),
            bytes: bytes.len() as u64,
        },
        &bytes,
    )
    .unwrap();
    let mut cfg = config();
    let mut files_only = f.host.clone();
    files_only.sandbox = None;
    cfg.tools = ToolHost::assemble(&files_only, &p).0;
    let error = SessionHandle::create_with_tools(
        f.root.clone(),
        f.session.clone(),
        f.header.clone(),
        Arc::new(ScriptedProvider::new([])),
        cfg,
        &f.host,
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(error, PersistError::Rejected { code } if code == "tool_host_conflict"));
    f.host.data_root = f._temp.path().join("different-data");
    let error = SessionHandle::create_with_tools(
        f.root.clone(),
        f.session.clone(),
        f.header.clone(),
        Arc::new(ScriptedProvider::new([])),
        config(),
        &f.host,
    )
    .await
    .err()
    .unwrap();
    assert!(
        matches!(error, PersistError::Rejected { code } if code == "tool_host_data_root_mismatch")
    );
}

#[tokio::test]
#[ignore = "S02 real Linux sandbox"]
async fn s02_multimegabyte_capture_uses_append_path_not_upload_limit() {
    let f = Fixture::new(&[]);
    let mut cfg = config();
    cfg.max_artifact_bytes = 1024; // Uploads are limited; executor captures are not uploads.
    let handle = SessionHandle::create_with_tools(
        f.root.clone(),
        f.session.clone(),
        f.header.clone(),
        Arc::new(ScriptedProvider::new([
            proposal(
                "bash",
                json!({"command":"head -c 3145728 /dev/zero", "timeout_ms":30000}),
            ),
            reply(),
        ])),
        cfg,
        &f.host,
    )
    .await
    .unwrap();
    start(&handle).await;
    approve(&handle, false).await;
    let view = wait(&handle, idle).await;
    let result = results(&view)[0];
    assert_status(result, ToolStatus::Succeeded);
    assert!(result.metadata.output_truncated);
    let capture = &result.metadata.artifacts[0];
    let bytes = f.root.read_artifact(capture, 64 * 1024 * 1024).unwrap();
    assert_eq!(bytes.len(), 3 * 1024 * 1024);
    assert!(bytes.iter().all(|b| *b == 0));
    assert_eq!(capture.sha256, Digest::of(&bytes));
    handle.shutdown().await;
}

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn tool_host_child() {
    let Ok(kind) = std::env::var("HARNESS_TOOL_HOST_CHILD") else {
        return;
    };
    let base = PathBuf::from(std::env::var_os("HARNESS_TOOL_HOST_ROOT").unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let root = Arc::new(DataRoot::open(base.join("data"), false).unwrap());
        let session: SessionId = fs::read_to_string(base.join("session")).unwrap().parse().unwrap();
        let host = ToolHostConfig { workspace_root: base.join("workspace"), data_root: root.path().into(), sandbox: Some(SandboxConfig::default()) };
        if kind == "descriptor" {
            use std::os::fd::AsRawFd;
            use rustix::io::{FdFlags, fcntl_setfd};
            let fd = fs::File::open("/dev/null").unwrap();
            fcntl_setfd(&fd, FdFlags::empty()).unwrap();
            assert!(fd.as_raw_fd() > 2);
            let handle = SessionHandle::open_with_tools(root, session, Arc::new(ScriptedProvider::new([])), config(), &host).await.unwrap();
            let report = serde_json::to_value(handle.tool_availability().unwrap()).unwrap();
            assert!(report["shell"]["reasons"].as_array().unwrap().iter().any(|r| r.as_str().unwrap() == "descriptor_audit_failed"), "{report}");
            assert_eq!(report["file_tools"]["status"], "available");
            handle.shutdown().await;
        } else {
            assert_eq!(kind, "crash");
            let handle = SessionHandle::open_with_tools(root, session, Arc::new(ScriptedProvider::new([
                proposal("bash", json!({"command":format!("{NAMESPACE}echo once >> counter; (sleep 30 & wait) & wait")})),
            ])), config(), &host).await.unwrap();
            start(&handle).await;
            approve(&handle, false).await;
            std::future::pending::<()>().await; // Parent SIGKILL: no graceful shutdown.
        }
    });
}
fn child(f: &Fixture, kind: &str) -> KillOnDrop {
    fs::write(f._temp.path().join("session"), f.session.to_string()).unwrap();
    KillOnDrop(
        Process::new(std::env::current_exe().unwrap())
            .args(["--exact", "tool_host_child", "--nocapture"])
            .env("HARNESS_TOOL_HOST_CHILD", kind)
            .env("HARNESS_TOOL_HOST_ROOT", f._temp.path())
            .spawn()
            .unwrap(),
    )
}
#[tokio::test]
#[ignore = "S02 real Linux sandbox"]
async fn s02_inheritable_descriptor_disables_shell_at_startup() {
    let f = Fixture::new(&[]);
    f.create(Arc::new(ScriptedProvider::new([])))
        .await
        .shutdown()
        .await;
    assert!(child(&f, "descriptor").0.wait().unwrap().success());
}
#[tokio::test]
#[ignore = "S02 real Linux sandbox"]
async fn s02_process_crash_kills_sandbox_and_never_replays_intent() {
    let f = Fixture::new(&[]);
    f.create(Arc::new(ScriptedProvider::new([])))
        .await
        .shutdown()
        .await;
    let mut child = child(&f, "crash");
    wait_file(&f.host.workspace_root.join("counter")).await;
    assert!(!namespace_processes(&f.host.workspace_root).is_empty());
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    // SIGKILL runs no Rust cancellation/destructors. The namespace is torn down
    // by Bubblewrap's --die-with-parent, not by the executor's explicit kill.
    assert_namespace_gone(&f.host.workspace_root).await;
    let provider = Arc::new(ScriptedProvider::new([reply()]));
    let handle = f.open(provider.clone()).await;
    let view = wait(&handle, |v| v.phase == Phase::AwaitingResolution).await;
    assert_eq!(view.awaiting.len(), 1);
    assert_eq!(view.awaiting[0].tool, "bash");
    assert!(provider.streams().is_empty());
    assert_eq!(
        fs::read_to_string(f.host.workspace_root.join("counter")).unwrap(),
        "once\n"
    );
    handle
        .command(Command::ResolveUncertain {
            invocation_id: view.awaiting[0].invocation_id.clone(),
            resolution: UserResolution::AcknowledgedUnknown,
        })
        .await
        .unwrap();
    wait(&handle, idle).await;
    start(&handle).await;
    wait(&handle, idle).await;
    assert_eq!(
        fs::read_to_string(f.host.workspace_root.join("counter")).unwrap(),
        "once\n"
    );
    handle.shutdown().await;
}
