//! The offline gate: the whole pipeline through the real runtime, storage, file
//! tools, compaction and the Bubblewrap sandbox.
//!
//! Ignored by default and run by `scripts/check.sh` where `bwrap` is available,
//! exactly like the product's S02 targets. It proves the mechanics, never model
//! quality: the post-history answers are scripts, and every metric assertion here
//! is about what the harness measures, not about which strategy is better.
//!
//! This target also carries the regressions for the review findings on m6-05:
//! exclusive check publication, live replay of the fixed history, real
//! pre-compaction progress, awaited shutdown on failure paths, commit and
//! configuration provenance, and completion checks that reject explicit
//! constraint violations.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use harness_core::{context::StrategyId, model::scripted::ScriptedProvider};
use harness_eval::{
    Aggregate, HiddenCheck, ModelPlan, RunOptions, Suite, Task, TaskRun, check, offline_driver,
    offline_model, offline_summary_model, record::Estimator, run_suite,
};

const COMMIT: &str = "970b9b566a256af9b50cd037297375d31e0f579f";

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn hidden_check_cleanup_preserves_a_replaced_regular_file() {
    let temp = scratch();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let script = temp.path().join("check.sh");
    fs::write(
        &script,
        "rm .harness-eval-check.sh\nprintf replacement > .harness-eval-check.sh\n",
    )
    .unwrap();
    let check = HiddenCheck::probe(temp.path()).unwrap();
    let outcome = check.run_check(&script, &workspace);
    assert!(!outcome.passed);
    assert_eq!(outcome.code.as_deref(), Some("check_carrier_replaced"));
    assert_eq!(
        fs::read(workspace.join(check::CHECK_FILE)).unwrap(),
        b"replacement"
    );
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn a_successful_live_plan_reaches_completion_without_offline_reply_leakage() {
    use harness_core::{
        model::{
            ProviderEvent,
            scripted::{Script, Step},
        },
        session::FinishReason,
    };
    let reply = |text: &str| {
        Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, text),
            Step::completed(FinishReason::Stop),
        ])
    };
    let temp = scratch();
    let suite = Suite::load(suite_root()).unwrap();
    let task = suite.task("bugfix-with-test").unwrap();
    let check = HiddenCheck::probe(temp.path()).unwrap();
    for (slot, strategy) in StrategyId::ALL.into_iter().enumerate() {
        let mut replies = Vec::new();
        if strategy.needs_model() {
            replies.push(reply(&task.scripted().summary));
        }
        replies.push(reply(&task.scripted().probe));
        replies.push(reply("No changes made."));
        let live = Arc::new(ScriptedProvider::new(replies));
        let run = runtime().block_on(harness_eval::run_task(
            task,
            strategy,
            0,
            &options(temp.path(), 100 + slot as u64),
            &ModelPlan::Live(live.clone()),
            &check,
        ));
        assert!(run.record.complete(), "{strategy}: {:?}", run.record.errors);
        assert_eq!(run.record.metrics.as_ref().unwrap().completion, Some(false));
        assert_eq!(run.compactions, 1);
        assert_eq!(
            live.streams().len(),
            2 + usize::from(strategy.needs_model())
        );
        assert_eq!(live.remaining(), 0);
    }
}

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
        commit: COMMIT.into(),
        repetitions: 1,
        model: offline_model(),
        summary_model: Some(offline_summary_model()),
        driver: offline_driver(),
        stage_timeout: harness_eval::pipeline::STAGE_TIMEOUT,
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn scratch() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path()).unwrap();
    temp
}

/// Copy one task so a script or the workspace can be changed without touching the
/// checked-in suite.
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

/// One cell with the ordinary scripted plan.
fn scripted_cell(
    temp: &Path,
    task: &Task,
    strategy: StrategyId,
    slot: u64,
    check: &HiddenCheck,
) -> TaskRun {
    runtime().block_on(harness_eval::run_task(
        task,
        strategy,
        0,
        &options(temp, slot),
        &ModelPlan::Scripted,
        check,
    ))
}

/// A cell whose continuation completed and whose hidden check passed.
fn fixing_run(temp: &Path, task_name: &str, slot: u64) -> (HiddenCheck, Task, TaskRun) {
    let suite = Suite::load(suite_root()).unwrap();
    let task = suite.task(task_name).unwrap().clone();
    let check = HiddenCheck::probe(temp).unwrap();
    let run = scripted_cell(temp, &task, StrategyId::ObservationMasking, slot, &check);
    assert_eq!(
        run.record.metrics.as_ref().unwrap().completion,
        Some(true),
        "{}: {}",
        task_name,
        run.record.errors.check.clone().unwrap_or_default()
    );
    (check, task, run)
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

    let first = runtime().block_on(async {
        run_suite(
            &tasks,
            &StrategyId::ALL,
            &options(temp.path(), 1),
            &ModelPlan::Scripted,
            &check,
        )
        .await
        .unwrap()
    });
    assert_eq!(first.records.len(), 12);
    for run in &first.runs {
        let record = &run.record;
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
        // Exactly one measured compaction per cell.
        assert_eq!(run.compactions, 1, "{} {}", record.task, record.strategy);
        // Decision 15: two identities, both recorded, never conflated.
        let session_configuration = record
            .session_configuration
            .as_ref()
            .unwrap_or_else(|| panic!("{} has no session configuration digest", record.task));
        let policy = record
            .compaction_policy
            .as_ref()
            .unwrap_or_else(|| panic!("{} has no compaction policy", record.task));
        assert_eq!(session_configuration.len(), 64);
        assert_eq!(policy.len(), 64);
        assert_ne!(session_configuration, policy);
        assert_eq!(record.schema, 2);
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
        // Decision 16 bumped only the prompts of the two model-based strategies.
        let version = if record.strategy == "observation_masking" {
            "1"
        } else {
            "2"
        };
        assert_eq!(record.strategy_version, version, "{}", record.strategy);
        assert_eq!(record.harness_commit, COMMIT);
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
    let policies: BTreeSet<&String> = first
        .records
        .iter()
        .filter(|record| record.task == "bugfix-with-test")
        .map(|record| record.compaction_policy.as_ref().unwrap())
        .collect();
    assert_eq!(
        policies.len(),
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

    // The ground truth for file recall is what the history really changed.
    for run in &first.runs {
        let task = tasks
            .iter()
            .find(|task| task.name() == run.record.task)
            .unwrap();
        assert_eq!(
            run.pre_compaction_changes,
            task.modified_files(),
            "{} {}: modified_files.json must name the files the history changed",
            run.record.task,
            run.record.strategy
        );
    }

    // Two runs of the same suite give byte-identical aggregates (M6 decision 15:
    // the aggregate, not the raw record, is the deterministic comparison).
    let second = runtime().block_on(async {
        run_suite(
            &tasks,
            &StrategyId::ALL,
            &options(temp.path(), 2),
            &ModelPlan::Scripted,
            &check,
        )
        .await
        .unwrap()
    });
    assert_eq!(
        first.aggregate.to_json().unwrap(),
        second.aggregate.to_json().unwrap()
    );
    assert_eq!(
        first.aggregate.to_markdown(),
        second.aggregate.to_markdown()
    );
    // Records differ only in timing and in the exact session-configuration digest,
    // which embeds the ephemeral workspace path. Provenance is never weakened to
    // make raw records equal.
    for (left, right) in first.records.iter().zip(second.records.iter()) {
        let mut left = left.clone();
        let mut right = right.clone();
        left.durations_ms = Default::default();
        right.durations_ms = Default::default();
        assert_ne!(
            left.session_configuration, right.session_configuration,
            "the session configuration digest embeds the scratch path"
        );
        assert_eq!(left.compaction_policy, right.compaction_policy);
        left.session_configuration = None;
        right.session_configuration = None;
        assert_eq!(
            left, right,
            "records must be equal apart from durations and the session digest"
        );
    }
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn every_task_declares_exactly_the_files_its_history_changed() {
    let temp = scratch();
    let suite = Suite::load(suite_root()).unwrap();
    let tasks: Vec<Task> = suite.tasks().to_vec();
    let check = HiddenCheck::probe(temp.path()).unwrap();
    runtime().block_on(async {
        let run = run_suite(
            &tasks,
            &[StrategyId::ObservationMasking],
            &options(temp.path(), 3),
            &ModelPlan::Scripted,
            &check,
        )
        .await
        .unwrap();
        for (task, cell) in tasks.iter().zip(run.runs.iter()) {
            assert!(
                !cell.pre_compaction_changes.is_empty(),
                "{}: the history must really modify the workspace",
                task.name()
            );
            assert_eq!(cell.pre_compaction_changes, task.modified_files());
            // The task is unfinished at that boundary: the hidden check fails on
            // the workspace as it stood *before* the continuation ran.
            let boundary = cell
                .pre_compaction_workspace
                .as_ref()
                .unwrap_or_else(|| panic!("{}: no pre-compaction snapshot", task.name()));
            let outcome = check.run_check(task.check(), boundary);
            assert!(
                !outcome.passed,
                "{}: the task must stay incomplete until the continuation: {}",
                task.name(),
                outcome.output
            );
            // The continuation is what finishes it.
            assert_eq!(
                cell.record.metrics.as_ref().unwrap().completion,
                Some(true),
                "{}: the continuation must complete the task",
                task.name()
            );
        }
    });
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn a_summary_that_drops_a_requirement_lowers_projection_retention() {
    let temp = scratch();
    let check = HiddenCheck::probe(temp.path()).unwrap();
    let task = copied_task(temp.path(), "bugfix-with-test");

    let full = runtime().block_on(async {
        run_suite(
            std::slice::from_ref(&task),
            &[StrategyId::RollingSummary],
            &options(temp.path(), 4),
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
    let reduced = runtime().block_on(async {
        run_suite(
            &[damaged],
            &[StrategyId::RollingSummary],
            &options(temp.path(), 5),
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
    // A continuation that only talks: the last call site keeps the old name.
    fs::write(
        task.dir().join("scripted/continue.jsonl"),
        "{\"type\":\"assistant\",\"text\":\"Nothing to change; the helper name is fine as it is.\"}\n",
    )
    .unwrap();
    let lazy = Task::load(task.dir()).unwrap();
    let run = scripted_cell(
        temp.path(),
        &lazy,
        StrategyId::ObservationMasking,
        6,
        &check,
    );
    assert_eq!(
        run.record.metrics.as_ref().unwrap().completion,
        Some(false),
        "an unfixed workspace must not complete"
    );
    // A failed completion is a metric, not a harness error.
    assert!(run.record.errors.check.is_none(), "{:?}", run.record.errors);

    // The same cell with the fixing continuation completes.
    let again = temp.path().join("again");
    fs::create_dir_all(&again).unwrap();
    let fixed = copied_task(&again, "multi-file-change");
    let run = scripted_cell(
        temp.path(),
        &fixed,
        StrategyId::ObservationMasking,
        7,
        &check,
    );
    assert_eq!(run.record.metrics.as_ref().unwrap().completion, Some(true));
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
fn hidden_check_publication_refuses_links_and_existing_paths() {
    let temp = scratch();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let script = temp.path().join("check.sh");
    fs::write(&script, b"exit 0\n").unwrap();
    let check = HiddenCheck::probe(temp.path()).unwrap();
    let carrier = workspace.join(check::CHECK_FILE);

    // A model-created symlink must never be followed: the external target keeps
    // its bytes and the check does not run.
    let outside = temp.path().join("outside-sentinel");
    fs::write(&outside, b"host data must survive\n").unwrap();
    std::os::unix::fs::symlink(&outside, &carrier).unwrap();
    let outcome = check.run_check(&script, &workspace);
    // The security property first: the link target keeps its bytes.
    assert_eq!(
        fs::read(&outside).unwrap(),
        b"host data must survive\n",
        "a symlinked carrier must never be written through"
    );
    assert!(!outcome.passed, "a symlinked carrier must be refused");
    assert_eq!(outcome.code.as_deref(), Some("check_carrier_exists"));
    assert_eq!(
        fs::read_link(&carrier).unwrap(),
        outside,
        "the refusal must leave the link alone"
    );
    fs::remove_file(&carrier).unwrap();

    // A pre-existing regular file is not overwritten either.
    fs::write(&carrier, b"existing\n").unwrap();
    let outcome = check.run_check(&script, &workspace);
    assert!(!outcome.passed, "an existing carrier must be refused");
    assert_eq!(outcome.code.as_deref(), Some("check_carrier_exists"));
    assert_eq!(fs::read(&carrier).unwrap(), b"existing\n");
    fs::remove_file(&carrier).unwrap();

    // A hardlink to something outside cannot be used as a write target.
    let linked = temp.path().join("hardlink-target");
    fs::write(&linked, b"linked\n").unwrap();
    fs::hard_link(&linked, &carrier).unwrap();
    let outcome = check.run_check(&script, &workspace);
    assert!(!outcome.passed, "a hardlinked carrier must be refused");
    assert_eq!(fs::read(&linked).unwrap(), b"linked\n");
    fs::remove_file(&carrier).unwrap();

    // An ordinary run still succeeds and leaves nothing behind.
    let outcome = check.run_check(&script, &workspace);
    assert!(outcome.passed, "{outcome:?}");
    assert!(outcome.code.is_none(), "{outcome:?}");
    assert!(!carrier.exists());
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn live_plan_replays_the_fixed_history_before_the_first_live_request() {
    let temp = scratch();
    let suite = Suite::load(suite_root()).unwrap();
    let task = suite.task("bugfix-with-test").unwrap();
    let check = HiddenCheck::probe(temp.path()).unwrap();
    // No paid API: the injected live provider fails on its first request. That
    // request must belong to compaction, after all scripted history has replayed.
    let live = Arc::new(ScriptedProvider::new([]));
    let run = runtime().block_on(harness_eval::run_task(
        task,
        StrategyId::RollingSummary,
        0,
        &options(temp.path(), 8),
        &ModelPlan::Live(live.clone()),
        &check,
    ));
    assert!(
        run.record.errors.replay.is_none(),
        "fixed history must not use the live provider: {:?}",
        run.record.errors
    );
    assert_eq!(
        run.record.errors.compaction.as_deref(),
        Some("provider_failure"),
        "the intentionally empty live provider must first fail at compaction: {:?}",
        run.record.errors
    );
    assert!(run.record.errors.probe.is_none());
    assert_eq!(live.streams().len(), 1);
    // The history really ran: the workspace holds the scripted progress.
    assert_eq!(run.pre_compaction_changes, task.modified_files());
    assert_eq!(
        run.history_requests.len(),
        task.turns()
            .iter()
            .map(|turn| turn.steps.len())
            .sum::<usize>(),
        "one inference per history step"
    );
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn live_and_scripted_plans_replay_identical_history_and_workspaces() {
    let temp = scratch();
    let suite = Suite::load(suite_root()).unwrap();
    let check = HiddenCheck::probe(temp.path()).unwrap();
    let mut baseline: Option<(Vec<String>, Vec<String>)> = None;
    let mut slot = 10;
    let strategies = [StrategyId::RollingSummary, StrategyId::ObservationMasking];
    for task in suite.tasks() {
        for strategy in strategies {
            // A model-free compaction leaves the first live request at the probe,
            // which the empty provider fails; every other strategy fails at the
            // summary. Either way the history must be identical.
            let live = Arc::new(ScriptedProvider::new([]));
            let live_run = runtime().block_on(harness_eval::run_task(
                task,
                strategy,
                0,
                &options(temp.path(), slot),
                &ModelPlan::Live(live.clone()),
                &check,
            ));
            slot += 1;
            let scripted_run = scripted_cell(temp.path(), task, strategy, slot, &check);
            slot += 1;

            assert_eq!(
                live_run.history_requests,
                scripted_run.history_requests,
                "{} {}: the live plan must replay the same history",
                task.name(),
                strategy.as_str()
            );
            assert_eq!(
                live_run.pre_compaction_changes,
                scripted_run.pre_compaction_changes,
                "{} {}: the live plan must produce the same workspace",
                task.name(),
                strategy.as_str()
            );
            assert_eq!(
                live_run.history_requests.len(),
                task.turns()
                    .iter()
                    .map(|turn| turn.steps.len())
                    .sum::<usize>(),
                "{}: one inference per history step",
                task.name()
            );
            // The live provider is never used during replay: the first live
            // request is the summary or, for a model-free strategy, the probe,
            // and the empty provider fails there.
            assert_eq!(
                live.streams().len(),
                1,
                "{} {}: only the post-history stage may reach the live provider",
                task.name(),
                strategy.as_str()
            );
            let key = (
                live_run.history_requests.clone(),
                live_run.pre_compaction_changes.clone(),
            );
            match &baseline {
                None => baseline = Some(key),
                Some(first) => {
                    assert_eq!(
                        first.0,
                        key.0,
                        "{} {}: history inputs must not depend on the strategy",
                        task.name(),
                        strategy.as_str()
                    );
                }
            }
            assert_eq!(scripted_run.compactions, 1);
        }
        baseline = None;
    }
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn a_failed_compaction_skips_the_probe_and_the_hidden_check() {
    let temp = scratch();
    let suite = Suite::load(suite_root()).unwrap();
    let task = suite.task("feature-style-rule").unwrap();
    let check = HiddenCheck::probe(temp.path()).unwrap();
    let live = Arc::new(ScriptedProvider::new([]));
    let run = runtime().block_on(harness_eval::run_task(
        task,
        StrategyId::RollingSummary,
        0,
        &options(temp.path(), 30),
        &ModelPlan::Live(live.clone()),
        &check,
    ));
    assert_eq!(
        run.record.errors.compaction.as_deref(),
        Some("provider_failure")
    );
    assert!(run.record.errors.probe.is_none(), "{:?}", run.record.errors);
    assert!(
        run.record.errors.continuation.is_none(),
        "{:?}",
        run.record.errors
    );
    assert_eq!(live.streams().len(), 1, "no probe or continuation request");
    assert!(
        run.record.metrics.is_none(),
        "no metrics are published without a compaction"
    );
    assert!(
        !run.workspace.join(check::CHECK_FILE).exists(),
        "no hidden check after a failed compaction"
    );
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn a_probe_failure_is_attributed_and_skips_the_hidden_check() {
    let temp = scratch();
    let suite = Suite::load(suite_root()).unwrap();
    let task = suite.task("multi-file-change").unwrap();
    let check = HiddenCheck::probe(temp.path()).unwrap();
    // Masking needs no model, so the first live request is the probe.
    let live = Arc::new(ScriptedProvider::new([]));
    let run = runtime().block_on(harness_eval::run_task(
        task,
        StrategyId::ObservationMasking,
        0,
        &options(temp.path(), 31),
        &ModelPlan::Live(live.clone()),
        &check,
    ));
    assert!(
        run.record.errors.replay.is_none(),
        "{:?}",
        run.record.errors
    );
    assert!(
        run.record.errors.compaction.is_none(),
        "{:?}",
        run.record.errors
    );
    assert_eq!(
        run.record.errors.probe.as_deref(),
        Some("provider_failure"),
        "the probe failure must be attributed to the probe: {:?}",
        run.record.errors
    );
    assert!(run.record.errors.continuation.is_none());
    assert_eq!(live.streams().len(), 1);
    assert!(
        run.record.metrics.is_none(),
        "no scores are published without a probe answer"
    );
    // The continuation never ran, so the workspace is still the history's state.
    assert_eq!(
        harness_eval::pipeline::changed_paths(task.workspace(), &run.workspace).unwrap(),
        task.modified_files()
    );
    assert!(!run.workspace.join(check::CHECK_FILE).exists());
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn a_stalled_probe_times_out_and_the_continuation_never_runs() {
    let temp = scratch();
    let suite = Suite::load(suite_root()).unwrap();
    let task = suite.task("bugfix-with-test").unwrap();
    let check = HiddenCheck::probe(temp.path()).unwrap();
    // One script that never yields and ignores cancellation: the probe inference
    // hangs, so the injected short deadline must end the cell.
    let stalled = Arc::new(ScriptedProvider::new([
        harness_core::model::scripted::Script::new([harness_core::model::scripted::Step::Stall]),
    ]));
    let mut options = options(temp.path(), 32);
    options.stage_timeout = std::time::Duration::from_millis(250);
    let run = runtime().block_on(harness_eval::run_task(
        task,
        StrategyId::ObservationMasking,
        0,
        &options,
        &ModelPlan::Live(stalled.clone()),
        &check,
    ));
    assert_eq!(
        run.record.errors.probe.as_deref(),
        Some("stage_timeout"),
        "a stalled probe must time out: {:?}",
        run.record.errors
    );
    assert!(
        run.record.errors.replay.is_none(),
        "{:?}",
        run.record.errors
    );
    assert!(run.record.errors.continuation.is_none());
    assert!(run.record.errors.check.is_none());
    assert_eq!(stalled.streams().len(), 1, "no later provider activity");
    assert!(
        run.record.metrics.is_none(),
        "no scores are published without a probe answer"
    );
    // The continuation would have edited test.sh; it never ran.
    assert_eq!(
        harness_eval::pipeline::changed_paths(task.workspace(), &run.workspace).unwrap(),
        task.modified_files()
    );
    assert!(!run.workspace.join(check::CHECK_FILE).exists());
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn a_non_compaction_configuration_change_changes_source_provenance() {
    let temp = scratch();
    let check = HiddenCheck::probe(temp.path()).unwrap();
    let plain = copied_task(temp.path(), "feature-style-rule");
    let with_instructions =
        copied_task(&temp.path().join("with-instructions"), "feature-style-rule");
    // A developer instruction is not part of the compaction policy, but it is part
    // of the session configuration the committed entry binds.
    fs::write(
        with_instructions.workspace().join("AGENTS.md"),
        "# House rules\n\nUse two spaces.\n",
    )
    .unwrap();
    let with_instructions = Task::load(with_instructions.dir()).unwrap();

    let first = scripted_cell(
        temp.path(),
        &plain,
        StrategyId::ObservationMasking,
        33,
        &check,
    );
    let second = scripted_cell(
        temp.path(),
        &with_instructions,
        StrategyId::ObservationMasking,
        34,
        &check,
    );
    assert!(
        first.record.errors.compaction.is_none(),
        "{:?}",
        first.record.errors
    );
    assert!(
        second.record.errors.compaction.is_none(),
        "{:?}",
        second.record.errors
    );
    assert_ne!(
        first.record.session_configuration, second.record.session_configuration,
        "an instruction change must change source-configuration provenance"
    );
    assert_eq!(
        first.record.compaction_policy, second.record.compaction_policy,
        "an instruction change is not a compaction-policy change"
    );
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn completion_rejects_a_change_to_the_forbidden_math_file() {
    let temp = scratch();
    let (check, task, run) = fixing_run(temp.path(), "multi-file-change", 40);
    let path = run.workspace.join("lib/math.sh");
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_slice(b"\n# This file was changed despite the explicit constraint.\n");
    fs::write(path, bytes).unwrap();
    assert!(
        !check.run_check(task.check(), &run.workspace).passed,
        "a forbidden-file change must fail completion even if add still works"
    );
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn completion_rejects_an_extra_file_in_the_no_new_files_task() {
    let temp = scratch();
    let (check, task, run) = fixing_run(temp.path(), "refactor-constraint", 41);
    fs::write(run.workspace.join("unexpected.txt"), b"extra\n").unwrap();
    assert!(
        !check.run_check(task.check(), &run.workspace).passed,
        "an arbitrary added file must fail the no-new-files constraint"
    );
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn byte_identical_output_includes_the_final_newline() {
    let temp = scratch();
    let (check, task, run) = fixing_run(temp.path(), "refactor-constraint", 42);
    let path = run.workspace.join("report.sh");
    let text = fs::read_to_string(&path).unwrap();
    let changed = text.replace("emit 'beta' '2'\n", "emit 'beta' '2' | tr -d '\\n'\n");
    assert_ne!(changed, text);
    fs::write(path, changed).unwrap();
    assert!(
        !check.run_check(task.check(), &run.workspace).passed,
        "dropping the output's final newline must fail byte identity"
    );
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn cli_records_a_real_commit_and_refuses_a_malformed_one() {
    let temp = scratch();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_harness-eval"))
        .args([
            "--offline",
            "--tasks",
            "bugfix-with-test",
            "--strategies",
            "observation_masking",
            "--out",
        ])
        .arg(temp.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the offline CLI must succeed without --commit: {stderr}"
    );
    let record: serde_json::Value = serde_json::from_slice(
        &fs::read(
            temp.path()
                .join("records/bugfix-with-test.observation_masking.0.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let commit = record["harness_commit"].as_str().unwrap();
    assert_eq!(
        commit.len(),
        40,
        "successful records need the actual commit, got {commit:?}"
    );
    assert!(commit.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_ne!(commit, "unknown");

    // A malformed commit is refused rather than recorded.
    let rejected = std::process::Command::new(env!("CARGO_BIN_EXE_harness-eval"))
        .args(["--offline", "--commit", "unknown", "--out"])
        .arg(temp.path().join("rejected"))
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        stderr.contains("--commit") && stderr.contains("40-character"),
        "unexpected rejection: {stderr}"
    );
}

#[test]
#[ignore = "spawns the Bubblewrap sandbox; runs in the S02 gate"]
fn aggregate_halves_are_derived_from_the_records() {
    // Guards the aggregate against drifting from the records it claims to sum.
    let temp = scratch();
    let suite = Suite::load(suite_root()).unwrap();
    let tasks: Vec<Task> = suite.tasks().to_vec();
    let check = HiddenCheck::probe(temp.path()).unwrap();
    let run = runtime().block_on(async {
        run_suite(
            &tasks,
            &StrategyId::ALL,
            &options(temp.path(), 50),
            &ModelPlan::Scripted,
            &check,
        )
        .await
        .unwrap()
    });
    let aggregate = Aggregate::new(&run.records, COMMIT);
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
    // The exact session-configuration digest is never aggregated (decision 15).
    let json = aggregate.to_json().unwrap();
    for record in &run.records {
        assert!(
            !json.contains(record.session_configuration.as_ref().unwrap()),
            "the aggregate must not embed the raw session configuration digest"
        );
    }
    let _ = cell(&run.runs, "bugfix-with-test", "rolling_summary");
}
