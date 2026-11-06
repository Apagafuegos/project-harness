//! Sequential tool pipeline, a child of the pure core so its authority never
//! escapes through a public constructor. Planning and execution remain effects.

use super::*;
use crate::session::RuntimeState::*;

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod tests;
use crate::session::{
    ApprovalId, ApprovalRequested, AuthorizationDecided, ConfigurationChanged, Decision,
    ExecutionIntent,
};
use crate::tools::{
    Allowance, Authorization, AuthorizedOperation, ExecutionOutcome, Plan, Refusal,
    ValidatedOperation, decide,
};

#[derive(Debug)]
pub(super) enum Activity {
    Validated {
        call: CallRef,
        tool: String,
        operation: ValidatedOperation,
    },
    Planning {
        token: PlanToken,
        call: CallRef,
        tool: String,
    },
    Planned(Plan),
    /// A trusted write/edit whose pre-image is being captured, before the
    /// intent that binds it may be committed.
    Capture {
        plan: Plan,
        /// The ticket the user just allowed, still bound to this plan.
        approval: Option<PendingApproval>,
        remember: bool,
        request: Option<RequestId>,
        token: CaptureToken,
    },
    Approval {
        plan: Plan,
        ticket: PendingApproval,
        timer: Option<TimerId>,
        expired: bool,
    },
    /// Decision 24: the denial is committed first; this result follows its ack.
    Denied((Option<TurnId>, EntryPayload)),
    Intent(AuthorizedOperation),
    Executing {
        token: ExecutionToken,
        operation: AuthorizedOperation,
        cancel_sent: bool,
        outcome: Option<ExecutionOutcome>,
    },
}

impl Runtime {
    pub(super) fn advance_tool(
        &mut self,
        snapshot: &SessionSnapshot,
        turn: TurnId,
        env: &mut dyn Env,
    ) {
        if matches!(self.tool, Some(Activity::Denied(_))) {
            let Some(Activity::Denied(result)) = self.tool.take() else {
                unreachable!()
            };
            self.commit_results(
                snapshot,
                &turn,
                vec![result],
                Vec::new(),
                idle_flight(),
                env,
            );
            return;
        }
        if snapshot.state() == Cancelling {
            if matches!(
                self.tool,
                Some(Activity::Executing {
                    outcome: Some(_),
                    ..
                })
            ) {
                self.record_tool_outcome(snapshot, &turn, env);
            }
            return;
        }
        if self.cancel.is_some() {
            if matches!(self.tool, Some(Activity::Capture { .. })) {
                // The capture is not a dispatch: drop it and answer the call.
                self.tool = None;
                self.cancel_unstarted(snapshot, &turn, None, env);
                return;
            }
            if matches!(self.tool, Some(Activity::Executing { .. })) {
                self.cancel_execution();
                self.persist(
                    audit(
                        snapshot,
                        vec![transition(&turn, snapshot.state(), Cancelling)],
                    ),
                    idle_flight(),
                    None,
                    env,
                );
            } else if matches!(self.tool, Some(Activity::Intent(_))) {
                let Some(Activity::Intent(operation)) = self.tool.take() else {
                    unreachable!()
                };
                // The intent is durable, even though dispatch lost the race to
                // cancellation. Give it an honest executor cancellation reply;
                // the executor must still observe the cancelled token in 4b.
                self.dispatch(operation);
                self.cancel_execution();
                self.persist(
                    audit(snapshot, vec![transition(&turn, Executing, Cancelling)]),
                    idle_flight(),
                    None,
                    env,
                );
            } else {
                self.cancel_unstarted(snapshot, &turn, None, env);
            }
            return;
        }
        match self.tool.take() {
            None if snapshot.state() == Processing => self.resolve_call(snapshot, &turn, env),
            Some(Activity::Validated {
                call,
                tool,
                operation,
            }) => {
                self.next_token += 1;
                let token = PlanToken(self.next_token);
                self.tool = Some(Activity::Planning {
                    token,
                    call: call.clone(),
                    tool: tool.clone(),
                });
                self.out.push(Effect::PlanCall {
                    token,
                    call,
                    tool,
                    operation,
                });
            }
            Some(Activity::Planned(plan)) => {
                let policy = match &self.policy {
                    Ok(p) => p,
                    Err(_) => {
                        return self.cancel_unstarted(snapshot, &turn, Some("policy_invalid"), env);
                    }
                };
                match decide(&plan, plan.facts(), policy) {
                    Authorization::Allow(_) => {
                        self.begin_allow(snapshot, &turn, plan, None, false, None, env)
                    }
                    Authorization::Deny(refusal) => {
                        let decision = self.authorization(
                            &turn,
                            &plan,
                            None,
                            Decision::Deny,
                            refusal.code.as_str(),
                            env,
                        );
                        self.record_refusal(
                            snapshot,
                            &turn,
                            plan.call(),
                            refusal,
                            vec![JournalEvent::AuthorizationDecided(decision)],
                            vec![plan.bytes().to_vec()],
                            env,
                        );
                    }
                    Authorization::Ask(_) => self.ask(snapshot, &turn, plan, env),
                }
            }
            Some(Activity::Approval {
                plan,
                ticket,
                timer,
                expired,
            }) => {
                let invalid = self
                    .policy
                    .as_ref()
                    .map_or(true, |p| p.revision() != plan.policy_revision())
                    || snapshot.policy_revision() != plan.policy_revision();
                if invalid || expired || env.now().instant() >= ticket.expires_at.instant() {
                    self.tool = Some(Activity::Approval {
                        plan,
                        ticket,
                        timer,
                        expired,
                    });
                    self.refuse_approval(
                        snapshot,
                        &turn,
                        if invalid {
                            ApprovalOutcome::Invalidated
                        } else {
                            ApprovalOutcome::Expired
                        },
                        None,
                        env,
                    );
                } else {
                    let timer = timer.or_else(|| {
                        self.next_timer += 1;
                        let timer = TimerId(self.next_timer);
                        let duration =
                            (ticket.expires_at.instant() - env.now().instant()).unsigned_abs();
                        self.out.push(Effect::SetTimer { timer, duration });
                        Some(timer)
                    });
                    self.tool = Some(Activity::Approval {
                        plan,
                        ticket,
                        timer,
                        expired,
                    });
                }
            }
            Some(activity @ Activity::Capture { .. }) => self.tool = Some(activity),
            Some(Activity::Intent(operation)) => self.dispatch(operation),
            Some(
                activity @ Activity::Executing {
                    outcome: Some(_), ..
                },
            ) => {
                self.tool = Some(activity);
                self.record_tool_outcome(snapshot, &turn, env);
            }
            Some(activity) => self.tool = Some(activity),
            None => self.fault(FaultCause::Internal("tool_phase_without_activity")),
        }
    }

    fn resolve_call(&mut self, snapshot: &SessionSnapshot, turn: &TurnId, env: &mut dyn Env) {
        let Some(call) = snapshot
            .0
            .pending(snapshot.head())
            .ok()
            .and_then(|p| p.first().cloned())
        else {
            return self.fault(FaultCause::Internal("processing_without_calls"));
        };
        let policy = match &self.policy {
            Ok(policy) => policy,
            Err(_) => return self.cancel_unstarted(snapshot, turn, Some("policy_invalid"), env),
        };
        let Some(StoredProposal { name, arguments }) = proposal(snapshot, &call) else {
            return self.fault(FaultCause::Internal("pending_proposal_missing"));
        };
        let resolved = (|| {
            let tool = self.tools.registry().resolve(&name, policy)?;
            if !self.tools.available(tool.definition().backend) {
                return Err(Refusal {
                    code: ResultCode::ToolsUnavailable,
                    status: ToolStatus::Denied,
                    detail: "the tool backend is unavailable".into(),
                });
            }
            tool.validate(&arguments)
        })();
        match resolved {
            Ok(operation) => {
                self.tool = Some(Activity::Validated {
                    call,
                    tool: name,
                    operation,
                });
                self.persist(
                    audit(snapshot, vec![transition(turn, Processing, Authorizing)]),
                    idle_flight(),
                    None,
                    env,
                );
            }
            Err(refusal) => {
                self.record_refusal(snapshot, turn, &call, refusal, Vec::new(), Vec::new(), env)
            }
        }
    }

    pub(super) fn planned(
        &mut self,
        token: PlanToken,
        result: Result<Plan, Refusal>,
        env: &mut dyn Env,
    ) {
        if self.mode != Mode::Ready
            || !matches!(&self.tool, Some(Activity::Planning { token: expected, .. }) if *expected == token)
        {
            return;
        }
        let Some(Activity::Planning { call, tool, .. }) = self.tool.take() else {
            unreachable!()
        };
        let snapshot = self.snapshot().clone();
        let turn = snapshot
            .active_turn()
            .cloned()
            .expect("planning active turn");
        match result {
            Ok(plan) => {
                if plan.call() != &call
                    || plan.name() != tool
                    || !self.tools.available(plan.backend())
                {
                    return self.fault(FaultCause::Internal("planning_binding_mismatch"));
                }
                self.tool = Some(Activity::Planned(plan));
                self.advance(env);
            }
            Err(refusal) => self.record_refusal(
                &snapshot,
                &turn,
                &call,
                refusal,
                Vec::new(),
                Vec::new(),
                env,
            ),
        }
    }

    fn ask(&mut self, snapshot: &SessionSnapshot, turn: &TurnId, plan: Plan, env: &mut dyn Env) {
        let expires_at = (env.now().instant() + time::Duration::minutes(5))
            .format(&time::format_description::well_known::Rfc3339)
            .expect("bounded expiry")
            .parse()
            .expect("UTC expiry");
        let ticket = PendingApproval {
            approval_id: env.approval_id(),
            call: plan.call().clone(),
            plan_digest: plan.digest().clone(),
            plan: plan.bytes().to_vec(),
            expires_at,
        };
        let request = ApprovalRequested {
            turn_id: turn.clone(),
            approval_id: ticket.approval_id.clone(),
            call: ticket.call.clone(),
            plan_digest: ticket.plan_digest.clone(),
            plan: artifact(plan.bytes()),
            policy_revision: plan.policy_revision().into(),
            expires_at: ticket.expires_at.clone(),
        };
        let flight = InFlight {
            artifacts: vec![plan.bytes().to_vec()],
            tool_events: vec![EventKind::ApprovalRequested {
                approval_id: ticket.approval_id.clone(),
                call: ticket.call.clone(),
                plan_digest: ticket.plan_digest.clone(),
                plan: ticket.plan.clone(),
                expires_at: ticket.expires_at.clone(),
            }],
            ..idle_flight()
        };
        self.tool = Some(Activity::Approval {
            plan,
            ticket,
            timer: None,
            expired: false,
        });
        self.persist(
            audit(snapshot, vec![JournalEvent::ApprovalRequested(request)]),
            flight,
            None,
            env,
        );
    }

    pub(super) fn submit_approval(
        &mut self,
        request: RequestId,
        approval_id: ApprovalId,
        plan_digest: Digest,
        decision: Decision,
        remember: bool,
        env: &mut dyn Env,
    ) -> Option<Result<Accepted, Rejection>> {
        if self.mode == Mode::Faulted {
            return Some(Err(Rejection::Faulted));
        }
        let Some(Activity::Approval { plan, ticket, .. }) = &self.tool else {
            return Some(Err(Rejection::NotActive));
        };
        if self.in_flight.is_some() || self.cancel.is_some() || ticket.approval_id != approval_id {
            return Some(Err(Rejection::NotActive));
        }
        if ticket.plan_digest != plan_digest {
            return Some(Err(Rejection::Invalid("approval_mismatch")));
        }
        let snapshot = self.snapshot().clone();
        let turn = snapshot
            .active_turn()
            .cloned()
            .expect("approval active turn");
        let invalid = self
            .policy
            .as_ref()
            .map_or(true, |p| p.revision() != plan.policy_revision())
            || snapshot.policy_revision() != plan.policy_revision();
        if invalid || env.now().instant() >= ticket.expires_at.instant() {
            self.refuse_approval(
                &snapshot,
                &turn,
                if invalid {
                    ApprovalOutcome::Invalidated
                } else {
                    ApprovalOutcome::Expired
                },
                Some(request),
                env,
            );
        } else if decision == Decision::Deny {
            self.refuse_approval(
                &snapshot,
                &turn,
                ApprovalOutcome::Denied,
                Some(request),
                env,
            );
        } else {
            let Some(Activity::Approval {
                plan,
                ticket,
                timer,
                ..
            }) = self.tool.take()
            else {
                unreachable!()
            };
            if let Some(timer) = timer {
                self.out.push(Effect::CancelTimer { timer });
            }
            self.begin_allow(
                &snapshot,
                &turn,
                plan,
                Some(ticket),
                remember,
                Some(request),
                env,
            );
        }
        None
    }

    fn authorization(
        &self,
        turn: &TurnId,
        plan: &Plan,
        approval_id: Option<ApprovalId>,
        decision: Decision,
        reason: &str,
        env: &mut dyn Env,
    ) -> AuthorizationDecided {
        AuthorizationDecided {
            turn_id: turn.clone(),
            decision_id: env.decision_id(),
            call: plan.call().clone(),
            approval_id,
            plan_digest: plan.digest().clone(),
            plan: artifact(plan.bytes()),
            policy_revision: plan.policy_revision().into(),
            decision,
            reason: reason.into(),
        }
    }

    /// An allow either captures the target's pre-image first or, for a call
    /// with no file mutation, commits immediately. Capture never extends an
    /// approval's lifetime and a refused capture dispatches nothing.
    #[allow(clippy::too_many_arguments)]
    fn begin_allow(
        &mut self,
        snapshot: &SessionSnapshot,
        turn: &TurnId,
        plan: Plan,
        approval: Option<PendingApproval>,
        remember: bool,
        request: Option<RequestId>,
        env: &mut dyn Env,
    ) {
        if plan
            .effects()
            .contains(crate::session::EffectClass::FilesystemMutation)
        {
            self.next_token += 1;
            let token = CaptureToken(self.next_token);
            self.tool = Some(Activity::Capture {
                plan: plan.clone(),
                approval,
                remember,
                request,
                token,
            });
            self.out.push(Effect::CaptureImage { token, plan });
            return;
        }
        self.allow_plan(
            snapshot,
            turn,
            plan,
            approval.map(|ticket| ticket.approval_id),
            remember,
            request,
            None,
            env,
        );
    }

    /// The capture finished. Revalidate the approval, then commit the intent
    /// that binds the image; a refusal records nothing executed.
    pub(super) fn captured(
        &mut self,
        token: CaptureToken,
        result: Result<crate::session::FileImage, Refusal>,
        env: &mut dyn Env,
    ) {
        if self.mode != Mode::Ready
            || !matches!(&self.tool, Some(Activity::Capture { token: expected, .. }) if *expected == token)
        {
            return;
        }
        let Some(Activity::Capture {
            plan,
            approval,
            remember,
            request,
            ..
        }) = self.tool.take()
        else {
            unreachable!()
        };
        let snapshot = self.snapshot().clone();
        let turn = snapshot
            .active_turn()
            .cloned()
            .expect("capture active turn");
        let image = match result {
            Ok(image) => image,
            Err(refusal) => {
                return self
                    .refuse_after_capture(&snapshot, &turn, plan, approval, request, refusal, env);
            }
        };
        if let Some(ticket) = approval {
            let invalid = self
                .policy
                .as_ref()
                .map_or(true, |policy| policy.revision() != plan.policy_revision())
                || snapshot.policy_revision() != plan.policy_revision();
            if invalid || env.now().instant() >= ticket.expires_at.instant() {
                // Capture must not give an expired or invalidated ticket a
                // second life: refuse exactly as an unanswered ticket would.
                self.tool = Some(Activity::Approval {
                    plan,
                    ticket,
                    timer: None,
                    expired: false,
                });
                self.refuse_approval(
                    &snapshot,
                    &turn,
                    if invalid {
                        ApprovalOutcome::Invalidated
                    } else {
                        ApprovalOutcome::Expired
                    },
                    request,
                    env,
                );
                return;
            }
            self.allow_plan(
                &snapshot,
                &turn,
                plan,
                Some(ticket.approval_id),
                remember,
                request,
                Some(image),
                env,
            );
        } else {
            self.allow_plan(&snapshot, &turn, plan, None, false, None, Some(image), env);
        }
    }

    /// A capture refusal: record the honest denial and nonexecuted refusal.
    #[allow(clippy::too_many_arguments)]
    fn refuse_after_capture(
        &mut self,
        snapshot: &SessionSnapshot,
        turn: &TurnId,
        plan: Plan,
        approval: Option<PendingApproval>,
        request: Option<RequestId>,
        refusal: Refusal,
        env: &mut dyn Env,
    ) {
        let decision = self.authorization(
            turn,
            &plan,
            approval.as_ref().map(|ticket| ticket.approval_id.clone()),
            Decision::Deny,
            refusal.code.as_str(),
            env,
        );
        let mut flight = idle_flight();
        if let Some(ticket) = &approval {
            flight.tool_events.push(EventKind::ApprovalResolved {
                approval_id: ticket.approval_id.clone(),
                outcome: ApprovalOutcome::Denied,
            });
            if let Some(request) = request {
                flight.waiters.push((
                    request,
                    Waiter::ApprovalSubmitted(ticket.approval_id.clone()),
                ));
            }
        }
        let result = runtime_result(
            turn,
            plan.call(),
            refusal.status,
            refusal.code,
            &refusal.text(),
        );
        self.tool = Some(Activity::Denied(result));
        self.persist(
            audit(snapshot, vec![JournalEvent::AuthorizationDecided(decision)]),
            flight,
            None,
            env,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn allow_plan(
        &mut self,
        snapshot: &SessionSnapshot,
        turn: &TurnId,
        plan: Plan,
        approval: Option<ApprovalId>,
        remember: bool,
        request: Option<RequestId>,
        image: Option<crate::session::FileImage>,
        env: &mut dyn Env,
    ) {
        let decision = self.authorization(
            turn,
            &plan,
            approval.clone(),
            Decision::Allow,
            "allowed",
            env,
        );
        let operation = AuthorizedOperation::new(
            plan.clone(),
            env.invocation_id(),
            decision.decision_id.clone(),
            image.clone(),
        );
        let intent = ExecutionIntent {
            turn_id: turn.clone(),
            invocation_id: operation.invocation_id().clone(),
            call: plan.call().clone(),
            decision_id: decision.decision_id.clone(),
            plan_digest: plan.digest().clone(),
            backend: plan.backend(),
            timeout_ms: plan.timeout_ms(),
            effects: Some(plan.effects().clone()),
            file_image: image,
        };
        // Intent consumes the old revision's authorization before the grant
        // changes that revision. All three events are one atomic transaction.
        let mut events = vec![
            JournalEvent::AuthorizationDecided(decision),
            JournalEvent::ExecutionIntent(intent),
        ];
        let mut flight = InFlight {
            artifacts: vec![plan.bytes().to_vec()],
            ..idle_flight()
        };
        if let Some(approval_id) = approval {
            flight.tool_events.push(EventKind::ApprovalResolved {
                approval_id: approval_id.clone(),
                outcome: ApprovalOutcome::Allowed,
            });
            if let Some(request) = request {
                flight
                    .waiters
                    .push((request, Waiter::ApprovalSubmitted(approval_id)));
            }
            if remember && let Some(allowance) = Allowance::remember(&plan) {
                let revision = env.policy_revision();
                let bytes = self
                    .policy
                    .as_ref()
                    .expect("validated policy")
                    .grant(allowance, &revision);
                let reference = artifact(&bytes);
                events.push(JournalEvent::ConfigurationChanged(ConfigurationChanged {
                    policy_revision: revision,
                    configuration: reference.clone(),
                }));
                flight.configuration = Some(
                    SessionConfiguration::parse(reference, &bytes)
                        .expect("grant preserves configuration"),
                );
                flight.artifacts.push(bytes);
            }
        }
        events.push(transition(turn, Authorizing, Executing));
        self.tool = Some(Activity::Intent(operation));
        self.persist(audit(snapshot, events), flight, None, env);
    }

    fn refuse_approval(
        &mut self,
        snapshot: &SessionSnapshot,
        turn: &TurnId,
        outcome: ApprovalOutcome,
        request: Option<RequestId>,
        env: &mut dyn Env,
    ) {
        let Some(Activity::Approval {
            plan,
            ticket,
            timer,
            ..
        }) = self.tool.take()
        else {
            return;
        };
        if let Some(timer) = timer {
            self.out.push(Effect::CancelTimer { timer });
        }
        let code = match outcome {
            ApprovalOutcome::Denied => ResultCode::ApprovalDenied,
            ApprovalOutcome::Expired => ResultCode::ApprovalExpired,
            ApprovalOutcome::Invalidated => ResultCode::ApprovalInvalidated,
            ApprovalOutcome::Cancelled => ResultCode::Cancelled,
            ApprovalOutcome::Allowed => unreachable!(),
        };
        let decision = self.authorization(
            turn,
            &plan,
            Some(ticket.approval_id.clone()),
            Decision::Deny,
            code.as_str(),
            env,
        );
        let mut flight = idle_flight();
        flight.tool_events.push(EventKind::ApprovalResolved {
            approval_id: ticket.approval_id.clone(),
            outcome,
        });
        if let Some(request) = request {
            flight
                .waiters
                .push((request, Waiter::ApprovalSubmitted(ticket.approval_id)));
        }
        let result = runtime_result(
            turn,
            plan.call(),
            if outcome == ApprovalOutcome::Cancelled {
                ToolStatus::Cancelled
            } else {
                ToolStatus::Denied
            },
            code,
            &format!("{}: approval no longer authorizes execution", code.as_str()),
        );
        self.tool = Some(Activity::Denied(result));
        self.persist(
            audit(snapshot, vec![JournalEvent::AuthorizationDecided(decision)]),
            flight,
            None,
            env,
        );
    }

    pub(super) fn approval_timer(&mut self, timer: TimerId, env: &mut dyn Env) -> bool {
        if let Some(Activity::Approval {
            timer: armed,
            expired,
            ..
        }) = &mut self.tool
            && *armed == Some(timer)
        {
            *armed = None;
            *expired = true;
            self.advance(env);
            true
        } else {
            false
        }
    }

    fn dispatch(&mut self, operation: AuthorizedOperation) {
        self.next_token += 1;
        let token = ExecutionToken(self.next_token);
        self.emit(EventKind::ToolStarted {
            call: operation.call().clone(),
            invocation_id: operation.invocation_id().clone(),
        });
        self.out.push(Effect::Execute {
            token,
            operation: operation.clone(),
        });
        self.tool = Some(Activity::Executing {
            token,
            operation,
            cancel_sent: false,
            outcome: None,
        });
    }

    pub(super) fn cancel_execution(&mut self) {
        if let Some(Activity::Executing {
            token, cancel_sent, ..
        }) = &mut self.tool
            && !*cancel_sent
        {
            *cancel_sent = true;
            self.out.push(Effect::CancelExecution { token: *token });
        }
    }

    pub(super) fn clear_tool(&mut self) {
        if let Some(Activity::Capture { .. }) = self.tool.take() {
            return;
        }
        if let Some(Activity::Approval {
            timer: Some(timer), ..
        }) = self.tool.take()
        {
            self.out.push(Effect::CancelTimer { timer });
        }
    }

    pub(super) fn tool_finished(
        &mut self,
        token: ExecutionToken,
        outcome: ExecutionOutcome,
        env: &mut dyn Env,
    ) {
        if self.mode != Mode::Ready {
            return;
        }
        let Some(Activity::Executing {
            token: expected,
            outcome: recorded,
            ..
        }) = &mut self.tool
        else {
            return;
        };
        if *expected != token || recorded.is_some() {
            return;
        }
        if !matches!(
            outcome.status,
            ToolStatus::Succeeded
                | ToolStatus::Failed
                | ToolStatus::TimedOut
                | ToolStatus::Cancelled
                | ToolStatus::InfrastructureFailure
        ) || (outcome.status == ToolStatus::Succeeded) != outcome.code.is_none()
            || serde_json::to_vec(&outcome.content).map_or(true, |b| b.len() > 32 * 1024)
            || outcome.content.iter().any(|b| {
                matches!(
                    b,
                    ContentBlock::ToolCall { .. }
                        | ContentBlock::ProviderState { .. }
                        | ContentBlock::ReasoningSummary { .. }
                )
            })
        {
            return self.fault(FaultCause::Internal("invalid_executor_outcome"));
        }
        *recorded = Some(outcome);
        self.advance(env);
    }

    fn record_tool_outcome(
        &mut self,
        snapshot: &SessionSnapshot,
        turn: &TurnId,
        env: &mut dyn Env,
    ) {
        let Some(Activity::Executing {
            operation,
            outcome: Some(outcome),
            ..
        }) = self.tool.take()
        else {
            unreachable!()
        };
        let result = ToolResult {
            call: operation.call().clone(),
            invocation_id: Some(operation.invocation_id().clone()),
            status: outcome.status,
            content: outcome.content,
            metadata: ToolMetadata {
                exit_code: outcome.exit_code,
                duration_ms: outcome.duration_ms,
                output_truncated: outcome.output_truncated,
                provenance: Provenance::Executor,
                artifacts: outcome.artifacts.iter().map(|b| artifact(b)).collect(),
                code: outcome.code.map(|c| c.as_str().into()),
                file_effect: outcome.file_effect,
                file_paths: outcome.file_paths,
            },
        };
        let flight = InFlight {
            artifacts: outcome.artifacts,
            ..idle_flight()
        };
        self.commit_results(
            snapshot,
            turn,
            vec![(
                Some(turn.clone()),
                EntryPayload::ToolResult(ToolResultEntry { result }),
            )],
            Vec::new(),
            flight,
            env,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn record_refusal(
        &mut self,
        snapshot: &SessionSnapshot,
        turn: &TurnId,
        call: &CallRef,
        refusal: Refusal,
        events: Vec<JournalEvent>,
        artifacts: Vec<Vec<u8>>,
        env: &mut dyn Env,
    ) {
        let result = runtime_result(turn, call, refusal.status, refusal.code, &refusal.text());
        let flight = InFlight {
            artifacts,
            ..idle_flight()
        };
        if events.is_empty() {
            self.commit_results(snapshot, turn, vec![result], events, flight, env);
        } else {
            self.tool = Some(Activity::Denied(result));
            self.persist(audit(snapshot, events), flight, None, env);
        }
    }

    fn cancel_unstarted(
        &mut self,
        snapshot: &SessionSnapshot,
        turn: &TurnId,
        failure: Option<&str>,
        env: &mut dyn Env,
    ) {
        let mut flight = idle_flight();
        let mut events = Vec::new();
        if matches!(self.tool, Some(Activity::Approval { .. })) {
            self.refuse_approval(snapshot, turn, ApprovalOutcome::Cancelled, None, env);
            return;
        }
        self.tool = None;
        let results = snapshot
            .0
            .pending(snapshot.head())
            .expect("pending calls")
            .iter()
            .map(|call| {
                runtime_result(
                    turn,
                    call,
                    ToolStatus::Cancelled,
                    ResultCode::Cancelled,
                    NOT_STARTED,
                )
            })
            .collect();
        events.push(transition(turn, snapshot.state(), Cancelling));
        let finished = match failure {
            Some(code) => turn_finished(turn, TurnOutcome::Failed, Some(code)),
            None => cancelled(turn),
        };
        events.push(JournalEvent::TurnFinished(finished.clone()));
        flight.finished = Some(finished);
        let entries = chain(snapshot, results, env);
        let Ok(events_for_results) = result_events(&entries) else {
            return self.fault(FaultCause::Internal("invalid_stored_result_code"));
        };
        flight.tool_events.extend(events_for_results);
        self.persist(advancing(snapshot, entries, events), flight, None, env);
    }

    fn commit_results(
        &mut self,
        snapshot: &SessionSnapshot,
        turn: &TurnId,
        mut results: Vec<(Option<TurnId>, EntryPayload)>,
        mut events: Vec<JournalEvent>,
        mut flight: InFlight,
        env: &mut dyn Env,
    ) {
        self.tool = None;
        let pending = snapshot.0.pending(snapshot.head()).expect("pending calls");
        let cancel = snapshot.state() == Cancelling || self.cancel.is_some();
        if cancel {
            results.extend(pending.iter().skip(results.len()).map(|call| {
                runtime_result(
                    turn,
                    call,
                    ToolStatus::Cancelled,
                    ResultCode::Cancelled,
                    NOT_STARTED,
                )
            }));
            if snapshot.state() != Cancelling {
                events.push(transition(turn, snapshot.state(), Cancelling));
            }
            flight.finished = Some(cancelled(turn));
        } else {
            let more = pending.len() > results.len();
            let state = snapshot.state();
            if state == Executing {
                events.push(transition(turn, Executing, Continuing));
            }
            if more {
                if state != Processing {
                    events.push(transition(
                        turn,
                        if state == Executing {
                            Continuing
                        } else {
                            state
                        },
                        Processing,
                    ));
                }
            } else {
                if state != Executing {
                    events.push(transition(turn, state, Continuing));
                }
                if self.inferences >= self.config.max_inferences_per_turn {
                    flight.finished = Some(turn_finished(
                        turn,
                        TurnOutcome::Failed,
                        Some("inference_limit"),
                    ));
                } else {
                    events.push(transition(turn, Continuing, Preparing));
                }
            }
        }
        events.extend(flight.finished.clone().map(JournalEvent::TurnFinished));
        let entries = chain(snapshot, results, env);
        let Ok(events_for_results) = result_events(&entries) else {
            return self.fault(FaultCause::Internal("invalid_stored_result_code"));
        };
        flight.tool_events.extend(events_for_results);
        self.persist(advancing(snapshot, entries, events), flight, None, env);
    }
}

fn artifact(bytes: &[u8]) -> crate::session::ArtifactRef {
    crate::session::ArtifactRef {
        sha256: Digest::of(bytes),
        bytes: bytes.len() as u64,
    }
}

fn result_events(entries: &[Entry]) -> Result<Vec<EventKind>, ()> {
    entries
        .iter()
        .filter_map(|entry| match &entry.payload {
            EntryPayload::ToolResult(r) => Some(&r.result),
            _ => None,
        })
        .map(|result| {
            let code = result
                .metadata
                .code
                .as_deref()
                .map(|code| ResultCode::parse(code).ok_or(()))
                .transpose()?;
            Ok(EventKind::ToolFinished {
                call: result.call.clone(),
                status: result.status,
                code,
            })
        })
        .collect()
}
struct StoredProposal {
    name: String,
    arguments: serde_json::Value,
}
fn proposal(snapshot: &SessionSnapshot, call: &CallRef) -> Option<StoredProposal> {
    let stored = snapshot.entry(&call.response_entry_id)?;
    let EntryPayload::AssistantResponse(response) = &stored.entry.payload else {
        return None;
    };
    response.content.iter().find_map(|block| {
        if let ContentBlock::ToolCall {
            call_id,
            name,
            arguments,
            ..
        } = block
            && call_id == &call.call_id
        {
            Some(StoredProposal {
                name: name.clone(),
                arguments: serde_json::Value::Object(arguments.clone()),
            })
        } else {
            None
        }
    })
}
