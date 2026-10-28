//! Driver-only seams prove stale protection faults and actual StartInference offers.
use super::*;
use crate::tools::SandboxConfig;
use serde_json::json;

fn policy(paths: &[&str], revision: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"policy_revision":revision,"enabled_tools":["read","write","edit","bash"],
        "workspace_read":"allow","workspace_mutation":"ask","process":"ask","network":"deny","protected_paths":paths})).unwrap()
}
struct Real {
    _temp: tempfile::TempDir,
    root: Arc<DataRoot>,
    workspace: std::path::PathBuf,
    handle: SessionHandle,
}
impl Real {
    async fn new(paths: &[&str], scripts: Vec<Script>, unavailable_files: bool) -> Self {
        Self::with_policy(paths, scripts, unavailable_files, false).await
    }
    async fn with_policy(
        paths: &[&str],
        scripts: Vec<Script>,
        unavailable_files: bool,
        invalid: bool,
    ) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        if unavailable_files {
            std::os::unix::fs::symlink(".", workspace.join("alias")).unwrap();
        }
        let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
        let header = SessionHeader {
            workspace: workspace.to_str().unwrap().into(),
            initial_model: ModelRef {
                provider_instance: "mock".into(),
                model: "test".into(),
            },
            policy_revision: "p1".into(),
            configuration: root
                .put_artifact(
                    if invalid {
                        b"{}".to_vec()
                    } else {
                        policy(paths, "p1")
                    }
                    .as_slice(),
                )
                .unwrap(),
        };
        let host = ToolHostConfig {
            workspace_root: workspace.clone(),
            data_root: root.path().into(),
            sandbox: Some(SandboxConfig {
                bwrap: "/missing/bwrap".into(),
                ..SandboxConfig::default()
            }),
        };
        let handle = SessionHandle::create_with_tools(
            root.clone(),
            SessionId::new(),
            header,
            Arc::new(ScriptedProvider::new(scripts)),
            config(),
            &host,
        )
        .await
        .unwrap();
        Self {
            _temp: temp,
            root,
            workspace,
            handle,
        }
    }
    async fn wait(&self, predicate: impl Fn(&RuntimeView) -> bool) -> RuntimeView {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let view = self.handle.view().await.unwrap();
                if predicate(&view) {
                    return view;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
}
#[tokio::test]
async fn changed_configuration_protection_faults_tool_host_stale_before_execution() {
    let proposal = Script::new([
        Step::Event(ProviderEvent::Started),
        Step::Event(ProviderEvent::ToolCallStarted {
            block_index: 0,
            name: "write".into(),
            provider_call_id: None,
        }),
        Step::Event(ProviderEvent::ToolArgumentsDelta {
            block_index: 0,
            fragment: json!({"path":"new","text":"hello"}).to_string(),
        }),
        Step::Event(ProviderEvent::ToolCallReady { block_index: 0 }),
        Step::completed(FinishReason::ToolCalls),
    ]);
    let f = Real::new(&[], vec![proposal], false).await;
    f.handle.command(Fixture::command(1)).await.unwrap();
    f.wait(|v| v.pending_approval.is_some()).await;
    let bytes = policy(&["newly-protected"], "p2");
    let reference = f.root.put_artifact(&bytes).unwrap();
    let configuration = SessionConfiguration::parse(reference, &bytes).unwrap();
    let mut events = f.handle.subscribe();
    events.next().await.unwrap();
    let (reply, received) = oneshot::channel();
    f.handle
        .commands
        .send(Message::InstallConfiguration(configuration.clone(), reply))
        .await
        .unwrap();
    received.await.unwrap();
    assert_eq!(f.handle.view().await.unwrap().phase, Phase::Faulted);
    let event = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(SubscriptionItem::Event(event)) = events.next().await
                && let EventKind::Fault(cause) = event.kind
            {
                break cause;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(event, FaultCause::Internal("tool_host_stale"));
    assert!(!f.workspace.join("new").exists());
    // Reopening restores the original durable policy, but must retain the
    // assembly binding for subsequent installations in the replacement epoch.
    f.handle.reopen().await.unwrap();
    f.wait(idle).await;
    let (reply, received) = oneshot::channel();
    f.handle
        .commands
        .send(Message::InstallConfiguration(configuration, reply))
        .await
        .unwrap();
    received.await.unwrap();
    assert_eq!(f.handle.view().await.unwrap().phase, Phase::Faulted);
    f.handle.shutdown().await;
}
#[tokio::test]
async fn fault_reopen_preserves_invalid_policy_enforcement_without_backends() {
    let f = Real::with_policy(&[], vec![reply("must not infer")], false, true).await;
    let (reply, received) = oneshot::channel();
    f.handle
        .commands
        .send(Message::Inject(
            Input::DriverInvariant { code: "test_fault" },
            reply,
        ))
        .await
        .unwrap();
    received.await.unwrap();
    f.handle.reopen().await.unwrap();
    f.wait(idle).await;
    f.handle.command(Fixture::command(1)).await.unwrap();
    let view = f.wait(idle).await;
    assert!(view.snapshot.events().any(|(_,_,e)| matches!(e,JournalEvent::TurnFinished(f) if f.code.as_deref() == Some("policy_invalid"))));
    let (reply, received) = oneshot::channel();
    f.handle
        .commands
        .send(Message::Offered(reply))
        .await
        .unwrap();
    assert!(received.await.unwrap().is_empty());
    f.handle.shutdown().await;
}
#[tokio::test]
async fn start_inference_filters_real_host_definitions_on_backend_failure() {
    for unavailable_files in [false, true] {
        let paths = if unavailable_files {
            vec!["alias/secret"]
        } else {
            vec![]
        };
        let f = Real::new(&paths, vec![reply("done")], unavailable_files).await;
        f.handle.command(Fixture::command(1)).await.unwrap();
        f.wait(idle).await;
        let (reply, received) = oneshot::channel();
        f.handle
            .commands
            .send(Message::Offered(reply))
            .await
            .unwrap();
        let definitions = received.await.unwrap();
        assert_eq!(definitions.len(), 1);
        let expected = if unavailable_files {
            vec![]
        } else {
            vec!["read", "write", "edit"]
        };
        assert_eq!(definitions[0], expected);
        f.handle.shutdown().await;
    }
}
