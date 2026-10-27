//! Injection of an out-of-band policy revision while an approval is pending.
//! The sequential public runtime currently changes policy only via remember;
//! this defensive gate deliberately substitutes the decoded policy, not storage.
use super::*;
use crate::{
    context::ModelLimits,
    session::{DataRoot, JournalStore, SessionHeader, SessionId},
    tools::{ToolSet, WorkspaceIdentity, tests::FakeWorkspace},
};
use std::sync::Arc;

struct Rig {
    runtime: Runtime,
    env: super::super::super::DeterministicEnv,
    store: JournalStore,
    root: Arc<DataRoot>,
    workspace: FakeWorkspace,
    emitted: Vec<Effect>,
    executions: usize,
}
impl Rig {
    fn feed(&mut self, input: Input) {
        let mut pending = std::collections::VecDeque::from([input]);
        while let Some(input) = pending.pop_front() {
            for effect in self.runtime.step(input, &mut self.env) {
                self.emitted.push(effect.clone());
                match effect {
                    Effect::Persist {
                        token,
                        mut commit,
                        command,
                        artifacts,
                        at,
                    } => {
                        for bytes in artifacts {
                            self.root.put_artifact(&bytes).unwrap();
                        }
                        if let Some((id, value)) = command {
                            self.store.bind_command(&mut commit, id, &value).unwrap();
                        }
                        let ack = self.store.append(commit, at).unwrap();
                        pending.push_back(Input::Persisted {
                            token,
                            ack,
                            snapshot: self.store.snapshot(),
                        });
                    }
                    Effect::PlanCall {
                        token,
                        call,
                        tool,
                        operation,
                    } => {
                        let policy = self.runtime.policy().unwrap();
                        let tool = self
                            .runtime
                            .tools()
                            .registry()
                            .resolve(&tool, policy)
                            .unwrap();
                        pending.push_back(Input::Planned {
                            token,
                            result: tool.plan(&operation, &call, &self.workspace, policy),
                        });
                    }
                    Effect::Execute { .. } => self.executions += 1,
                    _ => {}
                }
            }
        }
    }
}

#[test]
fn policy_revision_invalidates_pending_approval_with_durable_denial_and_no_execution() {
    let temp = tempfile::tempdir().unwrap();
    let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
    let bytes = br#"{"policy_revision":"initial","enabled_tools":["write"],"workspace_read":"allow","workspace_mutation":"ask","process":"ask","network":"deny"}"#;
    let reference = root.put_artifact(bytes).unwrap();
    let configuration = SessionConfiguration::parse(reference.clone(), bytes).unwrap();
    let model = ModelRef {
        provider_instance: "mock".into(),
        model: "scripted".into(),
    };
    let header = SessionHeader {
        workspace: "/workspace".into(),
        initial_model: model.clone(),
        policy_revision: "initial".into(),
        configuration: reference,
    };
    let mut env = crate::runtime::DeterministicEnv::new(1);
    let store = JournalStore::create(root.clone(), SessionId::new(), header, env.now()).unwrap();
    let mut config = RuntimeConfig::default();
    config.context.catalog.insert(
        model,
        ModelLimits {
            context_tokens: Some(1_000_000),
            max_output_tokens: None,
        },
    );
    let runtime = Runtime::with_tools(
        store.snapshot(),
        configuration,
        config,
        ToolSet::new([crate::session::ExecutionBackend::TrustedFs]),
        &mut env,
    );
    let mut workspace = FakeWorkspace::new();
    workspace.identity = WorkspaceIdentity("/workspace".into());
    let mut rig = Rig {
        runtime,
        env,
        store,
        root,
        workspace,
        emitted: Vec::new(),
        executions: 0,
    };
    rig.feed(Input::Command {
        request: RequestId(1),
        command: Command::StartTurn {
            command_id: CommandId::new(),
            content: vec![ContentBlock::Text {
                text: "write".into(),
            }],
        },
    });
    let inference = InferenceId(1);
    for event in [
        ProviderEvent::Started,
        ProviderEvent::ToolCallStarted {
            block_index: 0,
            name: "write".into(),
            provider_call_id: None,
        },
        ProviderEvent::ToolArgumentsDelta {
            block_index: 0,
            fragment: r#"{"path":"new","text":"x"}"#.into(),
        },
        ProviderEvent::ToolCallReady { block_index: 0 },
        ProviderEvent::Finished {
            outcome: ProviderOutcome::Completed,
            finish_reason: Some(crate::session::FinishReason::ToolCalls),
            error: None,
        },
    ] {
        rig.feed(Input::Provider { inference, event });
    }
    let ticket = rig.runtime.view().pending_approval.unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    value["policy_revision"] = json!("new-revision");
    let changed = serde_json::to_vec(&value).unwrap();
    let reference = rig.root.put_artifact(&changed).unwrap();
    rig.runtime.policy = Ok(PolicySnapshot::parse(reference, &changed).unwrap());
    rig.feed(Input::Command {
        request: RequestId(2),
        command: Command::SubmitApproval {
            approval_id: ticket.approval_id.clone(),
            plan_digest: ticket.plan_digest.clone(),
            decision: Decision::Allow,
            remember: true,
        },
    });
    assert_eq!(rig.executions, 0);
    assert!(rig.runtime.view().pending_approval.is_none());
    assert!(rig.runtime.snapshot().entries().any(|(_, e)| matches!(&e.entry.payload, EntryPayload::ToolResult(r) if r.result.status == ToolStatus::Denied && r.result.metadata.code.as_deref() == Some("approval_invalidated"))));
    assert!(rig.runtime.snapshot().events().any(|(_, _, e)| matches!(e, JournalEvent::AuthorizationDecided(d) if d.reason == "approval_invalidated" && d.decision == Decision::Deny)));
    assert!(rig.emitted.iter().any(|e| matches!(
        e,
        Effect::Emit(RuntimeEvent {
            kind: EventKind::ApprovalResolved {
                outcome: ApprovalOutcome::Invalidated,
                ..
            },
            ..
        })
    )));
    assert!(
        rig.runtime
            .snapshot()
            .events()
            .all(|(_, _, e)| !matches!(e, JournalEvent::ExecutionIntent(_)))
    );
}
