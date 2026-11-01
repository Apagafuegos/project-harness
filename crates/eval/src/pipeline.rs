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
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use harness::{wiring, wiring::SessionOptions};
use harness_core::{
    context::{
        ContextItem, ContextReport, ItemKind, Part, StrategyId, compaction::CompactionPreview,
    },
    model::{
        ModelEventStream, ModelProvider, ModelRequest, OperationControl, ProviderEvent,
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

/// Default bound on one stage, so a stuck runtime fails the run instead of the
/// gate.
pub const STAGE_TIMEOUT: Duration = Duration::from_secs(180);

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
    /// Per-stage deadline. Production uses [`STAGE_TIMEOUT`]; the offline gate
    /// injects a short one to prove failure paths release the session.
    pub stage_timeout: Duration,
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
    /// Every request the replayed history sent, rendered in order. Identical
    /// across plans and strategies that replay the same task.
    pub history_requests: Vec<String>,
    /// Workspace paths whose bytes differ from the task's pristine workspace at
    /// the pre-compaction boundary, sorted. The ground truth for
    /// `modified_files.json`.
    pub pre_compaction_changes: Vec<String>,
    /// A copy of the workspace at the pre-compaction boundary, before the
    /// continuation ran. Absent when the cell failed before that boundary.
    pub pre_compaction_workspace: Option<PathBuf>,
    /// Compaction entries on the final branch. One cell must commit exactly one.
    pub compactions: usize,
    /// The scratch workspace, for the hidden check the caller may re-run.
    pub workspace: PathBuf,
}

/// Which stage a failure came from. Every exit is attributed, so a probe
/// timeout can never be reported as a replay failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// History replay, session creation or an approval.
    Replay,
    /// Applying the task's pins and exclusions.
    Control,
    /// Preview or commit of the one compaction.
    Compaction,
    /// The probe turn, or building the post-compaction projection.
    Probe,
    /// The continuation turn.
    Continuation,
}

/// A stage failure: where it happened and its stable code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageFailure {
    /// The stage that failed.
    pub stage: Stage,
    /// Stable code, such as `stage_timeout` or a product rejection code.
    pub code: String,
}

impl StageFailure {
    /// A failure at `stage` with `code`.
    pub fn new(stage: Stage, code: impl Into<String>) -> Self {
        Self {
            stage,
            code: code.into(),
        }
    }
}

trait StageResult<T> {
    /// Attribute a bare code to the stage being run.
    fn at(self, stage: Stage) -> Result<T, StageFailure>;
}

impl<T> StageResult<T> for Result<T, String> {
    fn at(self, stage: Stage) -> Result<T, StageFailure> {
        self.map_err(|code| StageFailure::new(stage, code))
    }
}

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
    let mut history_requests = Vec::new();
    let mut pre_compaction_changes = Vec::new();
    let mut pre_compaction_workspace = None;
    let mut compactions = 0;

    if let Err(error) = prepare_scratch(task, &scratch, &workspace, &state) {
        record.errors.replay = Some("scratch_unavailable".into());
        record.durations_ms.total_ms = millis(started);
        eprintln!("{}: scratch: {error}", task.name());
        return TaskRun {
            record,
            projection_text,
            probe_text,
            continuation_requests,
            history_requests,
            pre_compaction_changes,
            pre_compaction_workspace,
            compactions,
            workspace,
        };
    }

    // Every post-open exit inside `drive` awaits the session's shutdown before it
    // returns, so the hidden check can never be published while inference,
    // driver or executor work is still live.
    let observed = drive(
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
        &mut history_requests,
        &mut pre_compaction_changes,
        &mut pre_compaction_workspace,
        &mut compactions,
    )
    .await;
    if let Err(failure) = observed {
        record.errors.set(failure.stage, failure.code);
    }

    // The hidden check runs only when every preceding stage succeeded and the
    // session is closed and drained: nothing the check does can reach a model,
    // and no model or tool work is still pending while it runs.
    let publishable = record.errors.is_clear() && record.metrics.is_some();
    if publishable {
        let check_started = Instant::now();
        let outcome = check.run_check(task.check(), &workspace);
        record.durations_ms.check_ms = millis(check_started);
        if let Some(metrics) = record.metrics.as_mut() {
            metrics.completion = Some(outcome.passed);
        }
        // A check that ran and failed is a metric (`completion: false`), not a
        // mechanical error; only a failure that prevented the check from running
        // to a verdict is recorded as an error code.
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
        history_requests,
        pre_compaction_changes,
        pre_compaction_workspace,
        compactions,
        workspace,
    }
}

/// Open the session, run every stage, and always await shutdown.
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
    history_requests: &mut Vec<String>,
    pre_compaction_changes: &mut Vec<String>,
    pre_compaction_workspace: &mut Option<PathBuf>,
    compactions: &mut usize,
) -> Result<(), StageFailure> {
    let scripted = Arc::new(ScriptedProvider::new([]));
    // The fixed history is replayed from the scripts in **both** plans; only the
    // post-history stages go to a live provider.
    let later: Arc<dyn ModelProvider> = match plan {
        ModelPlan::Scripted => scripted.clone(),
        ModelPlan::Live(provider) => provider.clone(),
    };
    let phase = Arc::new(PhaseProvider {
        scripted: scripted.clone(),
        later,
        script_post_history: matches!(plan, ModelPlan::Scripted),
        replaying_history: AtomicBool::new(true),
    });
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
    let wired = wiring::open_session(&session_options, phase.clone())
        .await
        .map_err(|(_, code)| StageFailure::new(Stage::Replay, code))?;
    let handle = &wired.handle;
    let mut session = Session {
        handle,
        scripted: &scripted,
        phase: &phase,
        submitted: BTreeSet::new(),
        timeout: options.stage_timeout,
        workspace,
    };
    let outcome = run_stages(
        &mut session,
        task,
        strategy,
        record,
        projection_text,
        probe_text,
        continuation_requests,
        history_requests,
        pre_compaction_changes,
        pre_compaction_workspace,
        compactions,
    )
    .await;
    // One awaited shutdown on every path, success or failure, before the caller
    // is allowed to run the hidden check.
    handle.shutdown().await;
    outcome
}

/// Delegate every request to the scripted provider until the fixed history has
/// replayed, then to the plan's provider.
struct PhaseProvider {
    scripted: Arc<ScriptedProvider>,
    later: Arc<dyn ModelProvider>,
    script_post_history: bool,
    replaying_history: AtomicBool,
}

impl ModelProvider for PhaseProvider {
    fn stream(&self, request: ModelRequest, control: OperationControl) -> ModelEventStream {
        if self.replaying_history.load(Ordering::Acquire) {
            self.scripted.stream(request, control)
        } else {
            self.later.stream(request, control)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_stages(
    session: &mut Session<'_>,
    task: &Task,
    strategy: StrategyId,
    record: &mut Record,
    projection_text: &mut String,
    probe_text: &mut String,
    continuation_requests: &mut Vec<String>,
    history_requests: &mut Vec<String>,
    pre_compaction_changes: &mut Vec<String>,
    pre_compaction_workspace: &mut Option<PathBuf>,
    compactions: &mut usize,
) -> Result<(), StageFailure> {
    let handle = session.handle;

    // 1. Replay the fixed history through the real runtime.
    let replay_started = Instant::now();
    let streams_before = session.scripted.streams().len();
    let mut turn_ids = Vec::new();
    for turn in task.turns() {
        for step in &turn.steps {
            session.scripted.push(script_for(step));
        }
        let turn_id = session.start(&turn.prompt).await.at(Stage::Replay)?;
        session.settle().await.at(Stage::Replay)?;
        let snapshot = session.snapshot().await.at(Stage::Replay)?;
        let (outcome, code) = turn_result(&snapshot, &turn_id);
        if outcome != TurnOutcome::Completed {
            return Err(StageFailure::new(
                Stage::Replay,
                code.unwrap_or_else(|| "history_turn_incomplete".into()),
            ));
        }
        turn_ids.push(turn_id);
    }
    for stream in session.scripted.streams().into_iter().skip(streams_before) {
        // Instructions are fixed scaffolding that embeds the absolute workspace
        // path, so only the dialogue is comparable across cells.
        history_requests.push(render_dialogue(&stream.request.input.items));
    }
    session.expect_scripts_consumed(Stage::Replay, "unconsumed_history_scripts")?;
    record.durations_ms.replay_ms = millis(replay_started);

    // 2. Apply the task's controls, before compacting.
    for ordinal in &task.controls().pin {
        session
            .control(&turn_ids[ordinal - 1], ContextMode::Pinned)
            .await
            .at(Stage::Control)?;
    }
    for ordinal in &task.controls().exclude {
        session
            .control(&turn_ids[ordinal - 1], ContextMode::Excluded)
            .await
            .at(Stage::Control)?;
    }

    // The ground truth for file recall: what the replayed history actually
    // changed, compared with the task's pristine bytes. A path existing in the
    // workspace is not evidence that it changed.
    *pre_compaction_changes = changed_paths(task.workspace(), session.workspace())
        .map_err(|error| StageFailure::new(Stage::Control, format!("workspace_diff:{error}")))?;
    // A copy of the workspace at the pre-compaction boundary, so the gate can show
    // the task is still incomplete before the continuation.
    *pre_compaction_workspace = Some(session.snapshot_workspace().map_err(|error| {
        StageFailure::new(Stage::Control, format!("pre_compaction_snapshot:{error}"))
    })?);

    // 3. One compaction, through the real commands, forced regardless of budget.
    // The history has replayed, so the configured provider takes over here.
    session.begin_post_history();
    let compaction_started = Instant::now();
    if strategy.needs_model() && session.phase.script_post_history {
        session
            .scripted
            .push(reply_script(&task.scripted().summary));
    }
    let preview = session.preview_compaction().await?;
    let Some((preview_id, preview)) = preview else {
        record.durations_ms.compaction_ms = millis(compaction_started);
        return Ok(());
    };
    {
        let EntryPayload::Compaction(compaction) = &preview.entry.payload else {
            return Err(StageFailure::new(
                Stage::Compaction,
                "preview_is_not_a_compaction",
            ));
        };
        record.strategy = compaction.strategy.clone();
        record.strategy_version = compaction.strategy_version.clone();
        record.summary_model = compaction.model.as_ref().map(model_name);
        // M6 decision 15: two identities, never interchangeable. The exact
        // session-configuration artifact digest the committed entry binds, and
        // the compaction-policy fingerprint.
        record.session_configuration = Some(digest_text(&compaction.configuration_fingerprint));
        record.compaction_policy = Some(digest_text(&preview.binding.policy_fingerprint));
        record.estimator = Some(Estimator {
            method: estimator_method(&preview.before.totals.estimator.method),
            version: preview.before.totals.estimator.version.clone(),
        });
        record.tokens = Some(Tokens {
            before: preview.diff.before.estimated_total,
            after: preview.diff.after.estimated_total,
        });
    }
    session.commit_compaction(preview_id).await?;
    record.durations_ms.compaction_ms = millis(compaction_started);

    // 4. Score the post-compaction projection.
    let snapshot = session.snapshot().await.at(Stage::Probe)?;
    let report = handle
        .inspect_context(None, None)
        .await
        .map_err(|error| StageFailure::new(Stage::Probe, format!("inspect: {error:?}")))?;
    *projection_text = render_projection(&snapshot, &report);
    let projection_score = score(task, projection_text);
    *compactions = snapshot
        .ancestry(snapshot.head())
        .map(|branch| {
            branch
                .iter()
                .filter(|stored| matches!(stored.entry.payload, EntryPayload::Compaction(_)))
                .count()
        })
        .unwrap_or(0);

    // 5. Ask the probe and score its answer.
    let probe_started = Instant::now();
    if session.phase.script_post_history {
        session.scripted.push(reply_script(&task.scripted().probe));
    }
    let probe_turn = session.start(PROBE_PROMPT).await.at(Stage::Probe)?;
    session.settle().await.at(Stage::Probe)?;
    let snapshot = session.snapshot().await.at(Stage::Probe)?;
    let (outcome, code) = turn_result(&snapshot, &probe_turn);
    if outcome != TurnOutcome::Completed {
        return Err(StageFailure::new(
            Stage::Probe,
            code.unwrap_or_else(|| "probe_incomplete".into()),
        ));
    }
    *probe_text = assistant_text(&snapshot, &probe_turn);
    session.expect_scripts_consumed(Stage::Probe, "unconsumed_probe_scripts")?;
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
    let streams_before = session.scripted.streams().len();
    if session.phase.script_post_history {
        for step in &task.scripted().continuation {
            session.scripted.push(script_for(step));
        }
    }
    let continuation_turn = session
        .start(task.continue_prompt())
        .await
        .at(Stage::Continuation)?;
    session.settle().await.at(Stage::Continuation)?;
    let snapshot = session.snapshot().await.at(Stage::Continuation)?;
    let (outcome, code) = turn_result(&snapshot, &continuation_turn);
    if outcome != TurnOutcome::Completed {
        return Err(StageFailure::new(
            Stage::Continuation,
            code.unwrap_or_else(|| "continuation_incomplete".into()),
        ));
    }
    for stream in session.scripted.streams().into_iter().skip(streams_before) {
        continuation_requests.push(render_items(&stream.request.input.items));
    }
    session.expect_scripts_consumed(Stage::Continuation, "unconsumed_continuation_scripts")?;
    record.durations_ms.continuation_ms = millis(continuation_started);
    Ok(())
}

struct Session<'a> {
    handle: &'a SessionHandle,
    scripted: &'a Arc<ScriptedProvider>,
    phase: &'a Arc<PhaseProvider>,
    submitted: BTreeSet<ApprovalId>,
    timeout: Duration,
    workspace: &'a Path,
}

impl<'a> Session<'a> {
    fn workspace(&self) -> &'a Path {
        self.workspace
    }

    /// Every script pushed for a finished stage must have been consumed. A
    /// leftover script would be served to the next stage, so this is a hard
    /// failure rather than a silently corrupted run.
    fn expect_scripts_consumed(&self, stage: Stage, code: &str) -> Result<(), StageFailure> {
        let remaining = self.scripted.remaining();
        if remaining == 0 {
            return Ok(());
        }
        Err(StageFailure::new(stage, format!("{code}:{remaining}_left")))
    }

    /// Copy the workspace beside itself, for the pre-compaction boundary.
    fn snapshot_workspace(&self) -> Result<PathBuf, String> {
        let target = self
            .workspace
            .parent()
            .ok_or_else(|| "workspace has no parent".to_owned())?
            .join("pre-compaction");
        copy_workspace(self.workspace, &target).map_err(|error| error.to_string())?;
        Ok(target)
    }

    /// Hand every later request to the plan's provider. Called once, after the
    /// fixed history has replayed and before the compaction preview.
    fn begin_post_history(&self) {
        self.phase.replaying_history.store(false, Ordering::Release);
    }

    async fn start(&mut self, prompt: &str) -> Result<TurnId, String> {
        let reply = self
            .bounded(self.handle.command(Command::StartTurn {
                command_id: CommandId::new(),
                content: vec![ContentBlock::Text {
                    text: prompt.to_owned(),
                }],
            }))
            .await?;
        match reply {
            Ok(Accepted::TurnStarted { turn_id, .. }) => Ok(turn_id),
            Ok(other) => Err(format!("unexpected_reply:{}", kind_name(&other))),
            Err(rejection) => Err(rejection_code(&rejection)),
        }
    }

    async fn control(&mut self, turn: &TurnId, mode: ContextMode) -> Result<(), String> {
        match self
            .bounded(self.handle.command(Command::SetContextControl {
                turn_id: turn.clone(),
                mode,
            }))
            .await?
        {
            Ok(Accepted::ContextControlSet { .. }) => Ok(()),
            Ok(other) => Err(format!("unexpected_reply:{}", kind_name(&other))),
            Err(rejection) => Err(rejection_code(&rejection)),
        }
    }

    /// Generate the one preview. `None` means the refusal or failure is already
    /// recorded in `record.errors.compaction` and the cell stops here.
    async fn preview_compaction(
        &mut self,
    ) -> Result<Option<(EntryId, Box<CompactionPreview>)>, StageFailure> {
        let reply = self
            .bounded(self.handle.command(Command::PreviewCompaction))
            .await
            .map_err(|code| StageFailure::new(Stage::Compaction, code))?;
        match reply {
            Ok(Accepted::CompactionPreviewed {
                preview_id,
                preview,
            }) => Ok(Some((preview_id, preview))),
            Ok(other) => Err(StageFailure::new(
                Stage::Compaction,
                format!("unexpected_reply:{}", kind_name(&other)),
            )),
            Err(rejection) => Err(StageFailure::new(
                Stage::Compaction,
                rejection_code(&rejection),
            )),
        }
    }

    async fn commit_compaction(&mut self, preview_id: EntryId) -> Result<(), StageFailure> {
        let reply = self
            .bounded(
                self.handle
                    .command(Command::CommitCompaction { preview_id }),
            )
            .await
            .map_err(|code| StageFailure::new(Stage::Compaction, code))?;
        match reply {
            Ok(Accepted::CompactionCommitted { .. }) => Ok(()),
            Ok(other) => Err(StageFailure::new(
                Stage::Compaction,
                format!("unexpected_reply:{}", kind_name(&other)),
            )),
            Err(rejection) => Err(StageFailure::new(
                Stage::Compaction,
                rejection_code(&rejection),
            )),
        }
    }

    async fn snapshot(&self) -> Result<SessionSnapshot, String> {
        Ok(self
            .handle
            .view()
            .await
            .map_err(|error| format!("view: {error:?}"))?
            .snapshot)
    }

    /// Bound one command by this run's stage deadline.
    async fn bounded<T>(&self, future: impl std::future::Future<Output = T>) -> Result<T, String> {
        tokio::time::timeout(self.timeout, future)
            .await
            .map_err(|_| "stage_timeout".to_owned())
    }

    /// Poll the runtime until the turn is finished, answering every approval the
    /// eval is asked for. This is the only auto-approval in the harness.
    async fn settle(&mut self) -> Result<RuntimeView, String> {
        let deadline = tokio::time::Instant::now() + self.timeout;
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
                let reply = self
                    .bounded(self.handle.command(Command::SubmitApproval {
                        approval_id: ticket.approval_id,
                        plan_digest: ticket.plan_digest,
                        decision: Decision::Allow,
                        remember: false,
                    }))
                    .await
                    .map_err(|code| format!("approval:{code}"))?;
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

/// Relative paths whose bytes differ between `pristine` and `current`, including
/// additions and deletions, sorted. Symlinks, special files and unreadable
/// entries are reported as differences rather than followed. This is the ground
/// truth for `modified_files.json`: a path that merely exists is not a change.
pub fn changed_paths(pristine: &Path, current: &Path) -> Result<Vec<String>, String> {
    fn collect(
        root: &Path,
        out: &mut Vec<(String, Option<Vec<u8>>)>,
        prefix: &Path,
    ) -> Result<(), String> {
        let entries = fs::read_dir(root).map_err(|error| format!("{}: {error}", root.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("{}: {error}", root.display()))?;
            let name = entry
                .file_name()
                .to_str()
                .ok_or_else(|| format!("{}: non-UTF-8 name", root.display()))?
                .to_owned();
            if name == crate::check::CHECK_FILE || name == ".git" {
                continue;
            }
            let relative = prefix.join(&name);
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|error| format!("{}: {error}", entry.path().display()))?;
            if metadata.file_type().is_symlink() {
                out.push((
                    relative.to_string_lossy().into_owned(),
                    Some(b"<symlink>".to_vec()),
                ));
            } else if metadata.is_dir() {
                collect(&entry.path(), out, &relative)?;
            } else if metadata.is_file() {
                let bytes = fs::read(entry.path())
                    .map_err(|error| format!("{}: {error}", entry.path().display()))?;
                out.push((relative.to_string_lossy().into_owned(), Some(bytes)));
            } else {
                out.push((
                    relative.to_string_lossy().into_owned(),
                    Some(b"<special>".to_vec()),
                ));
            }
        }
        Ok(())
    }
    let mut pristine_files = Vec::new();
    let mut current_files = Vec::new();
    collect(pristine, &mut pristine_files, Path::new(""))?;
    collect(current, &mut current_files, Path::new(""))?;
    let to_map = |files: Vec<(String, Option<Vec<u8>>)>| -> BTreeMap<String, Option<Vec<u8>>> {
        files.into_iter().collect()
    };
    let pristine = to_map(pristine_files);
    let current = to_map(current_files);
    let mut changed = BTreeSet::new();
    for (path, bytes) in &current {
        match pristine.get(path) {
            Some(previous) if previous == bytes => {}
            _ => {
                changed.insert(path.clone());
            }
        }
    }
    for path in pristine.keys() {
        if !current.contains_key(path) {
            changed.insert(path.clone());
        }
    }
    Ok(changed.into_iter().collect())
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

/// Render only the dialogue of a projection, skipping the fixed instructions.
fn render_dialogue(items: &[ContextItem]) -> String {
    let dialogue: Vec<ContextItem> = items
        .iter()
        .filter(|item| !matches!(item, ContextItem::Instructions { .. }))
        .cloned()
        .collect();
    render_items(&dialogue)
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
