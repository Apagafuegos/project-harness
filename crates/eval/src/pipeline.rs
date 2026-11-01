//! The pipeline: replay a task's history through the real runtime, compact once
//! per strategy, score the result, continue the task and run the hidden check.
//!
//! Every stage uses product code. The session is created through the shared
//! wiring (so the tool host, instructions and compaction policy are the
//! product's), the history is replayed through `SessionHandle`, the compaction
//! goes through `PreviewCompaction`/`CommitCompaction`, tool calls execute for
//! real against the scratch workspace, and the hidden check runs in the existing
//! Bubblewrap sandbox. Nothing here is a second implementation of the product.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use harness::{wiring, wiring::SessionOptions};
use harness_core::{
    context::{ContextItem, ContextReport, ItemKind, Part, StrategyId},
    model::{
        ModelProvider, ProviderEvent,
        scripted::{Script, ScriptedProvider, Step as ModelStep},
    },
    runtime::*,
    session::*,
    tools::SandboxConfig,
};
use serde_json::Value;

use crate::{
    check::HiddenCheck,
    matcher::{Requirement, normalize},
    record::{Estimator, Metrics, ModelName, Record, Tokens},
    task::{Step, Task, copy_workspace},
};

/// The probe prompt. Fixed by the harness, never by a task, so every strategy is
/// asked the same question about the same task.
pub const PROBE_PROMPT: &str = "Before we continue: restate (a) every requirement stated in this \
conversation and (b) every file changed so far. Be concise and use your own words only for the \
restatement.";

/// Bounds on one stage, so a stuck runtime fails the run instead of the gate.
const STAGE_TIMEOUT: Duration = Duration::from_secs(180);

/// How the post-history stages are answered.
#[derive(Clone)]
pub enum ModelPlan {
    /// Answer the summary, probe and continuation from the task's scripts. This
    /// is what `--offline` uses; it proves mechanics, not model quality.
    Scripted,
    /// Answer them with a real provider. Opt-in live mode only.
    Live(Arc<dyn ModelProvider>),
}

/// Per-run configuration shared by every cell of the matrix.
#[derive(Clone)]
pub struct RunOptions {
    /// Scratch root; each cell gets its own directory below it.
    pub scratch_root: PathBuf,
    /// `git rev-parse HEAD`, supplied by the caller.
    pub commit: String,
    /// Repetitions per task × strategy. Offline runs are deterministic, so more
    /// than one only proves that.
    pub repetitions: usize,
    /// Session model.
    pub model: ModelRef,
    /// Summary model for the strategies that use one; defaults to the session
    /// model.
    pub summary_model: Option<ModelRef>,
    /// Runtime defaults, with the sourced model catalog already installed.
    pub driver: DriverConfig,
}

/// One cell's scores plus the texts they were computed from.
pub struct TaskRun {
    /// The result record.
    pub record: Record,
    /// The post-compaction projection text that was scored.
    pub projection_text: String,
    /// The probe answer that was scored.
    pub probe_text: String,
    /// Every request the continuation sent, rendered for leak assertions.
    pub continuation_requests: Vec<String>,
    /// The scratch workspace, for the hidden check the caller may re-run.
    pub workspace: PathBuf,
}

/// Failure of a stage, as a stable code.
type Stage<T> = Result<T, String>;

/// Run one task × strategy × repetition cell.
pub async fn run_task(
    task: &Task,
    strategy: StrategyId,
    repetition: usize,
    options: &RunOptions,
    plan: &ModelPlan,
    check: &HiddenCheck,
) -> TaskRun {
    let started = Instant::now();
    let summary_model = strategy.needs_model().then(|| {
        options
            .summary_model
            .clone()
            .unwrap_or_else(|| options.model.clone())
    });
    let mut record = Record::skeleton(
        task,
        strategy,
        &options.model,
        summary_model.as_ref(),
        repetition,
        &options.commit,
    );
    let scratch = options.scratch_root.join(format!(
        "{}.{}.{}",
        task.name(),
        strategy.as_str(),
        repetition
    ));
    let workspace = scratch.join("workspace");
    let state = scratch.join("state");
    let mut projection_text = String::new();
    let mut probe_text = String::new();
    let mut continuation_requests = Vec::new();

    let prepared = prepare_scratch(task, &scratch, &workspace, &state);
    if let Err(error) = prepared {
        record.errors.replay = Some("scratch_unavailable".into());
        record.durations_ms.total_ms = millis(started);
        eprintln!("{}: scratch: {error}", task.name());
        return TaskRun {
            record,
            projection_text,
            probe_text,
            continuation_requests,
            workspace,
        };
    }

    match drive(
        task,
        strategy,
        options,
        plan,
        summary_model.as_ref(),
        &mut record,
        &workspace,
        &state,
        &mut projection_text,
        &mut probe_text,
        &mut continuation_requests,
    )
    .await
    {
        Ok(()) => {}
        Err(code) => {
            if record.errors.replay.is_none() {
                record.errors.replay = Some(code);
            }
        }
    }

    // The hidden check runs after the session is closed, so nothing the check
    // does can reach the model and nothing the model did can see the check.
    if record.errors.compaction.is_none() && record.errors.continuation.is_none() {
        let check_started = Instant::now();
        let outcome = check.run_check(task.check(), &workspace);
        record.durations_ms.check_ms = millis(check_started);
        if let Some(metrics) = record.metrics.as_mut() {
            metrics.completion = Some(outcome.passed);
        }
        // A check that ran and failed is a metric (`completion: false`), not a
        // mechanical error; only an executor failure that prevented the check
        // from running is recorded as an error code.
        if outcome
            .code
            .as_deref()
            .is_some_and(|code| code != "nonzero_exit")
        {
            record.errors.check = outcome.code.clone();
        }
    }
    record.durations_ms.total_ms = millis(started);
    TaskRun {
        record,
        projection_text,
        probe_text,
        continuation_requests,
        workspace,
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    task: &Task,
    strategy: StrategyId,
    options: &RunOptions,
    plan: &ModelPlan,
    summary_model: Option<&ModelRef>,
    record: &mut Record,
    workspace: &Path,
    state: &Path,
    projection_text: &mut String,
    probe_text: &mut String,
    continuation_requests: &mut Vec<String>,
) -> Stage<()> {
    let (scripted, provider): (Option<Arc<ScriptedProvider>>, Arc<dyn ModelProvider>) = match plan {
        ModelPlan::Scripted => {
            let provider = Arc::new(ScriptedProvider::new([]));
            (Some(provider.clone()), provider as Arc<dyn ModelProvider>)
        }
        ModelPlan::Live(provider) => (None, provider.clone()),
    };
    let mut known_instances = vec![options.model.provider_instance.clone()];
    if let Some(summary) = summary_model {
        known_instances.push(summary.provider_instance.clone());
    }
    let session_options = SessionOptions {
        data_root: state.to_path_buf(),
        workspace: workspace.to_path_buf(),
        session: None,
        model: options.model.clone(),
        explicit_model: false,
        explicit_provider: false,
        switch_policy: Portability::Reject,
        known_instances,
        summary_model: summary_model.cloned(),
        compaction_strategy: strategy,
        explicit_strategy: true,
        driver: options.driver.clone(),
        // Auto-approval is not a policy: the product policy still says `ask` for
        // every mutation and every process. The eval answers those approvals
        // itself, because it runs unattended in a scratch workspace.
        sandbox: Some(SandboxConfig {
            workspace_root: workspace.to_path_buf(),
            ..SandboxConfig::default()
        }),
    };
    let wired = wiring::open_session(&session_options, provider.clone())
        .await
        .map_err(|(_, code)| code)?;
    let handle = &wired.handle;
    let mut session = Session {
        handle,
        scripted: scripted.as_ref(),
        submitted: BTreeSet::new(),
    };

    // 1. Replay the history through the real runtime.
    let replay_started = Instant::now();
    let mut turn_ids = Vec::new();
    for (index, turn) in task.turns().iter().enumerate() {
        if let Some(scripted) = session.scripted {
            for step in &turn.steps {
                scripted.push(script_for(step));
            }
        }
        let turn_id = session.start(&turn.prompt).await?;
        session.settle().await?;
        let snapshot = session.snapshot().await?;
        let (outcome, code) = turn_result(&snapshot, &turn_id);
        if outcome != TurnOutcome::Completed {
            record.errors.replay = Some(code.unwrap_or_else(|| format!("turn_{}", index + 1)));
            handle.shutdown().await;
            return Ok(());
        }
        turn_ids.push(turn_id);
    }
    record.durations_ms.replay_ms = millis(replay_started);

    // 2. Apply the task's controls, before compacting.
    for ordinal in &task.controls().pin {
        session
            .control(&turn_ids[ordinal - 1], ContextMode::Pinned)
            .await?;
    }
    for ordinal in &task.controls().exclude {
        session
            .control(&turn_ids[ordinal - 1], ContextMode::Excluded)
            .await?;
    }

    // 3. One compaction, through the real commands, forced regardless of budget.
    let compaction_started = Instant::now();
    if strategy.needs_model()
        && let Some(scripted) = &session.scripted
    {
        scripted.push(reply_script(&task.scripted().summary));
    }
    let preview = match handle.command(Command::PreviewCompaction).await {
        Ok(Accepted::CompactionPreviewed {
            preview_id,
            preview,
        }) => Some((preview_id, preview)),
        Ok(other) => {
            record.errors.compaction = Some(format!("unexpected_reply:{}", kind_name(&other)));
            None
        }
        Err(rejection) => {
            record.errors.compaction = Some(rejection_code(&rejection));
            None
        }
    };
    let Some((preview_id, preview)) = preview else {
        handle.shutdown().await;
        record.durations_ms.compaction_ms = millis(compaction_started);
        return Ok(());
    };
    {
        let EntryPayload::Compaction(compaction) = &preview.entry.payload else {
            record.errors.compaction = Some("preview_is_not_a_compaction".into());
            handle.shutdown().await;
            return Ok(());
        };
        record.strategy = compaction.strategy.clone();
        record.strategy_version = compaction.strategy_version.clone();
        record.summary_model = compaction.model.as_ref().map(model_name);
        record.configuration_fingerprint = Some(digest_text(&preview.binding.policy_fingerprint));
        record.estimator = Some(Estimator {
            method: estimator_method(&preview.before.totals.estimator.method),
            version: preview.before.totals.estimator.version.clone(),
        });
        record.tokens = Some(Tokens {
            before: preview.diff.before.estimated_total,
            after: preview.diff.after.estimated_total,
        });
    }
    match handle
        .command(Command::CommitCompaction { preview_id })
        .await
    {
        Ok(Accepted::CompactionCommitted { .. }) => {}
        Ok(other) => {
            record.errors.compaction = Some(format!("unexpected_reply:{}", kind_name(&other)));
            handle.shutdown().await;
            return Ok(());
        }
        Err(rejection) => {
            record.errors.compaction = Some(rejection_code(&rejection));
            handle.shutdown().await;
            return Ok(());
        }
    }
    record.durations_ms.compaction_ms = millis(compaction_started);

    // 4. Score the post-compaction projection.
    let snapshot = session.snapshot().await?;
    let report = handle
        .inspect_context(None, None)
        .await
        .map_err(|error| format!("inspect: {error:?}"))?;
    *projection_text = render_projection(&snapshot, &report);
    let projection_score = score(task, projection_text);

    // 5. Ask the probe and score its answer.
    let probe_started = Instant::now();
    if let Some(scripted) = &session.scripted {
        scripted.push(reply_script(&task.scripted().probe));
    }
    let probe_turn = match session.start(PROBE_PROMPT).await {
        Ok(turn) => turn,
        Err(code) => {
            record.errors.probe = Some(code);
            handle.shutdown().await;
            return Ok(());
        }
    };
    session.settle().await?;
    let snapshot = session.snapshot().await?;
    let (outcome, code) = turn_result(&snapshot, &probe_turn);
    if outcome != TurnOutcome::Completed {
        record.errors.probe = Some(code.unwrap_or_else(|| "probe_failed".into()));
        handle.shutdown().await;
        return Ok(());
    }
    *probe_text = assistant_text(&snapshot, &probe_turn);
    record.durations_ms.probe_ms = millis(probe_started);
    let probe_score = score(task, probe_text);

    record.metrics = Some(Metrics {
        requirements_retained_projection: projection_score.requirements,
        requirements_retained_probe: probe_score.requirements,
        requirements_total: task.requirements().len() as u64,
        files_recalled_projection: projection_score.files,
        files_recalled_probe: probe_score.files,
        files_total: task.modified_files().len() as u64,
        completion: None,
    });

    // 6. Continue the task through the normal tool loop.
    let continuation_started = Instant::now();
    let streams_before = session.scripted.map(|provider| provider.streams().len());
    if let (Some(scripted), ModelPlan::Scripted) = (&session.scripted, plan) {
        for step in &task.scripted().continuation {
            scripted.push(script_for(step));
        }
    }
    let continuation_turn = match session.start(task.continue_prompt()).await {
        Ok(turn) => turn,
        Err(code) => {
            record.errors.continuation = Some(code);
            handle.shutdown().await;
            return Ok(());
        }
    };
    session.settle().await?;
    let snapshot = session.snapshot().await?;
    let (outcome, code) = turn_result(&snapshot, &continuation_turn);
    if outcome != TurnOutcome::Completed {
        record.errors.continuation = Some(code.unwrap_or_else(|| "continuation_failed".into()));
    }
    if let (Some(scripted), Some(before)) = (session.scripted, streams_before) {
        for record in scripted.streams().into_iter().skip(before) {
            continuation_requests.push(render_items(&record.request.input.items));
        }
    }
    record.durations_ms.continuation_ms = millis(continuation_started);
    handle.shutdown().await;
    Ok(())
}

struct Session<'a> {
    handle: &'a SessionHandle,
    scripted: Option<&'a Arc<ScriptedProvider>>,
    submitted: BTreeSet<ApprovalId>,
}

impl Session<'_> {
    async fn start(&mut self, prompt: &str) -> Stage<TurnId> {
        let reply = tokio::time::timeout(
            STAGE_TIMEOUT,
            self.handle.command(Command::StartTurn {
                command_id: CommandId::new(),
                content: vec![ContentBlock::Text {
                    text: prompt.to_owned(),
                }],
            }),
        )
        .await
        .map_err(|_| "start_timeout".to_owned())?;
        match reply {
            Ok(Accepted::TurnStarted { turn_id, .. }) => Ok(turn_id),
            Ok(other) => Err(format!("unexpected_reply:{}", kind_name(&other))),
            Err(rejection) => Err(rejection_code(&rejection)),
        }
    }

    async fn control(&mut self, turn: &TurnId, mode: ContextMode) -> Stage<()> {
        match self
            .handle
            .command(Command::SetContextControl {
                turn_id: turn.clone(),
                mode,
            })
            .await
        {
            Ok(Accepted::ContextControlSet { .. }) => Ok(()),
            Ok(other) => Err(format!("unexpected_reply:{}", kind_name(&other))),
            Err(rejection) => Err(rejection_code(&rejection)),
        }
    }

    async fn snapshot(&self) -> Stage<SessionSnapshot> {
        Ok(self
            .handle
            .view()
            .await
            .map_err(|error| format!("view: {error:?}"))?
            .snapshot)
    }

    /// Poll the runtime until the turn is finished, answering every approval the
    /// eval is asked for. This is the only auto-approval in the harness.
    async fn settle(&mut self) -> Stage<RuntimeView> {
        let deadline = tokio::time::Instant::now() + STAGE_TIMEOUT;
        loop {
            let view = self
                .handle
                .view()
                .await
                .map_err(|error| format!("view: {error:?}"))?;
            match view.phase {
                Phase::Faulted => return Err("runtime_faulted".into()),
                Phase::AwaitingResolution => return Err("resolution_required".into()),
                Phase::RecoveryRequired | Phase::Durable(_) => {}
            }
            if let Some(ticket) = view.pending_approval.clone()
                && self.submitted.insert(ticket.approval_id.clone())
            {
                let reply = tokio::time::timeout(
                    STAGE_TIMEOUT,
                    self.handle.command(Command::SubmitApproval {
                        approval_id: ticket.approval_id,
                        plan_digest: ticket.plan_digest,
                        decision: Decision::Allow,
                        remember: false,
                    }),
                )
                .await
                .map_err(|_| "approval_timeout".to_owned())?;
                if let Err(rejection) = reply {
                    return Err(format!("approval:{}", rejection_code(&rejection)));
                }
            }
            if view.phase == Phase::Durable(RuntimeState::Idle) && view.active_turn.is_none() {
                return Ok(view);
            }
            if tokio::time::Instant::now() > deadline {
                return Err("stage_timeout".into());
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

struct Score {
    requirements: u64,
    files: u64,
}

fn score(task: &Task, text: &str) -> Score {
    let normalized = normalize(text);
    Score {
        requirements: task
            .requirements()
            .iter()
            .filter(|requirement| requirement.matcher.matches(&normalized))
            .count() as u64,
        files: task
            .modified_files()
            .iter()
            .filter(|path| normalized.contains(&normalize(path)))
            .count() as u64,
    }
}

/// Which requirements and files a text retained, for diagnostics in a test.
pub fn retained_details(task: &Task, text: &str) -> Vec<(String, bool)> {
    let normalized = normalize(text);
    task.requirements()
        .iter()
        .map(|requirement: &Requirement| {
            (
                requirement.id.clone(),
                requirement.matcher.matches(&normalized),
            )
        })
        .collect()
}

/// Render the post-compaction projection from the product's own report and
/// journal. Instructions are skipped: they are fixed scaffolding, identical in
/// every arm, and never derived from the task's history.
fn render_projection(snapshot: &SessionSnapshot, report: &ContextReport) -> String {
    let mut parts: Vec<String> = Vec::new();
    for item in &report.included {
        if item.kind == ItemKind::Instructions {
            continue;
        }
        let Some(entry_id) = &item.entry_id else {
            continue;
        };
        let Some(stored) = snapshot.entry(entry_id) else {
            continue;
        };
        match &stored.entry.payload {
            EntryPayload::Compaction(compaction) => parts.push(compaction.summary.clone()),
            EntryPayload::UserMessage(message) => {
                parts.extend(text_blocks(&message.content));
            }
            EntryPayload::ToolResult(entry) => {
                parts.extend(text_blocks(&entry.result.content));
            }
            EntryPayload::AssistantResponse(response) => {
                let included: BTreeSet<u32> = item
                    .blocks
                    .iter()
                    .filter_map(|block| match block.part {
                        Part::Block { index } => Some(index),
                        _ => None,
                    })
                    .collect();
                for (index, block) in response.content.iter().enumerate() {
                    if !included.contains(&(index as u32)) {
                        continue;
                    }
                    match block {
                        ContentBlock::Text { text } => parts.push(text.clone()),
                        ContentBlock::ToolCall {
                            name, arguments, ..
                        } => parts.push(format!("{name} {}", Value::Object(arguments.clone()))),
                        _ => {}
                    }
                }
            }
            EntryPayload::ModelChanged(_) | EntryPayload::ContextControl(_) => {}
        }
    }
    parts.join("\n")
}

fn render_items(items: &[ContextItem]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for item in items {
        match item {
            ContextItem::Instructions { instructions } => {
                parts.extend(
                    instructions
                        .iter()
                        .map(|instruction| instruction.text.clone()),
                );
            }
            ContextItem::User { content, .. } => parts.extend(text_blocks(content)),
            ContextItem::Assistant { content, .. } => {
                for block in content {
                    match block {
                        ContentBlock::Text { text } => parts.push(text.clone()),
                        ContentBlock::ToolCall {
                            name, arguments, ..
                        } => parts.push(format!("{name} {}", Value::Object(arguments.clone()))),
                        _ => {}
                    }
                }
            }
            ContextItem::ToolResult { result, .. } => parts.extend(text_blocks(&result.content)),
            ContextItem::Summary { summary, .. } => parts.push(summary.clone()),
        }
    }
    parts.join("\n")
}

fn text_blocks(content: &[ContentBlock]) -> Vec<String> {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn assistant_text(snapshot: &SessionSnapshot, turn: &TurnId) -> String {
    let Ok(branch) = snapshot.ancestry(snapshot.head()) else {
        return String::new();
    };
    branch
        .iter()
        .rev()
        .find_map(|stored| match &stored.entry.payload {
            EntryPayload::AssistantResponse(response)
                if stored.entry.turn_id == Some(turn.clone()) =>
            {
                Some(text_blocks(&response.content).join("\n"))
            }
            _ => None,
        })
        .unwrap_or_default()
}

fn turn_result(snapshot: &SessionSnapshot, turn: &TurnId) -> (TurnOutcome, Option<String>) {
    snapshot
        .events()
        .find_map(|(_, _, event)| match event {
            JournalEvent::TurnFinished(finished) if &finished.turn_id == turn => {
                Some((finished.outcome, finished.code.clone()))
            }
            _ => None,
        })
        .unwrap_or((TurnOutcome::Failed, Some("turn_unfinished".into())))
}

fn script_for(step: &Step) -> Script {
    match step {
        Step::Assistant { text } => reply_script(text),
        Step::ToolCall {
            tool,
            arguments,
            say,
        } => {
            let mut steps = vec![ModelStep::Event(ProviderEvent::Started)];
            let call_index = if say.is_empty() {
                0
            } else {
                steps.push(ModelStep::text(0, say.clone()));
                1
            };
            steps.extend([
                ModelStep::Event(ProviderEvent::ToolCallStarted {
                    block_index: call_index,
                    name: tool.clone(),
                    provider_call_id: None,
                }),
                ModelStep::Event(ProviderEvent::ToolArgumentsDelta {
                    block_index: call_index,
                    fragment: arguments.to_string(),
                }),
                ModelStep::Event(ProviderEvent::ToolCallReady {
                    block_index: call_index,
                }),
                ModelStep::completed(FinishReason::ToolCalls),
            ]);
            Script::new(steps)
        }
        Step::User { .. } => Script::default(),
    }
}

fn reply_script(text: &str) -> Script {
    Script::new([
        ModelStep::Event(ProviderEvent::Started),
        ModelStep::text(0, text),
        ModelStep::completed(FinishReason::Stop),
    ])
}

fn model_name(model: &ModelRef) -> ModelName {
    format!("{}/{}", model.provider_instance, model.model)
}

fn digest_text(digest: &Digest) -> String {
    serde_json::to_value(digest)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn estimator_method(method: &harness_core::context::EstimateMethod) -> String {
    serde_json::to_value(method)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn kind_name(accepted: &Accepted) -> &'static str {
    match accepted {
        Accepted::CompactionPreviewed { .. } => "compaction_previewed",
        Accepted::CompactionCommitted { .. } => "compaction_committed",
        Accepted::CompactionCancelled => "compaction_cancelled",
        Accepted::TurnStarted { .. } => "turn_started",
        Accepted::Cancelling { .. } => "cancelling",
        Accepted::Resolved { .. } => "resolved",
        Accepted::ApprovalSubmitted { .. } => "approval_submitted",
        Accepted::HeadSelected { .. } => "head_selected",
        Accepted::ModelSwitched { .. } => "model_switched",
        Accepted::ContextControlSet { .. } => "context_control_set",
        Accepted::CompactionStrategySet { .. } => "compaction_strategy_set",
    }
}

/// A stable code for a refusal. Never includes plan bytes or journal content.
pub fn rejection_code(rejection: &Rejection) -> String {
    match rejection {
        Rejection::StalePreview { field } => format!("stale_preview:{field}"),
        Rejection::Compaction(error) => error.code().to_owned(),
        Rejection::ContextBudgetExceeded { reason } => match reason {
            Some(reason) => format!("context_budget_exceeded:{}", reason_code(*reason)),
            None => "context_budget_exceeded".into(),
        },
        Rejection::Overloaded => "overloaded".into(),
        Rejection::Busy => "busy".into(),
        Rejection::RecoveryRequired => "recovery_required".into(),
        Rejection::ResolutionRequired => "resolution_required".into(),
        Rejection::Faulted => "faulted".into(),
        Rejection::ShuttingDown => "shutting_down".into(),
        Rejection::NotActive => "not_active".into(),
        Rejection::StaleCommand => "stale_command".into(),
        Rejection::Invalid(code) => (*code).to_owned(),
        Rejection::PortabilityLoss(_) => "portability_loss".into(),
    }
}

fn reason_code(reason: harness_core::context::compaction::NotEligible) -> String {
    use harness_core::context::compaction::NotEligible;
    match reason {
        NotEligible::Empty => "empty",
        NotEligible::OnlyRecentTurns => "only_recent_turns",
        NotEligible::UnresolvedWork => "unresolved_work",
        NotEligible::NothingNewSinceSummary => "nothing_new_since_summary",
        NotEligible::NonCompletedTurn => "non_completed_turn",
        NotEligible::OnlyControlledTurns => "only_controlled_turns",
        NotEligible::UnknownHead => "unknown_head",
        NotEligible::InvalidConfig => "invalid_compaction",
    }
    .to_owned()
}

fn prepare_scratch(
    task: &Task,
    scratch: &Path,
    workspace: &Path,
    state: &Path,
) -> std::io::Result<()> {
    match fs::remove_dir_all(scratch) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    copy_workspace(task.workspace(), workspace).map_err(std::io::Error::other)?;
    // The data root is created by `DataRoot::open(create = true)`, which needs a
    // 0700 private directory and never creates missing ancestors. The scratch
    // directory exists by now, so let the store make the root itself.
    let _ = state;
    Ok(())
}

fn millis(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_call_with_narration_uses_distinct_block_indices() {
        let script = script_for(&Step::ToolCall {
            tool: "read".into(),
            arguments: serde_json::json!({"path": "a"}),
            say: "reading".into(),
        });
        let debug = format!("{script:?}");
        assert!(debug.contains("block_index: 0"), "{debug}");
        assert!(debug.contains("block_index: 1"), "{debug}");
    }

    #[test]
    fn a_reply_script_is_one_text_block_then_stop() {
        let script = format!("{:?}", reply_script("done"));
        assert!(script.contains("finish_reason: Some(Stop)"), "{script}");
        assert!(script.contains("done"), "{script}");
    }

    #[test]
    fn scoring_requires_every_term_after_normalization() {
        use crate::matcher::normalize;
        let text = "Keep  the\nPUBLIC name of report_row.";
        assert!(normalize(text).contains("keep the public name"));
    }
}
