//! Result records, one per task × strategy × repetition, and their aggregate.
//!
//! Records are plain JSON with stable field order. Every metric is a count, so
//! two runs of the same suite produce byte-identical aggregate files: durations
//! and the exact session-configuration digest live only in the per-record files
//! and are never aggregated (M6 decision 15). See ADR 0027.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use serde::{Deserialize, Serialize};

/// Record schema version. Version 2 splits configuration provenance into the
/// exact session-configuration artifact digest and the compaction-policy
/// fingerprint (M6 decision 15).
pub const RECORD_SCHEMA: u32 = 2;

/// A provider-qualified model name, as written in a record.
pub type ModelName = String;

fn model_name(model: &harness_core::session::ModelRef) -> ModelName {
    format!("{}/{}", model.provider_instance, model.model)
}

/// Token estimates before and after the one compaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    /// Estimated total of the projection the summary replaced.
    pub before: u64,
    /// Estimated total of the projection that replaced it.
    pub after: u64,
}

/// What one run measured. Counts only: no floats, no identities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metrics {
    /// Requirements the post-compaction projection still states.
    pub requirements_retained_projection: u64,
    /// Requirements the probe answer states.
    pub requirements_retained_probe: u64,
    /// Requirements a task states.
    pub requirements_total: u64,
    /// Modified files the post-compaction projection still names.
    pub files_recalled_projection: u64,
    /// Modified files the probe answer names.
    pub files_recalled_probe: u64,
    /// Modified files a task declares.
    pub files_total: u64,
    /// Whether the hidden check passed after the continuation. Absent when no
    /// check ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<bool>,
}

/// Wall-clock durations, excluded from every aggregate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Durations {
    /// The whole run.
    pub total_ms: u64,
    /// Replaying the history through the real runtime.
    pub replay_ms: u64,
    /// One preview plus its commit.
    pub compaction_ms: u64,
    /// The probe turn.
    pub probe_ms: u64,
    /// The continuation turn.
    pub continuation_ms: u64,
    /// The hidden check.
    pub check_ms: u64,
}

/// Stable error codes, one slot per stage. `None` means the stage succeeded.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Errors {
    /// A history turn, approval or session failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<String>,
    /// Applying the task's pins and exclusions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<String>,
    /// Preview or commit refusal, or the strategy's coded failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<String>,
    /// The probe turn's outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<String>,
    /// The continuation turn's outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<String>,
    /// The hidden check's executor status or code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
}

impl Errors {
    /// No stage failed.
    pub fn is_clear(&self) -> bool {
        self.replay.is_none()
            && self.control.is_none()
            && self.compaction.is_none()
            && self.probe.is_none()
            && self.continuation.is_none()
            && self.check.is_none()
    }

    /// Record a stage failure in its own slot. The stage decides the slot, so a
    /// probe timeout can never be reported as a replay failure.
    pub fn set(&mut self, stage: crate::pipeline::Stage, code: String) {
        use crate::pipeline::Stage;
        match stage {
            Stage::Replay => self.replay = Some(code),
            Stage::Control => self.control = Some(code),
            Stage::Compaction => self.compaction = Some(code),
            Stage::Probe => self.probe = Some(code),
            Stage::Continuation => self.continuation = Some(code),
        }
    }
}

/// One task × strategy × repetition result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Schema version of this file.
    pub schema: u32,
    /// `git rev-parse HEAD` as supplied by the caller.
    pub harness_commit: String,
    /// Digest of the whole task directory.
    pub task_fingerprint: String,
    /// Task name.
    pub task: String,
    /// Task shape.
    pub shape: String,
    /// Strategy name, as recorded in the committed compaction entry.
    pub strategy: String,
    /// Strategy version, as recorded in the committed compaction entry.
    pub strategy_version: String,
    /// Session model.
    pub session_model: ModelName,
    /// Summary model, absent for a model-free strategy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_model: Option<ModelName>,
    /// Exact digest of the session configuration artifact the committed
    /// compaction entry binds (instructions, execution policy, compaction
    /// policy). Per-record only: it includes the ephemeral workspace path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_configuration: Option<String>,
    /// Compaction-policy fingerprint: strategy name and version plus the
    /// compaction configuration. Independent of the workspace path, so an
    /// aggregate can group by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_policy: Option<String>,
    /// Token estimator method and version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimator: Option<Estimator>,
    /// Repetition index, starting at zero.
    pub repetition: usize,
    /// Deterministic seed; equal to the repetition index.
    pub seed: u64,
    /// Estimates before and after compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    /// Scores, absent when compaction failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Metrics>,
    /// Per-stage wall-clock durations.
    pub durations_ms: Durations,
    /// Per-stage error codes.
    pub errors: Errors,
}

/// Estimator identity in a record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Estimator {
    /// Estimate method, such as `heuristic`.
    pub method: String,
    /// Arithmetic version.
    pub version: String,
}

impl Record {
    /// Build a record skeleton for one cell of the matrix.
    pub fn skeleton(
        task: &crate::task::Task,
        strategy: harness_core::context::StrategyId,
        session_model: &harness_core::session::ModelRef,
        summary_model: Option<&harness_core::session::ModelRef>,
        repetition: usize,
        harness_commit: &str,
    ) -> Self {
        Self {
            schema: RECORD_SCHEMA,
            harness_commit: harness_commit.to_owned(),
            task_fingerprint: digest_text(task.fingerprint()),
            task: task.name().to_owned(),
            shape: task.shape().as_str().to_owned(),
            strategy: strategy.as_str().to_owned(),
            strategy_version: strategy.version().to_owned(),
            session_model: model_name(session_model),
            summary_model: summary_model.map(model_name),
            session_configuration: None,
            compaction_policy: None,
            estimator: None,
            repetition,
            seed: repetition as u64,
            tokens: None,
            metrics: None,
            durations_ms: Durations::default(),
            errors: Errors::default(),
        }
    }

    /// Whether every stage succeeded and a check ran.
    pub fn complete(&self) -> bool {
        self.errors.replay.is_none()
            && self.errors.control.is_none()
            && self.errors.compaction.is_none()
            && self.errors.probe.is_none()
            && self.errors.continuation.is_none()
            && self.errors.check.is_none()
            && self.metrics.is_some()
    }

    /// Canonical JSON bytes of this record.
    pub fn to_json(&self) -> Result<String, String> {
        let mut text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        text.push('\n');
        Ok(text)
    }
}

fn digest_text(digest: &harness_core::session::Digest) -> String {
    serde_json::to_value(digest)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Summed counts for one strategy, across every task and repetition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyTotals {
    /// Strategy name.
    pub strategy: String,
    /// Strategy version.
    pub strategy_version: String,
    /// Runs counted.
    pub runs: u64,
    /// Requirements retained in the post-compaction projection.
    pub requirements_retained_projection: u64,
    /// Requirements stated, summed over runs.
    pub requirements_total: u64,
    /// Requirements retained in the probe answer.
    pub requirements_retained_probe: u64,
    /// Modified files recalled from the post-compaction projection.
    pub files_recalled_projection: u64,
    /// Modified files declared, summed over runs.
    pub files_total: u64,
    /// Modified files recalled from the probe answer.
    pub files_recalled_probe: u64,
    /// Runs whose hidden check passed.
    pub completions: u64,
    /// Runs that reached the check.
    pub checkable: u64,
    /// Estimated tokens before compaction, summed.
    pub tokens_before: u64,
    /// Estimated tokens after compaction, summed.
    pub tokens_after: u64,
    /// Mechanical failures (a non-`None` error code in any stage).
    pub failures: u64,
}

/// Per-task detail, so a reader can see which tasks differ.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTotals {
    /// Task name.
    pub task: String,
    /// Task fingerprint.
    pub task_fingerprint: String,
    /// Task shape.
    pub shape: String,
    /// One entry per strategy, in the suite's strategy order.
    pub strategies: Vec<StrategyTotals>,
}

/// The whole run: byte-identical for identical suites and strategies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aggregate {
    /// Schema version of this file.
    pub schema: u32,
    /// `git rev-parse HEAD` as supplied by the caller.
    pub harness_commit: String,
    /// Records aggregated.
    pub runs: u64,
    /// One entry per strategy, in strategy order.
    pub strategies: Vec<StrategyTotals>,
    /// One entry per task, in task order.
    pub tasks: Vec<TaskTotals>,
    /// Compaction-policy fingerprints seen, per strategy. The exact
    /// session-configuration digest is deliberately not aggregated: it varies
    /// with the ephemeral scratch path (M6 decision 15).
    pub compaction_policies: BTreeMap<String, BTreeSet<String>>,
    /// Estimator identities seen.
    pub estimators: BTreeSet<(String, String)>,
}

impl Aggregate {
    /// Sum records into an aggregate. Ordering follows the supplied record
    /// order, so callers pass tasks and strategies in a fixed order.
    pub fn new(records: &[Record], harness_commit: &str) -> Self {
        let mut strategies: Vec<StrategyTotals> = Vec::new();
        let mut tasks: Vec<TaskTotals> = Vec::new();
        let mut compaction_policies: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut estimators = BTreeSet::new();
        for record in records {
            if let Some(fingerprint) = &record.compaction_policy {
                compaction_policies
                    .entry(record.strategy.clone())
                    .or_default()
                    .insert(fingerprint.clone());
            }
            if let Some(estimator) = &record.estimator {
                estimators.insert((estimator.method.clone(), estimator.version.clone()));
            }
            accumulate(strategy_entry(&mut strategies, record), record);
            let task = match tasks.iter_mut().find(|task| task.task == record.task) {
                Some(task) => task,
                None => {
                    tasks.push(TaskTotals {
                        task: record.task.clone(),
                        task_fingerprint: record.task_fingerprint.clone(),
                        shape: record.shape.clone(),
                        strategies: Vec::new(),
                    });
                    tasks.last_mut().expect("just pushed")
                }
            };
            accumulate(strategy_entry(&mut task.strategies, record), record);
        }
        Self {
            schema: RECORD_SCHEMA,
            harness_commit: harness_commit.to_owned(),
            runs: records.len() as u64,
            strategies,
            tasks,
            compaction_policies,
            estimators,
        }
    }

    /// Canonical JSON bytes.
    pub fn to_json(&self) -> Result<String, String> {
        let mut text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        text.push('\n');
        Ok(text)
    }

    /// A Markdown table of means and counts per strategy.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Compaction evaluation aggregate\n\n");
        out.push_str(&format!("- harness commit: `{}`\n", self.harness_commit));
        out.push_str(&format!("- runs: {}\n", self.runs));
        out.push_str(&format!("- tasks: {}\n", self.tasks.len()));
        out.push_str(&format!("- strategies: {}\n", self.strategies.len()));
        let estimators: Vec<String> = self
            .estimators
            .iter()
            .map(|(method, version)| format!("{method} {version}"))
            .collect();
        out.push_str(&format!("- estimators: {}\n\n", estimators.join(", ")));
        out.push_str("## Per strategy\n\n");
        out.push_str(
            "| strategy | version | runs | requirement retention (projection) | \
             requirement retention (probe) | file recall (projection) | \
             file recall (probe) | completion | tokens before | tokens after | failures |\n",
        );
        out.push_str("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n");
        for strategy in &self.strategies {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                strategy.strategy,
                strategy.strategy_version,
                strategy.runs,
                ratio(
                    strategy.requirements_retained_projection,
                    strategy.requirements_total
                ),
                ratio(
                    strategy.requirements_retained_probe,
                    strategy.requirements_total
                ),
                ratio(strategy.files_recalled_projection, strategy.files_total),
                ratio(strategy.files_recalled_probe, strategy.files_total),
                ratio(strategy.completions, strategy.checkable),
                strategy.tokens_before,
                strategy.tokens_after,
                strategy.failures,
            ));
        }
        out.push_str("\n## Per task\n\n");
        out.push_str(
            "| task | shape | strategy | retention (projection) | retention (probe) | \
             recall (projection) | recall (probe) | completion |\n",
        );
        out.push_str("| --- | --- | --- | --- | --- | --- | --- | --- |\n");
        for task in &self.tasks {
            for strategy in &task.strategies {
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                    task.task,
                    task.shape,
                    strategy.strategy,
                    ratio(
                        strategy.requirements_retained_projection,
                        strategy.requirements_total
                    ),
                    ratio(
                        strategy.requirements_retained_probe,
                        strategy.requirements_total
                    ),
                    ratio(strategy.files_recalled_projection, strategy.files_total),
                    ratio(strategy.files_recalled_probe, strategy.files_total),
                    ratio(strategy.completions, strategy.checkable),
                ));
            }
        }
        out
    }
}

fn strategy_entry<'a>(
    entries: &'a mut Vec<StrategyTotals>,
    record: &Record,
) -> &'a mut StrategyTotals {
    if let Some(index) = entries
        .iter()
        .position(|entry| entry.strategy == record.strategy)
    {
        return &mut entries[index];
    }
    entries.push(StrategyTotals {
        strategy: record.strategy.clone(),
        strategy_version: record.strategy_version.clone(),
        runs: 0,
        requirements_retained_projection: 0,
        requirements_total: 0,
        requirements_retained_probe: 0,
        files_recalled_projection: 0,
        files_total: 0,
        files_recalled_probe: 0,
        completions: 0,
        checkable: 0,
        tokens_before: 0,
        tokens_after: 0,
        failures: 0,
    });
    entries.last_mut().expect("just pushed")
}

fn accumulate(entry: &mut StrategyTotals, record: &Record) {
    entry.runs += 1;
    if let Some(metrics) = &record.metrics {
        entry.requirements_retained_projection += metrics.requirements_retained_projection;
        entry.requirements_retained_probe += metrics.requirements_retained_probe;
        entry.requirements_total += metrics.requirements_total;
        entry.files_recalled_projection += metrics.files_recalled_projection;
        entry.files_recalled_probe += metrics.files_recalled_probe;
        entry.files_total += metrics.files_total;
        if let Some(completion) = metrics.completion {
            entry.checkable += 1;
            entry.completions += u64::from(completion);
        }
    }
    if let Some(tokens) = record.tokens {
        entry.tokens_before += tokens.before;
        entry.tokens_after += tokens.after;
    }
    if !record.complete() {
        entry.failures += 1;
    }
}

fn ratio(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "n/a".into();
    }
    format!("{part}/{whole} ({:.3})", part as f64 / whole as f64)
}

/// Write the run's records and both aggregate files into `run_dir`.
pub fn write_run(run_dir: &Path, records: &[Record]) -> Result<Aggregate, String> {
    let records_dir = run_dir.join("records");
    fs::create_dir_all(&records_dir)
        .map_err(|error| format!("{}: {error}", records_dir.display()))?;
    for record in records {
        let name = format!(
            "{}.{}.{}.json",
            record.task, record.strategy, record.repetition
        );
        let path = records_dir.join(name);
        fs::write(&path, record.to_json()?)
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    let commit = records
        .first()
        .map(|record| record.harness_commit.clone())
        .unwrap_or_default();
    let aggregate = Aggregate::new(records, &commit);
    write_text(&run_dir.join("aggregate.json"), &aggregate.to_json()?)?;
    write_text(&run_dir.join("aggregate.md"), &aggregate.to_markdown())?;
    Ok(aggregate)
}

fn write_text(path: &Path, text: &str) -> Result<(), String> {
    fs::write(path, text).map_err(|error| format!("{}: {error}", path.display()))
}

/// A fresh run directory name. Contains a timestamp so runs never collide; the
/// timestamp never reaches an aggregate file.
pub fn run_id() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    format!("run-{seconds}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::context::StrategyId;
    use harness_core::session::ModelRef;

    fn model() -> ModelRef {
        ModelRef {
            provider_instance: "mock".into(),
            model: "scripted".into(),
        }
    }

    fn record(
        task: &crate::task::Task,
        strategy: StrategyId,
        retained: u64,
        completion: bool,
    ) -> Record {
        let mut record = Record::skeleton(task, strategy, &model(), None, 0, "commit");
        // Two distinct identities, per M6 decision 15.
        record.session_configuration = Some(format!("{:0>64}", strategy.as_str()));
        record.compaction_policy = Some(format!("{:1>64}", strategy.as_str()));
        record.estimator = Some(Estimator {
            method: "heuristic".into(),
            version: "bytes4-flat-media-v1".into(),
        });
        record.tokens = Some(Tokens {
            before: 100,
            after: 40,
        });
        record.metrics = Some(Metrics {
            requirements_retained_projection: retained,
            requirements_retained_probe: retained,
            requirements_total: 2,
            files_recalled_projection: 1,
            files_recalled_probe: 1,
            files_total: 1,
            completion: Some(completion),
        });
        record
    }

    fn task() -> crate::task::Task {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("t");
        std::fs::create_dir_all(dir.join("workspace")).unwrap();
        std::fs::write(
            dir.join("task.json"),
            r#"{"name":"t","shape":"refactor","description":"d"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("history.jsonl"),
            concat!(
                r#"{"type":"user","text":"u"}"#,
                "\n",
                r#"{"type":"tool_call","tool":"read","arguments":{"path":"a"}}"#,
                "\n",
                r#"{"type":"assistant","text":"a"}"#,
                "\n",
                r#"{"type":"user","text":"u"}"#,
                "\n",
                r#"{"type":"tool_call","tool":"read","arguments":{"path":"a"}}"#,
                "\n",
                r#"{"type":"assistant","text":"a"}"#,
                "\n",
                r#"{"type":"user","text":"u"}"#,
                "\n",
                r#"{"type":"assistant","text":"a"}"#,
                "\n",
                r#"{"type":"user","text":"u"}"#,
                "\n",
                r#"{"type":"assistant","text":"a"}"#,
                "\n",
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("requirements.json"),
            r#"{"requirements":[{"id":"R1","text":"t","all_of":["a"]},{"id":"R2","text":"t","all_of":["b"]}]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("modified_files.json"), r#"{"paths":["a"]}"#).unwrap();
        std::fs::write(dir.join("continue.md"), "go").unwrap();
        std::fs::write(
            dir.join("check.sh"),
            format!("{} t\nexit 0\n", crate::task::CHECK_SENTINEL),
        )
        .unwrap();
        std::fs::write(dir.join("workspace/a"), "x").unwrap();
        std::fs::create_dir_all(dir.join("scripted")).unwrap();
        std::fs::write(
            dir.join("scripted/summary.md"),
            "## Task and constraints\ns",
        )
        .unwrap();
        std::fs::write(dir.join("scripted/probe.md"), "p").unwrap();
        std::fs::write(
            dir.join("scripted/continue.jsonl"),
            "{\"type\":\"assistant\",\"text\":\"a\"}\n",
        )
        .unwrap();
        crate::task::Task::load(dir).unwrap()
    }

    #[test]
    fn an_aggregate_counts_runs_and_is_order_stable() {
        let task = task();
        let records = vec![
            record(&task, StrategyId::RollingSummary, 2, true),
            record(&task, StrategyId::ObservationMasking, 1, false),
        ];
        let aggregate = Aggregate::new(&records, "commit");
        assert_eq!(aggregate.runs, 2);
        assert_eq!(aggregate.strategies.len(), 2);
        let rolling = &aggregate.strategies[0];
        assert_eq!(rolling.strategy, "rolling_summary");
        assert_eq!(rolling.requirements_retained_projection, 2);
        assert_eq!(rolling.completions, 1);
        assert_eq!(rolling.checkable, 1);
        assert_eq!(rolling.tokens_before, 100);
        assert_eq!(
            aggregate.to_json().unwrap(),
            Aggregate::new(&records, "commit").to_json().unwrap()
        );
        let markdown = aggregate.to_markdown();
        assert!(markdown.contains("| rolling_summary | 2 | 1 | 2/2 (1.000) |"));
        assert!(markdown.contains("| observation_masking |"));
    }

    #[test]
    fn a_failed_run_is_counted_but_scores_nothing() {
        let task = task();
        let mut failed =
            Record::skeleton(&task, StrategyId::RollingSummary, &model(), None, 0, "c");
        failed.errors.compaction = Some("empty_summary".into());
        let aggregate = Aggregate::new(&[failed], "c");
        assert_eq!(aggregate.strategies[0].failures, 1);
        assert_eq!(aggregate.strategies[0].requirements_total, 0);
        assert!(aggregate.to_markdown().contains("n/a"));
    }
}
