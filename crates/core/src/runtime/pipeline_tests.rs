//! Storage-backed boundary tests for approval clocks, policy invalidation and
//! defensive decoding of result codes.
use super::*;
use crate::{
    context::ModelLimits,
    session::{DataRoot, JournalStore, SessionHeader, SessionId},
    tools::{ToolSet, WorkspaceIdentity, tests::FakeWorkspace},
};
use std::sync::Arc;

const CONFIGURATION: &[u8] = br#"{"policy_revision":"initial","enabled_tools":["write"],"workspace_read":"allow","workspace_mutation":"ask","process":"ask","network":"deny"}"#;

struct AdvancingEnv {
    inner: crate::runtime::DeterministicEnv,
    tick: std::time::Duration,
    samples: usize,
}

macro_rules! identities {
    ($($method:ident -> $result:ty),+ $(,)?) => {
        $(fn $method(&mut self) -> $result { self.inner.$method() })+
    };
}

impl Env for AdvancingEnv {
    fn now(&mut self) -> crate::session::Timestamp {
        self.samples += 1;
        let now = self.inner.now();
        self.inner.advance(self.tick);
        now
    }
    identities! {
        runtime_epoch -> RuntimeEpoch,
        entry_id -> EntryId,
        turn_id -> TurnId,
        response_id -> crate::session::ResponseId,
        call_id -> crate::session::CallId,
        approval_id -> ApprovalId,
        decision_id -> crate::session::DecisionId,
        invocation_id -> InvocationId,
        policy_revision -> String,
    }
}

struct Rig {
    _temp: tempfile::TempDir,
    runtime: Runtime,
    env: AdvancingEnv,
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

fn pending_approval() -> Rig {
    let temp = tempfile::tempdir().unwrap();
    let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
    let bytes = CONFIGURATION;
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
    let mut env = AdvancingEnv {
        inner: crate::runtime::DeterministicEnv::new(1),
        tick: std::time::Duration::from_millis(1),
        samples: 0,
    };
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
        _temp: temp,
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
    assert!(rig.runtime.view().pending_approval.is_some());
    rig
}

#[test]
fn policy_revision_invalidates_pending_approval_with_durable_denial_and_no_execution() {
    let mut rig = pending_approval();
    let ticket = rig.runtime.view().pending_approval.unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(CONFIGURATION).unwrap();
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

fn advance_to(rig: &mut Rig, at: time::OffsetDateTime) {
    let remaining = at - rig.env.inner.now().instant();
    assert!(!remaining.is_negative());
    rig.env.inner.advance(remaining.unsigned_abs());
}

#[test]
fn approval_one_millisecond_before_expiry_uses_decision_instant_for_commit() {
    for remember in [false, true] {
        let mut rig = pending_approval();
        let ticket = rig.runtime.view().pending_approval.unwrap();
        let decision_at = ticket.expires_at.instant() - time::Duration::milliseconds(1);
        advance_to(&mut rig, decision_at);
        let samples = rig.env.samples;
        rig.feed(Input::Command {
            request: RequestId(2),
            command: Command::SubmitApproval {
                approval_id: ticket.approval_id,
                plan_digest: ticket.plan_digest,
                decision: Decision::Allow,
                remember,
            },
        });
        assert_eq!(rig.env.samples - samples, 1, "one clock read for the step");
        assert_eq!(rig.executions, 1, "approval at expiry - 1 ms must execute");
        assert_eq!(rig.runtime.phase(), Phase::Durable(Executing));
        assert!(rig.runtime.view().pending_approval.is_none());
        let at =
            rig.emitted
                .iter()
                .find_map(|effect| {
                    let Effect::Persist { commit, at, .. } = effect else {
                        return None;
                    };
                    commit.events.iter().any(|event| matches!(event,
                JournalEvent::AuthorizationDecided(d) if d.decision == Decision::Allow
            )).then_some(at)
                })
                .unwrap();
        assert_eq!(at.instant(), decision_at);
        assert!(at.instant() < ticket.expires_at.instant());
        assert!(rig.emitted.iter().any(|effect| matches!(
            effect,
            Effect::Reply {
                request: RequestId(2),
                result: Ok(Accepted::ApprovalSubmitted { .. })
            }
        )));
    }
}

#[test]
fn approval_at_exact_expiry_is_durably_expired_without_execution() {
    let mut rig = pending_approval();
    let ticket = rig.runtime.view().pending_approval.unwrap();
    advance_to(&mut rig, ticket.expires_at.instant());
    rig.feed(Input::Command {
        request: RequestId(2),
        command: Command::SubmitApproval {
            approval_id: ticket.approval_id,
            plan_digest: ticket.plan_digest,
            decision: Decision::Allow,
            remember: true,
        },
    });
    assert_eq!(rig.executions, 0);
    assert_ne!(rig.runtime.phase(), Phase::Faulted);
    assert!(rig.runtime.view().pending_approval.is_none());
    assert!(rig.runtime.snapshot().entries().any(|(_, entry)| matches!(
        &entry.entry.payload,
        EntryPayload::ToolResult(r) if r.result.status == ToolStatus::Denied
            && r.result.metadata.code.as_deref() == Some("approval_expired")
    )));
    assert!(rig.emitted.iter().any(|effect| matches!(effect,
        Effect::Persist { commit, at, .. } if at == &ticket.expires_at
            && commit.events.iter().any(|event| matches!(event,
                JournalEvent::AuthorizationDecided(d) if d.reason == "approval_expired"
                    && d.decision == Decision::Deny
            ))
    )));
    assert!(
        rig.runtime
            .snapshot()
            .events()
            .all(|(_, _, event)| !matches!(
                event,
                JournalEvent::ExecutionIntent(_) | JournalEvent::ConfigurationChanged(_)
            ))
    );
}

#[test]
fn approval_request_deadline_and_timer_share_their_steps_clock_sample() {
    let rig = pending_approval();
    let ticket = rig.runtime.view().pending_approval.unwrap();
    let requested_at = rig
        .emitted
        .iter()
        .find_map(|effect| match effect {
            Effect::Persist { commit, at, .. }
                if commit
                    .events
                    .iter()
                    .any(|event| matches!(event, JournalEvent::ApprovalRequested(_))) =>
            {
                Some(at)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        ticket.expires_at.instant() - requested_at.instant(),
        time::Duration::minutes(5)
    );
    let duration = rig
        .emitted
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::SetTimer { duration, .. } => Some(*duration),
            _ => None,
        })
        .unwrap();
    assert_eq!(duration, std::time::Duration::from_millis(299_999));
}

#[test]
fn unknown_stored_result_code_faults_without_persisting_or_panicking() {
    let mut rig = pending_approval();
    let snapshot = rig.runtime.snapshot().clone();
    let turn = snapshot.active_turn().unwrap().clone();
    let ticket = rig.runtime.view().pending_approval.unwrap();
    let mut result = runtime_result(
        &turn,
        &ticket.call,
        ToolStatus::Denied,
        ResultCode::ApprovalDenied,
        "denied",
    );
    let EntryPayload::ToolResult(entry) = &mut result.1 else {
        unreachable!()
    };
    entry.result.metadata.code = Some("future_result_code".into());
    rig.runtime.commit_results(
        &snapshot,
        &turn,
        vec![result],
        Vec::new(),
        idle_flight(),
        &mut rig.env,
    );
    assert_eq!(rig.runtime.phase(), Phase::Faulted);
    assert_eq!(rig.executions, 0);
    assert!(rig.runtime.out.iter().any(|effect| matches!(
        effect,
        Effect::Emit(RuntimeEvent {
            kind: EventKind::Fault(FaultCause::Internal("invalid_stored_result_code")),
            ..
        })
    )));
    assert!(
        rig.runtime
            .out
            .iter()
            .all(|effect| !matches!(effect, Effect::Persist { .. }))
    );
}
