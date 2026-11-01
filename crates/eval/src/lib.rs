//! Dev-only evaluation harness for Milestone 6's compaction strategies.
//!
//! This crate is not a dependency of the `harness` binary and holds no product
//! types: `harness-core` gains nothing from it. See ADR 0027 for the task format,
//! the metric definitions, determinism, sandboxing and the auto-approval scope.
//!
//! The pipeline is real: the shared wiring creates each session, a task's
//! history is replayed through the runtime so tool calls really run, one
//! compaction is generated and committed through the product commands, the
//! post-compaction projection and a probe turn are scored with deterministic
//! matchers, the task is continued through the normal tool loop with the eval
//! answering approvals, and a hidden check runs in the existing Bubblewrap
//! sandbox. Only the model's post-history answers are scripted offline.

pub mod check;
pub mod matcher;
pub mod pipeline;
pub mod record;
pub mod task;

use std::{str::FromStr, sync::Arc};

use harness_core::{
    context::{ModelLimits, StrategyId},
    model::ModelProvider,
    runtime::DriverConfig,
    session::ModelRef,
};

pub use check::{CheckOutcome, HiddenCheck};
pub use matcher::{Matcher, Requirement, normalize};
pub use pipeline::{ModelPlan, RunOptions, TaskRun, run_task};
pub use record::{Aggregate, Record, write_run};
pub use task::{Controls, Scripted, Shape, Step, Suite, Task, TaskError, Turn};

/// A fresh scratch root whose absolute path has a fixed width.
///
/// A session's base instruction embeds the absolute workspace path, and the
/// product's token estimate counts its bytes. Keeping the scratch path the same
/// length on every run keeps the estimate out of the results path: two runs of
/// the same suite then produce byte-identical aggregates.
pub fn scratch_root() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default();
    std::env::temp_dir().join(format!("harness-eval-{nanos:016x}"))
}

/// The offline session model. `ScriptedProvider` replays the task's scripts, so
/// the name only has to be stable and unmistakably not a real model.
pub fn offline_model() -> ModelRef {
    ModelRef {
        provider_instance: "mock".into(),
        model: "scripted".into(),
    }
}

/// The offline summary model, distinct from the session model so a record shows
/// which credential each stage used.
pub fn offline_summary_model() -> ModelRef {
    ModelRef {
        provider_instance: "mock".into(),
        model: "summary".into(),
    }
}

/// Runtime defaults for an offline run: a known budget so estimates are
/// meaningful, and a bounded tool loop so a runaway continuation fails the cell
/// instead of the gate.
pub fn offline_driver() -> DriverConfig {
    let mut driver = DriverConfig::default();
    driver.runtime.max_inferences_per_turn = 8;
    for model in [offline_model(), offline_summary_model()] {
        driver.runtime.context.catalog.insert(
            model,
            ModelLimits {
                context_tokens: Some(128_000),
                max_output_tokens: Some(8_192),
            },
        );
    }
    driver
}

/// Everything one suite run produced.
pub struct SuiteRun {
    /// One record per task × strategy × repetition, in run order.
    pub records: Vec<Record>,
    /// The same cells with the texts they were scored from.
    pub runs: Vec<TaskRun>,
    /// Sums over every record.
    pub aggregate: Aggregate,
}

/// Run every selected task × strategy × repetition.
pub async fn run_suite(
    tasks: &[Task],
    strategies: &[StrategyId],
    options: &RunOptions,
    plan: &ModelPlan,
    check: &HiddenCheck,
) -> Result<SuiteRun, String> {
    if tasks.is_empty() || strategies.is_empty() {
        return Err("no tasks or strategies selected".into());
    }
    std::fs::create_dir_all(&options.scratch_root)
        .map_err(|error| format!("{}: {error}", options.scratch_root.display()))?;
    let mut records = Vec::new();
    let mut runs = Vec::new();
    for task in tasks {
        for strategy in strategies {
            for repetition in 0..options.repetitions {
                let run = run_task(task, *strategy, repetition, options, plan, check).await;
                records.push(run.record.clone());
                runs.push(run);
            }
        }
    }
    let aggregate = Aggregate::new(&records, &options.commit);
    Ok(SuiteRun {
        records,
        runs,
        aggregate,
    })
}

/// Build a live provider and defaults from the environment. `selected` names the
/// provider instance, such as `deepseek`; the adapter reads its own credential.
pub fn live_providers(selected: &str) -> Result<harness::wiring::Providers, String> {
    harness::wiring::providers(selected, false).map_err(|error| error.diagnostic.to_owned())
}

/// Resolve the model for a live run: `provider/model`, or the provider's default.
pub fn live_model(provider: &str, model: Option<&str>) -> ModelRef {
    ModelRef {
        provider_instance: provider.to_owned(),
        model: model
            .map(str::to_owned)
            .unwrap_or_else(|| harness::wiring::default_model(provider, false).to_owned()),
    }
}

/// A coarse token estimate for a live run, computed from a scripted dry run of
/// the same cells. The estimator is heuristic, so this is an order-of-magnitude
/// check meant to be read before `--yes`, never a bill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveEstimate {
    /// Cells that will run.
    pub cells: usize,
    /// Model requests the scripted dry run needed per cell, summed.
    pub requests: u64,
    /// Estimated prompt tokens, summed over cells.
    pub input_tokens: u64,
}

impl LiveEstimate {
    /// A human-readable line, printed before a live run starts.
    pub fn describe(&self) -> String {
        format!(
            "estimate: {} cell(s), {} model request(s), about {} prompt tokens \
             (heuristic; excludes tool-loop retries and output tokens)",
            self.cells, self.requests, self.input_tokens
        )
    }
}

/// Derive the estimate from the scripted runs of the same cells.
pub fn estimate_from(runs: &[TaskRun]) -> LiveEstimate {
    let mut requests = 0;
    let mut input_tokens = 0;
    for run in runs {
        let record = &run.record;
        let (before, after) = record
            .tokens
            .map(|tokens| (tokens.before, tokens.after))
            .unwrap_or((0, 0));
        let continuation_calls = run.continuation_requests.len() as u64;
        let summary_calls = u64::from(
            StrategyId::from_str(&record.strategy)
                .map(StrategyId::needs_model)
                .unwrap_or(true),
        );
        // History replay is scripted in live mode too, so it costs no requests:
        // one summary, one probe, then the continuation's own inferences.
        requests += summary_calls + 1 + continuation_calls;
        input_tokens += before * (1 + summary_calls) + after * (1 + continuation_calls);
    }
    LiveEstimate {
        cells: runs.len(),
        requests,
        input_tokens,
    }
}

/// Parse a strategy name, as accepted by `--strategies`.
pub fn parse_strategy(name: &str) -> Result<StrategyId, String> {
    name.parse::<StrategyId>()
        .map_err(|()| format!("unknown strategy {name:?}"))
}

/// A live plan over an already-constructed provider.
pub fn live_plan(provider: Arc<dyn ModelProvider>) -> ModelPlan {
    ModelPlan::Live(provider)
}
