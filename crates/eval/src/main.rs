//! `harness-eval`: the dev-only evaluation entry point.
//!
//! Offline mode (`--offline`) runs the whole pipeline with `ScriptedProvider`
//! and is what the gate executes. Live mode (`--live`) needs a credential in the
//! environment, prints a coarse token estimate and requires `--yes`; it is never
//! part of CI.

use std::{path::PathBuf, sync::Arc};

use clap::{Arg, ArgAction, Command};
use harness_core::context::StrategyId;
use harness_eval::{
    Aggregate, HiddenCheck, ModelPlan, RunOptions, Task, live_model, live_providers,
    offline_driver, offline_model, offline_summary_model, parse_strategy, record::run_id,
    run_suite, write_run,
};

fn main() {
    let matches = Command::new("harness-eval")
        .about("Dev-only harness comparing context-compaction strategies")
        .arg(
            Arg::new("offline")
                .long("offline")
                .help("Run every cell with ScriptedProvider")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("live")
                .long("live")
                .help("Run every cell with a real provider")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("provider")
                .long("provider")
                .value_name("PROVIDER")
                .help("Provider instance for a live run, such as deepseek"),
        )
        .arg(
            Arg::new("model")
                .long("model")
                .value_name("MODEL")
                .help("Session model for a live run; defaults to the provider's"),
        )
        .arg(
            Arg::new("summary-model")
                .long("summary-model")
                .value_name("MODEL")
                .help("Summary model for a live run; defaults to the session model"),
        )
        .arg(
            Arg::new("repetitions")
                .long("repetitions")
                .value_name("N")
                .value_parser(clap::value_parser!(u32))
                .help("Repetitions per task and strategy (default 1)"),
        )
        .arg(
            Arg::new("tasks")
                .long("tasks")
                .value_name("NAMES")
                .value_delimiter(',')
                .help("Comma-separated task names (default: every task)"),
        )
        .arg(
            Arg::new("strategies")
                .long("strategies")
                .value_name("NAMES")
                .value_delimiter(',')
                .help("Comma-separated strategies (default: all three)"),
        )
        .arg(
            Arg::new("out")
                .long("out")
                .value_name("DIR")
                .help("Run directory (default: eval/results/<run-id>)"),
        )
        .arg(
            Arg::new("tasks-root")
                .long("tasks-root")
                .value_name("DIR")
                .help("Task suite root (default: eval/tasks)"),
        )
        .arg(
            Arg::new("commit")
                .long("commit")
                .value_name("SHA")
                .help("Harness commit to record, usually `git rev-parse HEAD`"),
        )
        .arg(
            Arg::new("yes")
                .long("yes")
                .help("Confirm a live run after reading its estimate")
                .action(ArgAction::SetTrue),
        )
        .get_matches();

    let offline = matches.get_flag("offline");
    let live = matches.get_flag("live");
    if offline == live {
        usage("select exactly one of --offline or --live");
    }
    let root = repo_root();
    let tasks_root = matches
        .get_one::<String>("tasks-root")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("eval/tasks"));
    let out = matches
        .get_one::<String>("out")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("eval/results").join(run_id()));
    let commit = matches
        .get_one::<String>("commit")
        .cloned()
        .unwrap_or_else(|| "unknown".into());
    let repetitions = matches
        .get_one::<u32>("repetitions")
        .copied()
        .unwrap_or(1)
        .max(1) as usize;
    let names: Option<Vec<String>> = matches
        .get_many::<String>("tasks")
        .map(|values| values.cloned().collect());
    let strategies = match matches
        .get_many::<String>("strategies")
        .map(|values| values.cloned().collect::<Vec<_>>())
    {
        None => StrategyId::ALL.to_vec(),
        Some(names) => {
            let mut parsed = Vec::new();
            for name in names {
                match parse_strategy(&name) {
                    Ok(strategy) => parsed.push(strategy),
                    Err(message) => usage(&message),
                }
            }
            parsed
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => fail(&2, format!("runtime: {error}")),
    };
    let code = runtime.block_on(run(
        offline,
        &tasks_root,
        &out,
        &commit,
        repetitions,
        names,
        strategies,
        &matches,
    ));
    std::process::exit(code);
}

#[allow(clippy::too_many_arguments)]
async fn run(
    offline: bool,
    tasks_root: &std::path::Path,
    out: &std::path::Path,
    commit: &str,
    repetitions: usize,
    names: Option<Vec<String>>,
    strategies: Vec<StrategyId>,
    matches: &clap::ArgMatches,
) -> i32 {
    let suite = match harness_eval::Suite::load(tasks_root) {
        Ok(suite) => suite,
        Err(error) => fail(&2, format!("task suite: {error}")),
    };
    let tasks: Vec<Task> = match names {
        None => suite.tasks().to_vec(),
        Some(names) => {
            let mut selected = Vec::new();
            for name in names {
                match suite.task(&name) {
                    Some(task) => selected.push(task.clone()),
                    None => fail(&2, format!("unknown task {name:?}")),
                }
            }
            selected
        }
    };
    let scratch_root = harness_eval::scratch_root();
    if let Err(error) = std::fs::create_dir_all(&scratch_root) {
        fail(&2, format!("{}: {error}", scratch_root.display()));
    }
    let check = match HiddenCheck::probe(&scratch_root) {
        Ok(check) => check,
        Err(reason) => fail(&1, format!("sandbox unavailable: {reason}")),
    };

    if offline {
        let options = RunOptions {
            scratch_root: scratch_root.clone(),
            commit: commit.to_owned(),
            repetitions,
            model: offline_model(),
            summary_model: Some(offline_summary_model()),
            driver: offline_driver(),
        };
        let code = finish(
            out,
            run_suite(&tasks, &strategies, &options, &ModelPlan::Scripted, &check).await,
        );
        tidy(&scratch_root, code);
        return code;
    }

    // Live: opt-in, never in CI. A credential comes from the environment; the
    // adapter is the only thing that reads it.
    let Some(provider_name) = matches.get_one::<String>("provider") else {
        fail(&2, "--live needs --provider".into());
    };
    if !matches.get_flag("yes") {
        eprintln!("harness-eval: --live spends money; pass --yes after reading the estimate");
    }
    let providers = match live_providers(provider_name) {
        Ok(providers) => providers,
        Err(diagnostic) => fail(&2, format!("provider: {diagnostic}")),
    };
    let model = live_model(
        provider_name,
        matches.get_one::<String>("model").map(String::as_str),
    );
    let summary_model = Some(live_model(
        provider_name,
        matches
            .get_one::<String>("summary-model")
            .map(String::as_str),
    ));
    let driver = providers.driver.clone();
    let dry_scratch = harness_eval::scratch_root();
    let dry = RunOptions {
        scratch_root: dry_scratch.clone(),
        commit: commit.to_owned(),
        repetitions,
        model: model.clone(),
        summary_model: summary_model.clone(),
        driver: driver.clone(),
    };
    // A scripted dry run measures the projections before any money is spent and
    // proves the mechanics work with this task suite.
    let dry_run = match run_suite(&tasks, &strategies, &dry, &ModelPlan::Scripted, &check).await {
        Ok(run) => run,
        Err(error) => fail(&1, format!("dry run: {error}")),
    };
    let estimate = harness_eval::estimate_from(&dry_run.runs);
    eprintln!("{}", estimate.describe());
    if !matches.get_flag("yes") {
        return 2;
    }
    let options = RunOptions {
        scratch_root: scratch_root.clone(),
        commit: commit.to_owned(),
        repetitions,
        model,
        summary_model,
        driver,
    };
    let plan = ModelPlan::Live(
        Arc::clone(&providers.provider) as Arc<dyn harness_core::model::ModelProvider>
    );
    let code = finish(
        out,
        run_suite(&tasks, &strategies, &options, &plan, &check).await,
    );
    tidy(&dry_scratch, code);
    tidy(&scratch_root, code);
    code
}

/// Remove a scratch tree only after a clean run; a failed run keeps it for
/// inspection and says where it is.
fn tidy(scratch: &std::path::Path, code: i32) {
    if code == 0 {
        let _ = std::fs::remove_dir_all(scratch);
    } else {
        eprintln!("scratch kept for inspection: {}", scratch.display());
    }
}

fn finish(out: &std::path::Path, result: Result<harness_eval::SuiteRun, String>) -> i32 {
    let suite = match result {
        Ok(suite) => suite,
        Err(error) => fail(&1, error),
    };
    let aggregate: Aggregate = match write_run(out, &suite.records) {
        Ok(aggregate) => aggregate,
        Err(error) => fail(&1, error),
    };
    print!("{}", aggregate.to_markdown());
    eprintln!("results: {}", out.display());
    let failures = suite
        .records
        .iter()
        .filter(|record| !record.complete())
        .count();
    if failures > 0 {
        for record in suite.records.iter().filter(|record| !record.complete()) {
            eprintln!(
                "{} {} #{}: replay={:?} compaction={:?} probe={:?} continuation={:?} check={:?}",
                record.task,
                record.strategy,
                record.repetition,
                record.errors.replay,
                record.errors.compaction,
                record.errors.probe,
                record.errors.continuation,
                record.errors.check,
            );
        }
        return 1;
    }
    0
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn usage(message: &str) -> ! {
    fail(&2, message.to_owned())
}

fn fail(code: &i32, message: String) -> ! {
    eprintln!("harness-eval: {message}");
    std::process::exit(*code);
}
