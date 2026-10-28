//! M3 T01/T02, R01/R02 and write-ahead gates, using only fake workspace facts
//! and counted synchronous executors against real storage.
use super::*;
use crate::tools::{PolicySnapshot, Protection, WorkspaceIdentity};
use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

pub(super) fn configuration() -> Value {
    json!({"policy_revision":"initial", "enabled_tools":["read","write","edit","bash"], "workspace_read":"allow", "workspace_mutation":"ask", "process":"ask", "network":"deny", "protected_paths":[], "allowances":[]})
}
pub(super) fn fake_workspace() -> FakeWorkspace {
    let mut workspace = FakeWorkspace::new();
    workspace.identity = WorkspaceIdentity("/workspace".into());
    for n in 0..4 {
        workspace.file(&format!("f{n}"), b"hello world");
    }
    workspace
}
pub(super) fn enabled() -> Driver {
    configured(configuration())
}
pub(super) fn configured(value: Value) -> Driver {
    Driver::with_tools(
        known(),
        &serde_json::to_vec(&value).unwrap(),
        ToolSet::new([ExecutionBackend::TrustedFs, ExecutionBackend::Bubblewrap]),
    )
}

#[derive(Default)]
pub(crate) struct StubExecutor {
    pub(crate) invocations: Mutex<Vec<InvocationId>>,
    pub(super) outcomes: Mutex<std::collections::VecDeque<ExecutionOutcome>>,
    pub(super) success_wins: AtomicBool,
}
struct StubBackend<'a> {
    backend: ExecutionBackend,
    state: &'a StubExecutor,
}
impl StubExecutor {
    pub(crate) fn execute(
        &self,
        operation: &AuthorizedOperation,
        cancel: &CancelToken,
    ) -> ExecutionOutcome {
        let executor = StubBackend {
            backend: operation.plan().backend(),
            state: self,
        };
        assert_eq!(executor.backend(), operation.plan().backend());
        executor.execute(operation, cancel)
    }
}
impl Executor for StubBackend<'_> {
    fn backend(&self) -> ExecutionBackend {
        self.backend
    }
    fn execute(&self, operation: &AuthorizedOperation, cancel: &CancelToken) -> ExecutionOutcome {
        let mut invocations = self.state.invocations.lock().unwrap();
        assert!(
            !invocations.contains(operation.invocation_id()),
            "invocation executed twice"
        );
        invocations.push(operation.invocation_id().clone());
        if cancel.is_cancelled() && !self.state.success_wins.load(Ordering::Acquire) {
            return outcome(ToolStatus::Cancelled, Some(ResultCode::Cancelled));
        }
        self.state
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| outcome(ToolStatus::Succeeded, None))
    }
}
pub(crate) fn outcome(status: ToolStatus, code: Option<ResultCode>) -> ExecutionOutcome {
    ExecutionOutcome {
        status,
        code,
        content: text("stub output"),
        exit_code: Some(if status == ToolStatus::Succeeded {
            0
        } else {
            1
        }),
        duration_ms: Some(17),
        output_truncated: false,
        artifacts: vec![b"full stub capture".to_vec()],
    }
}
impl Driver {
    pub(super) fn plan(&mut self) -> bool {
        let Some((token, call, name, operation)) = self.planning.take() else {
            return false;
        };
        let policy = self.runtime.policy().unwrap();
        let tool = self
            .runtime
            .tools()
            .registry()
            .resolve(&name, policy)
            .unwrap();
        let result = tool.plan(&operation, &call, &self.workspace, policy);
        self.step(Input::Planned { token, result });
        true
    }
    pub(super) fn execute(&mut self) -> bool {
        let Some((token, operation, cancel)) = self.executing.take() else {
            return false;
        };
        let outcome = self.executor.execute(&operation, &cancel);
        self.step(Input::ToolFinished { token, outcome });
        true
    }
    pub(super) fn count(&self) -> usize {
        self.executor.invocations.lock().unwrap().len()
    }
    pub(super) fn approval(&mut self, decision: Decision, remember: bool) -> RequestId {
        let pending = self.runtime.view().pending_approval.unwrap();
        self.command(Command::SubmitApproval {
            approval_id: pending.approval_id,
            plan_digest: pending.plan_digest,
            decision,
            remember,
        })
    }
    pub(super) fn run_tools(&mut self) {
        for _ in 0..100 {
            if self.complete() || self.plan() || self.execute() {
                continue;
            }
            return;
        }
        panic!("tool loop stalled");
    }
}

pub(super) fn propose(d: &mut Driver, calls: &[(&str, Value)]) {
    d.start("use the tools");
    d.settle();
    d.provider(ProviderEvent::Started);
    for (i, (name, args)) in calls.iter().enumerate() {
        let block_index = i as u32;
        d.provider(ProviderEvent::ToolCallStarted {
            block_index,
            name: (*name).into(),
            provider_call_id: Some(format!("native-{i}")),
        });
        d.provider(ProviderEvent::ToolArgumentsDelta {
            block_index,
            fragment: args.to_string(),
        });
        d.provider(ProviderEvent::ToolCallReady { block_index });
    }
    d.provider(ProviderEvent::Finished {
        outcome: ProviderOutcome::Completed,
        finish_reason: Some(FinishReason::ToolCalls),
        error: None,
    });
}
pub(super) fn stop(d: &mut Driver) {
    assert_eq!(d.runtime.phase(), Phase::Durable(RuntimeState::Inferring));
    d.provider(ProviderEvent::Started);
    d.provider(ProviderEvent::Finished {
        outcome: ProviderOutcome::Completed,
        finish_reason: Some(FinishReason::Stop),
        error: None,
    });
    d.settle();
}
pub(super) fn results(d: &Driver) -> Vec<ToolResult> {
    d.runtime
        .snapshot()
        .ancestry(d.runtime.snapshot().head())
        .unwrap()
        .iter()
        .filter_map(|stored| match &stored.entry.payload {
            EntryPayload::ToolResult(r) => Some(r.result.clone()),
            _ => None,
        })
        .collect()
}
pub(super) fn checked(d: Driver) {
    let finished = d.finish();
    check_turns(&finished);
    check_resync(&finished);
    check_replay(&finished);
}

#[test]
fn t01_resolve_validate_plan_and_policy_refusals_never_execute() {
    let cases = [
        ("unknown", json!({}), ResultCode::UnknownTool),
        (
            "read",
            json!({"path":"a","extra":true}),
            ResultCode::InvalidArguments,
        ),
        ("read", json!({"path":"absent"}), ResultCode::NotFound),
        (
            "write",
            json!({"path":"a","text":"x"}),
            ResultCode::AlreadyExists,
        ),
        (
            "edit",
            json!({"path":"a","old_text":"absent","new_text":"x"}),
            ResultCode::NoMatch,
        ),
        ("read", json!({"path":"secret"}), ResultCode::ProtectedPath),
    ];
    for (tool, args, code) in cases {
        let mut d = enabled();
        d.workspace.file("secret", b"private");
        d.workspace.facts_mut("secret").protection = Protection::Denied;
        propose(&mut d, &[(tool, args)]);
        d.run_tools();
        assert_eq!(d.count(), 0);
        let r = results(&d);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].metadata.code.as_deref(), Some(code.as_str()));
        assert_ne!(r[0].status, ToolStatus::Succeeded);
        assert!(r[0].invocation_id.is_none());
        stop(&mut d);
        checked(d);
    }
    for (key, value, code) in [
        ("enabled_tools", json!([]), ResultCode::ToolDisabled),
        ("workspace_read", json!("deny"), ResultCode::PolicyDenied),
    ] {
        let mut config = configuration();
        config[key] = value;
        let mut d = configured(config);
        propose(&mut d, &[("read", json!({"path":"a"}))]);
        d.run_tools();
        assert_eq!(d.count(), 0);
        assert_eq!(results(&d)[0].metadata.code.as_deref(), Some(code.as_str()));
        stop(&mut d);
        checked(d);
    }
}

#[test]
fn t02_results_and_execution_stay_in_block_order_after_invalid_call() {
    scenario_from(enabled, |d| {
        propose(
            d,
            &[
                ("read", json!({"path":"a"})),
                ("write", json!({"path":"x"})),
                ("read", json!({"path":"f2"})),
            ],
        );
        d.run_tools();
        assert_eq!(d.count(), 2);
        let r = results(d);
        assert_eq!(
            r.iter().map(|r| r.status).collect::<Vec<_>>(),
            vec![
                ToolStatus::Succeeded,
                ToolStatus::InvalidArguments,
                ToolStatus::Succeeded
            ]
        );
        let ids: BTreeSet<_> = r.iter().map(|r| r.call.clone().call_id).collect();
        assert_eq!(ids.len(), 3);
        stop(d);
    });
}

#[test]
fn write_ahead_exact_plan_and_executor_failed_result_are_durable() {
    scenario_from(enabled, |d| {
        d.executor
            .outcomes
            .lock()
            .unwrap()
            .push_back(outcome(ToolStatus::Failed, Some(ResultCode::StalePlan)));
        propose(d, &[("read", json!({"path":"a"}))]);
        assert!(d.complete()); // proposal
        assert!(d.complete()); // Authorizing, emits PlanCall
        assert!(d.plan()); // emits intent commit, not Execute
        assert!(d.executing.is_none());
        assert_eq!(d.count(), 0);
        let commit = &d.pending.as_ref().unwrap().commit;
        assert!(
            commit
                .events
                .iter()
                .any(|e| matches!(e, JournalEvent::ExecutionIntent(_)))
        );
        assert!(d.complete()); // intent ack, now Execute
        assert!(d.executing.is_some());
        assert_eq!(d.count(), 0);
        assert!(d.execute()); // result waiting for sync
        assert!(results(d).is_empty());
        assert!(
            !d.segment
                .events()
                .any(|e| matches!(e.kind, EventKind::ToolFinished { .. }))
        );
        d.settle();
        let r = results(d);
        assert_eq!(r[0].status, ToolStatus::Failed);
        assert_eq!(r[0].metadata.provenance, Provenance::Executor);
        assert_eq!(r[0].metadata.code.as_deref(), Some("stale_plan"));
        assert_eq!(
            d.root
                .read_artifact(&r[0].metadata.artifacts[0], 1024)
                .unwrap(),
            b"full stub capture"
        );
        stop(d);
    });
}

#[test]
fn approvals_mismatch_is_commit_free_then_approve_or_deny_or_expire() {
    for kind in 0..3 {
        let mut d = enabled();
        propose(&mut d, &[("write", json!({"path":"new","text":"x"}))]);
        d.run_tools();
        let ticket = d.runtime.view().pending_approval.unwrap();
        assert_eq!(ticket.plan_digest, Digest::of(&ticket.plan));
        assert_eq!(
            ticket.expires_at.instant() - d.env.now().instant(),
            time::Duration::minutes(5)
        );
        let seq = d.runtime.snapshot().sequence();
        let mismatch = d.command(Command::SubmitApproval {
            approval_id: ticket.approval_id.clone(),
            plan_digest: Digest::of(b"wrong"),
            decision: Decision::Allow,
            remember: false,
        });
        assert_eq!(
            d.rejected(mismatch),
            Rejection::Invalid("approval_mismatch")
        );
        assert_eq!(d.runtime.snapshot().sequence(), seq);
        assert!(d.pending.is_none());
        if kind == 2 {
            let timer = *d.timers.iter().next().unwrap();
            d.fire(timer);
        } else {
            let request = d.approval(
                if kind == 0 {
                    Decision::Allow
                } else {
                    Decision::Deny
                },
                false,
            );
            d.run_tools();
            d.accepted(request);
        }
        d.run_tools();
        assert!(d.runtime.view().pending_approval.is_none());
        assert_eq!(d.count(), usize::from(kind == 0));
        let r = results(&d);
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0].metadata.code.as_deref(),
            [None, Some("approval_denied"), Some("approval_expired")][kind]
        );
        let stale = d.command(Command::SubmitApproval {
            approval_id: ticket.approval_id,
            plan_digest: ticket.plan_digest,
            decision: Decision::Allow,
            remember: false,
        });
        assert_eq!(d.rejected(stale), Rejection::NotActive);
        stop(&mut d);
        checked(d);
    }
}

#[test]
fn remember_reloads_policy_and_second_identical_call_never_asks() {
    for tool in ["write", "edit", "bash", "read"] {
        let mut config = configuration();
        config["workspace_read"] = json!("ask");
        let mut d = configured(config);
        let args = match tool {
            "write" => json!({"path":"new","text":"x"}),
            "edit" => json!({"path":"a","old_text":"hello","new_text":"hi"}),
            "bash" => json!({"command":"true"}),
            _ => json!({"path":"a"}),
        };
        propose(&mut d, &[(tool, args.clone()), (tool, args)]);
        d.run_tools();
        let request = d.approval(Decision::Allow, true);
        d.run_tools();
        d.accepted(request);
        if tool == "read" {
            assert!(d.runtime.view().pending_approval.is_some());
            let request = d.approval(Decision::Allow, false);
            d.run_tools();
            d.accepted(request);
            assert_eq!(d.runtime.snapshot().policy_revision(), "initial");
        } else {
            assert!(d.runtime.view().pending_approval.is_none());
            assert_ne!(d.runtime.snapshot().policy_revision(), "initial");
            let bytes = d
                .root
                .read_artifact(d.runtime.snapshot().configuration(), 1024 * 1024)
                .unwrap();
            let policy =
                PolicySnapshot::parse(d.runtime.snapshot().configuration().clone(), &bytes)
                    .unwrap();
            assert_eq!(policy.allowances().len(), 1);
            assert_eq!(policy.revision(), d.runtime.snapshot().policy_revision());
            assert_eq!(d.runtime.policy().unwrap().revision(), policy.revision());
        }
        assert_eq!(d.count(), 2);
        let asks = d
            .segment
            .events()
            .filter(|e| matches!(e.kind, EventKind::ApprovalRequested { .. }))
            .count();
        assert_eq!(asks, if tool == "read" { 2 } else { 1 });
        stop(&mut d);
        checked(d);
    }
}

#[test]
fn r02_cancel_approval_answers_every_pending_call_and_revokes_ticket() {
    scenario_from(enabled, |d| {
        propose(
            d,
            &[
                ("write", json!({"path":"new","text":"x"})),
                ("read", json!({"path":"a"})),
                ("bash", json!({"command":"true"})),
            ],
        );
        d.run_tools();
        let ticket = d.runtime.view().pending_approval.unwrap();
        let request = d.cancel();
        d.settle();
        d.accepted(request);
        assert_eq!(d.runtime.phase(), Phase::Durable(RuntimeState::Idle));
        assert_eq!(d.count(), 0);
        assert_eq!(results(d).len(), 3);
        assert!(results(d).iter().all(|r| r.status == ToolStatus::Cancelled
            && r.metadata.code.as_deref() == Some("cancelled")));
        let request = d.command(Command::SubmitApproval {
            approval_id: ticket.approval_id,
            plan_digest: ticket.plan_digest,
            decision: Decision::Allow,
            remember: true,
        });
        assert_eq!(d.rejected(request), Rejection::NotActive);
    });
}

#[test]
fn r02_executing_cancel_waits_for_executor_and_preserves_winning_success() {
    for success in [false, true] {
        let mut d = enabled();
        d.executor.success_wins.store(success, Ordering::Release);
        propose(
            &mut d,
            &[
                ("read", json!({"path":"a"})),
                ("read", json!({"path":"f1"})),
            ],
        );
        d.settle();
        d.plan();
        d.settle();
        assert!(d.executing.is_some());
        let request = d.cancel();
        d.settle();
        d.accepted(request);
        assert_eq!(d.runtime.phase(), Phase::Durable(RuntimeState::Cancelling));
        assert!(d.active_turn().is_some());
        d.execute();
        d.settle();
        assert_eq!(d.runtime.phase(), Phase::Durable(RuntimeState::Idle));
        let r = results(&d);
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0].status,
            if success {
                ToolStatus::Succeeded
            } else {
                ToolStatus::Cancelled
            }
        );
        assert_eq!(r[1].status, ToolStatus::Cancelled);
        assert_eq!(d.count(), 1);
        checked(d);
    }
}

#[test]
fn r01_crashes_in_every_new_phase_never_replay_and_revoke_approvals() {
    for gate in 0..10 {
        for fate in [Crash::Lost, Crash::Durable, Crash::Torn] {
            scenario_from(enabled, |d| {
                let ask = matches!(gate, 2 | 3 | 7 | 8 | 9);
                propose(
                    d,
                    &[(
                        if ask { "write" } else { "read" },
                        if ask {
                            json!({"path":"new","text":"x"})
                        } else {
                            json!({"path":"a"})
                        },
                    )],
                );
                d.complete(); // proposal ack, Authorizing transition in flight
                if gate >= 1 {
                    d.complete();
                } // Authorizing, PlanCall outstanding
                if gate >= 2 {
                    d.plan();
                } // request or intent in flight
                if gate >= 3 && gate != 4 {
                    d.complete();
                } // request/intent acknowledged
                let ticket = d.runtime.view().pending_approval.clone();
                if gate == 6 {
                    d.execute();
                } // executor result before acknowledgement
                if gate == 7 || gate == 8 {
                    d.approval(Decision::Deny, false);
                }
                if gate == 9 {
                    d.cancel();
                }
                if gate == 8 || gate == 9 {
                    d.complete();
                } // durable denial, result in flight
                let before = d.count();
                d.kill(fate);
                d.restart();
                d.settle();
                let uncertain = d.runtime.snapshot().unresolved_invocations().len();
                // Every durable read intent closes under D9, regardless of
                // whether dispatch or its result acknowledgement was reached.
                let expected = 0;
                assert_eq!(uncertain, expected, "uncertainty at gate {gate}, {fate:?}");
                d.acknowledge_all();
                assert_eq!(
                    d.count(),
                    before,
                    "restart invoked an executor: gate {gate} {fate:?}"
                );
                if let Some(ticket) = &ticket {
                    let request = d.command(Command::SubmitApproval {
                        approval_id: ticket.approval_id.clone(),
                        plan_digest: ticket.plan_digest.clone(),
                        decision: Decision::Allow,
                        remember: true,
                    });
                    assert_eq!(d.rejected(request), Rejection::NotActive);
                }
                if (gate == 8 || gate == 9) && fate != Crash::Durable {
                    assert_eq!(
                        results(d)[0].metadata.code.as_deref(),
                        Some("process_restart")
                    );
                    assert!(
                        d.runtime
                            .snapshot()
                            .events()
                            .all(|(_, _, e)| !matches!(e, JournalEvent::ExecutionUncertain(_)))
                    );
                }
                assert_eq!(d.runtime.phase(), Phase::Durable(RuntimeState::Idle));
            });
        }
    }
}

#[test]
fn provider_definitions_filter_enabled_available_in_registry_order() {
    let mut config = configuration();
    config["enabled_tools"] = json!(["bash", "edit", "read"]);
    let mut d = Driver::with_tools(
        known(),
        &serde_json::to_vec(&config).unwrap(),
        ToolSet::new([ExecutionBackend::TrustedFs]),
    );
    d.start("hello");
    d.settle();
    let definitions = d
        .segment
        .effects
        .iter()
        .find_map(|e| {
            if let Effect::StartInference { tools, .. } = e {
                Some(tools)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(
        definitions
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        vec!["read", "edit"]
    );
    stop(&mut d);
    checked(d);
    let mut d = Driver::new();
    propose(&mut d, &[("read", json!({"path":"a"}))]);
    d.settle();
    assert_eq!(d.count(), 0);
    assert_eq!(
        results(&d)[0].metadata.code.as_deref(),
        Some("tools_unavailable")
    );
    assert!(
        d.segment
            .effects
            .iter()
            .all(|e| !matches!(e, Effect::StartInference { tools, .. } if !tools.is_empty()))
    );
    stop(&mut d);
    checked(d);
}

#[test]
fn broken_policy_fails_turn_not_tool_and_runtime_remains_usable() {
    let mut config = configuration();
    config["network"] = json!("allow");
    let mut d = configured(config);
    propose(&mut d, &[("read", json!({"path":"a"}))]);
    d.run_tools();
    assert_eq!(d.count(), 0);
    assert_eq!(d.runtime.phase(), Phase::Durable(RuntimeState::Idle));
    assert!(d.runtime.snapshot().events().any(|(_, _, e)| matches!(e, JournalEvent::TurnFinished(f) if f.code.as_deref() == Some("policy_invalid"))));
    assert!(
        results(&d)
            .iter()
            .all(|r| r.metadata.code.as_deref() == Some("cancelled"))
    );
    d.start("ordinary answer still works");
    d.settle();
    stop(&mut d);
    checked(d);
}
