//! The offline gate: the whole pipeline through the real runtime, storage, file
//! tools, compaction and the Bubblewrap sandbox.
//!
//! Ignored by default and run by `scripts/check.sh` where `bwrap` is available,
//! exactly like the product's S02 targets. It proves the mechanics, never model
//! quality: the post-history answers are scripts, and every metric assertion here
//! is about what the harness measures, not about which strategy is better.

use std::{
    fs,
    path::{Path, PathBuf},
};

use harness_core::context::StrategyId;
use harness_eval::{
    Aggregate, HiddenCheck, ModelPlan, RunOptions, Suite, Task, TaskRun, check, offline_driver,
    offline_model, offline_summary_model, record::Estimator, run_suite,
};

fn suite_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/tasks")
}

/// A scratch root whose absolute path has a fixed width inside the test's temp
/// directory. A session's base instruction embeds the absolute workspace path and
/// the product's estimate counts its bytes, so runs that differ only in their
/// slot number must still agree to the byte.
fn options(root: &Path, slot: u64) -> RunOptions {
    RunOptions {
        scratch_root: root.join(format!("scratch-{slot:08}")),
        commit: "gate".into(),
        repetitions: 1,
        model: offline_model(),
        summary_model: Some(offline_summary_model()),
        driver: offline_driver(),
    }
}

fn scratch() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path()).unwrap();
    temp
}

/// Copy one task so a script can be changed without touching the checked-in suite.
fn copied_task(root: &Path, name: &str) -> Task {
    let suite = Suite::load(suite_root()).unwrap();
    let task = suite.task(name).unwrap();
    let destination = root.join(name);
    harness_eval::task::copy_task(task.dir(), &destination).unwrap();
    Task::load(&destination).unwrap()
}

fn cell<'a>(runs: &'a [TaskRun], task: &str, strategy: &str) -> &'a TaskRun {
    runs.iter()
        .find(|run| run.record.task == task && run.record.strategy == strategy)
        .expect("cell present")
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn offline_pipeline_is_deterministic_complete_and_does_not_leak_the_check() {
    let temp = scratch();
    let suite = Suite::load(suite_root()).unwrap();
    let tasks: Vec<Task> = suite.tasks().to_vec();
    let names: Vec<&str> = tasks.iter().map(Task::name).collect();
    assert_eq!(names.len(), 4);
    let check =
        HiddenCheck::probe(temp.path()).expect("bubblewrap is required for the offline gate");

    // The hidden check must fail on the pre-task workspace: it is what makes
    // completion mean something.
    for task in &tasks {
        let pristine = temp.path().join("pristine").join(task.name());
        harness_eval::task::copy_workspace(task.workspace(), &pristine).unwrap();
        let outcome = check.run_check(task.check(), &pristine);
        assert!(
            !outcome.passed,
            "{}: the pre-task workspace must not pass the hidden check",
            task.name()
        );
        // The check script is never part of the workspace.
        assert!(
            !pristine.join(check::CHECK_FILE).exists(),
            "{}",
            task.name()
        );
    }

    let first = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            run_suite(
                &tasks,
                &StrategyId::ALL,
                &options(temp.path(), 1),
                &ModelPlan::Scripted,
                &check,
            )
            .await
            .unwrap()
        })
    };
    assert_eq!(first.records.len(), 12);
    for record in &first.records {
        assert!(
            record.complete(),
            "{} {}: {:?}",
            record.task,
            record.strategy,
            record.errors
        );
        assert_eq!(record.metrics.as_ref().unwrap().completion, Some(true));
        assert_eq!(
            record
                .metrics
                .as_ref()
                .unwrap()
                .requirements_retained_projection,
            record.metrics.as_ref().unwrap().requirements_total,
            "{} {}: the scripted summary states every requirement",
            record.task,
            record.strategy
        );
        assert_eq!(
            record.metrics.as_ref().unwrap().files_recalled_projection,
            record.metrics.as_ref().unwrap().files_total,
            "{} {}",
            record.task,
            record.strategy
        );
        assert_eq!(
            record.estimator,
            Some(Estimator {
                method: "heuristic".into(),
                version: "bytes4-flat-media-v1".into(),
            })
        );
        assert!(record.tokens.unwrap().before > 0);
    }

    // Every strategy is recorded with its own version and policy fingerprint, so
    // dropping the strategy from a record cannot go unnoticed.
    let strategies: Vec<&str> = first
        .records
        .iter()
        .map(|record| record.strategy.as_str())
        .collect();
    for strategy in ["rolling_summary", "observation_masking", "structured_state"] {
        assert!(strategies.contains(&strategy), "missing {strategy}");
    }
    for record in &first.records {
        assert_eq!(record.strategy_version, "1");
        let fingerprint = record
            .configuration_fingerprint
            .as_ref()
            .unwrap_or_else(|| panic!("{} has no configuration fingerprint", record.task));
        assert_eq!(fingerprint.len(), 64);
        let summary_model = record.summary_model.as_deref();
        if record.strategy == "observation_masking" {
            assert_eq!(
                summary_model, None,
                "a model-free strategy records no model"
            );
        } else {
            assert_eq!(summary_model, Some("mock/summary"));
        }
    }
    let fingerprints: std::collections::BTreeSet<&String> = first
        .records
        .iter()
        .filter(|record| record.task == "bugfix-with-test")
        .map(|record| record.configuration_fingerprint.as_ref().unwrap())
        .collect();
    assert_eq!(
        fingerprints.len(),
        3,
        "the policy fingerprint must include the strategy"
    );

    // The hidden check never reaches a model: not the sentinel, not its name, not
    // any of its expected values.
    let mut saw_requests = false;
    for run in &first.runs {
        assert!(!run.continuation_requests.is_empty(), "{}", run.record.task);
        saw_requests = true;
        for request in &run.continuation_requests {
            assert!(
                !request.contains(harness_eval::task::CHECK_SENTINEL),
                "{} {}: the continuation saw the check sentinel",
                run.record.task,
                run.record.strategy
            );
            assert!(!request.contains(check::CHECK_FILE), "{}", run.record.task);
            assert!(!request.contains("hidden check"), "{}", run.record.task);
        }
    }
    assert!(saw_requests);
    for run in &first.runs {
        assert!(
            !run.workspace.join(check::CHECK_FILE).exists(),
            "{}: the check file must be removed after the check",
            run.record.task
        );
    }

    // Two runs of the same suite give byte-identical aggregates.
    let second = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            run_suite(
                &tasks,
                &StrategyId::ALL,
                &options(temp.path(), 2),
                &ModelPlan::Scripted,
                &check,
            )
            .await
            .unwrap()
        })
    };
    assert_eq!(
        first.aggregate.to_json().unwrap(),
        second.aggregate.to_json().unwrap()
    );
    assert_eq!(
        first.aggregate.to_markdown(),
        second.aggregate.to_markdown()
    );
    // Records differ only in timing fields.
    for (left, right) in first.records.iter().zip(second.records.iter()) {
        let mut left = left.clone();
        let mut right = right.clone();
        left.durations_ms = Default::default();
        right.durations_ms = Default::default();
        assert_eq!(left, right, "records must be equal apart from durations");
    }
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn a_summary_that_drops_a_requirement_lowers_projection_retention() {
    let temp = scratch();
    let check = HiddenCheck::probe(temp.path()).unwrap();
    let task = copied_task(temp.path(), "bugfix-with-test");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let full = runtime.block_on(async {
        run_suite(
            std::slice::from_ref(&task),
            &[StrategyId::RollingSummary],
            &options(temp.path(), 3),
            &ModelPlan::Scripted,
            &check,
        )
        .await
        .unwrap()
    });
    assert_eq!(
        full.records[0]
            .metrics
            .as_ref()
            .unwrap()
            .requirements_retained_projection,
        3
    );

    // Drop the sentence that states R1 from the scripted summary, keeping the
    // five required headings. The metric must notice.
    let summary = task.dir().join("scripted/summary.md");
    let text = fs::read_to_string(&summary).unwrap();
    let dropped: String = text
        .lines()
        .map(|line| {
            if line.contains("16 characters") {
                "A long slug is cut too short."
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&summary, format!("{dropped}\n")).unwrap();
    let damaged = Task::load(task.dir()).unwrap();
    let reduced = runtime.block_on(async {
        run_suite(
            &[damaged],
            &[StrategyId::RollingSummary],
            &options(temp.path(), 4),
            &ModelPlan::Scripted,
            &check,
        )
        .await
        .unwrap()
    });
    let metrics = reduced.records[0].metrics.as_ref().unwrap();
    assert!(
        reduced.records[0].errors.compaction.is_none(),
        "compaction must still succeed"
    );
    assert_eq!(metrics.requirements_total, 3);
    assert!(
        metrics.requirements_retained_projection < 3,
        "dropping a requirement from the summary must lower retention, got {}",
        metrics.requirements_retained_projection
    );
    let details = harness_eval::pipeline::retained_details(&task, &reduced.runs[0].projection_text);
    assert!(
        details.iter().any(|(id, retained)| id == "R1" && !retained),
        "R1 must be the dropped requirement: {details:?}"
    );
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn a_continuation_that_does_not_fix_the_task_completes_false() {
    let temp = scratch();
    let check = HiddenCheck::probe(temp.path()).unwrap();
    let task = copied_task(temp.path(), "multi-file-change");
    // A continuation that only talks: the workspace keeps the old name.
    fs::write(
        task.dir().join("scripted/continue.jsonl"),
        "{\"type\":\"assistant\",\"text\":\"Nothing to change; the helper name is fine as it is.\"}\n",
    )
    .unwrap();
    let lazy = Task::load(task.dir()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let run = runtime.block_on(async {
        run_suite(
            &[lazy],
            &[StrategyId::ObservationMasking],
            &options(temp.path(), 5),
            &ModelPlan::Scripted,
            &check,
        )
        .await
        .unwrap()
    });
    let record = &run.records[0];
    assert_eq!(
        record.metrics.as_ref().unwrap().completion,
        Some(false),
        "an unfixed workspace must not complete"
    );
    // A failed completion is a metric, not a harness error.
    assert!(record.errors.check.is_none(), "{:?}", record.errors);

    // The same cell with the fixing continuation completes.
    let again = temp.path().join("again");
    fs::create_dir_all(&again).unwrap();
    let fixed = copied_task(&again, "multi-file-change");
    let run = runtime.block_on(async {
        run_suite(
            &[fixed],
            &[StrategyId::ObservationMasking],
            &options(temp.path(), 6),
            &ModelPlan::Scripted,
            &check,
        )
        .await
        .unwrap()
    });
    assert_eq!(
        run.records[0].metrics.as_ref().unwrap().completion,
        Some(true)
    );
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn the_hidden_check_runs_inside_the_sandbox_and_never_on_the_host() {
    let temp = scratch();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(workspace.join("nested")).unwrap();
    fs::write(workspace.join("nested/file"), "content\n").unwrap();
    // A host-only file outside the workspace must be unreachable from inside.
    let sentinel = temp.path().join("host-only-sentinel");
    fs::write(&sentinel, "host\n").unwrap();
    let check = HiddenCheck::probe(temp.path()).unwrap();

    let inside = check.run_command(&workspace, "pwd");
    assert!(inside.passed, "{inside:?}");
    assert_eq!(inside.output.trim(), "/workspace", "{inside:?}");

    let hidden = check.run_command(&workspace, &format!("test ! -e {}", sentinel.display()));
    assert!(
        hidden.passed,
        "the sandbox must not see host paths: {}",
        hidden.output
    );

    let network = check.run_command(&workspace, "wc -l < /proc/net/route");
    assert!(network.passed, "{network:?}");
    assert_eq!(
        network.output.trim(),
        "1",
        "the sandbox must have no routes: {}",
        network.output
    );

    // No unrestricted fallback: an unusable backend refuses instead of running
    // the check on the host.
    let missing = harness_core::tools::SandboxConfig {
        workspace_root: workspace.clone(),
        bwrap: temp.path().join("no-such-bwrap"),
        ..Default::default()
    };
    assert!(
        harness_core::tools::Sandbox::probe(missing).is_err(),
        "an unavailable backend must fail the probe, never fall back"
    );

    // The check script is written into the workspace only for the run itself.
    let task = copied_task(temp.path(), "feature-style-rule");
    let outcome = check.run_check(task.check(), &workspace);
    assert!(outcome.exit_code.is_some(), "{outcome:?}");
    assert!(!workspace.join(check::CHECK_FILE).exists());
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn aggregate_halves_are_derived_from_the_records() {
    // Guards the aggregate against drifting from the records it claims to sum.
    let temp = scratch();
    let suite = Suite::load(suite_root()).unwrap();
    let tasks: Vec<Task> = suite.tasks().to_vec();
    let check = HiddenCheck::probe(temp.path()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let run = runtime.block_on(async {
        run_suite(
            &tasks,
            &StrategyId::ALL,
            &options(temp.path(), 7),
            &ModelPlan::Scripted,
            &check,
        )
        .await
        .unwrap()
    });
    let aggregate = Aggregate::new(&run.records, "gate");
    for totals in &aggregate.strategies {
        let summed: u64 = run
            .records
            .iter()
            .filter(|record| record.strategy == totals.strategy)
            .map(|record| {
                record
                    .metrics
                    .as_ref()
                    .unwrap()
                    .requirements_retained_projection
            })
            .sum();
        assert_eq!(totals.requirements_retained_projection, summed);
    }
    // Writing the run is stable and round-trips through JSON.
    let written = harness_eval::write_run(temp.path(), &run.records).unwrap();
    assert_eq!(written.to_json().unwrap(), aggregate.to_json().unwrap());
    let reread: Aggregate =
        serde_json::from_str(&fs::read_to_string(temp.path().join("aggregate.json")).unwrap())
            .unwrap();
    assert_eq!(reread, aggregate);
    let _ = cell(&run.runs, "bugfix-with-test", "rolling_summary");
}
