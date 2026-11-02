//! Real adapter and process I/O wiring for the injected headless client.
use clap::{Arg, ArgAction, ArgMatches, Command, parser::ValueSource, value_parser};
use harness::ask::{self, ControllingTerminal, Options};
use harness_core::{
    context::StrategyId,
    session::{ModelRef, Portability, SessionId},
    tools::SandboxConfig,
};
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    process::ExitCode,
    str::FromStr,
};

pub fn command() -> Command {
    Command::new("ask")
        .about("Stream one durable turn; continue or switch models with --session")
        .long_about("Stream assistant text to stdout; diagnostics go to stderr. Tool approvals use /dev/tty, never stdin. Without a terminal every call is denied. Ctrl-C cancels durably; a second Ctrl-C exits immediately. A continuation that names another --provider or --model switches explicitly under --switch-policy: `reject` (default) refuses any optional native-state loss after printing the portability report, `omit-optional` drops it and prints the same report. Exit codes: 0 completed, 1 turn failure, 2 usage/configuration (including a refused switch), 3 authentication, 130 cancelled.")
        .arg(Arg::new("data-root").long("data-root").value_name("DIR").value_parser(value_parser!(PathBuf)).help("Private storage [default: $XDG_DATA_HOME/harness or ~/.local/share/harness]"))
        .arg(Arg::new("workspace").long("workspace").value_name("DIR").required(true).value_parser(value_parser!(PathBuf)).help("Workspace to authorize (relative paths are resolved)"))
        .arg(Arg::new("provider").long("provider").value_parser(["openai", "anthropic", "deepseek"]).default_value("openai").help("Provider for a new session, or the switch target on continuation"))
        .arg(Arg::new("auth").long("auth").value_parser(["codex", "api-key"]).default_value("codex").help("OpenAI authentication; not applicable to --provider anthropic or deepseek"))
        .arg(Arg::new("model").long("model").value_name("MODEL").help("Default: gpt-6.1-sol (codex), gpt-5.5 (api-key), claude-sonnet-5-5 (anthropic), deepseek-flash (deepseek); saved model on continuation"))
        .arg(Arg::new("summary-model").long("summary-model").value_name("MODEL").help("Independent summary model on the same provider instance; enables automatic compaction"))
        .arg(Arg::new("compaction-strategy").long("compaction-strategy").value_parser(["rolling_summary", "observation_masking", "structured_state"]).default_value(harness::wiring::CREATION_COMPACTION_STRATEGY).help("Compaction strategy for a new session; a different value on continuation is refused"))
        .arg(Arg::new("switch-policy").long("switch-policy").value_parser(["reject", "omit-optional"]).default_value("reject").help("On continuation, whether a model switch may omit optional native state"))
        .arg(Arg::new("session").long("session").value_name("ID").value_parser(|s: &str| s.parse::<SessionId>()).help("Continue an existing durable session"))
        .arg(Arg::new("approve-read-only").long("approve-read-only").action(ArgAction::SetTrue).help("With a terminal, auto-approve only registry-declared read-only file tools"))
        .arg(Arg::new("prompt").value_name("PROMPT").required(true).help("Prompt text, or - to read stdin"))
}

/// The compaction strategy a client asks for, and whether the user named it.
///
/// Both parsers (`ask` and the plain-`harness` TUI share this one function) get
/// their value from `wiring::creation_compaction_strategy`, so a new session
/// records `observation_masking` by default while an omitted flag on a
/// continuation stays an omission, never an override.
pub(crate) fn strategy_selection(args: &ArgMatches) -> (StrategyId, bool) {
    let explicit = args.value_source("compaction-strategy") == Some(ValueSource::CommandLine);
    let strategy = args
        .get_one::<String>("compaction-strategy")
        .map(|name| StrategyId::from_str(name).expect("value parser"))
        .unwrap_or_else(harness::wiring::creation_compaction_strategy);
    (strategy, explicit)
}

pub fn run(args: &ArgMatches) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(_) => {
            eprintln!("error: runtime_setup_failed");
            return ExitCode::from(2);
        }
    };
    ExitCode::from(runtime.block_on(async {
        // Install before setup; second SIGINT bypasses all destructors and recovery
        // is owned by the next runtime open, not a pretend graceful shutdown.
        let mut signal =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()) {
                Ok(s) => s,
                Err(_) => {
                    eprintln!("error: signal_setup_failed");
                    return 2;
                }
            };
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let signals = tokio::spawn(async move {
            signal.recv().await;
            let _ = tx.send(());
            signal.recv().await;
            std::process::exit(130);
        });
        let result = wired(args, rx).await;
        signals.abort();
        result
    }))
}

async fn wired(args: &ArgMatches, interrupts: tokio::sync::mpsc::UnboundedReceiver<()>) -> u8 {
    let provider_name = args
        .get_one::<String>("provider")
        .map(String::as_str)
        .unwrap_or("openai");
    let explicit_provider = matches!(
        args.value_source("provider"),
        Some(ValueSource::CommandLine)
    );
    let explicit_auth = matches!(args.value_source("auth"), Some(ValueSource::CommandLine));
    if !harness::wiring::openai_auth_applies(provider_name) && explicit_auth {
        eprintln!("error: auth_not_applicable");
        return 2;
    }
    let codex = args
        .get_one::<String>("auth")
        .map(String::as_str)
        .unwrap_or("codex")
        == "codex";
    // The Codex hint belongs to the OpenAI subscription path only.
    let codex_hint = codex && provider_name == "openai";
    // Authentication profiles have separate state owners even though their wire
    // provider is the same. Never replay subscription state to the platform.
    let selected = harness::wiring::provider_instance(provider_name, codex);
    let data_root = match args
        .get_one::<PathBuf>("data-root")
        .cloned()
        .map(Ok)
        .unwrap_or_else(ask::default_data_root)
    {
        Ok(p) => p,
        Err(code) => {
            eprintln!("error: {code}");
            return 2;
        }
    };
    let wired = match harness::wiring::providers(selected, codex_hint) {
        Ok(wired) => wired,
        Err(error) => {
            eprintln!("error: {}", error.diagnostic);
            if error.codex_hint {
                eprintln!("run `codex login`");
            }
            return error.code;
        }
    };
    let model = args
        .get_one::<String>("model")
        .cloned()
        .unwrap_or_else(|| harness::wiring::default_model(provider_name, codex).into());
    let switch_policy = match args
        .get_one::<String>("switch-policy")
        .map(String::as_str)
        .unwrap_or("reject")
    {
        "omit-optional" => Portability::OmitOptional,
        _ => Portability::Reject,
    };
    let (compaction_strategy, explicit_strategy) = strategy_selection(args);
    let options = Options {
        data_root,
        workspace: args
            .get_one::<PathBuf>("workspace")
            .expect("required")
            .clone(),
        session: args.get_one::<SessionId>("session").cloned(),
        model: ModelRef {
            provider_instance: selected.into(),
            model,
        },
        explicit_model: args.contains_id("model"),
        explicit_provider,
        switch_policy,
        known_instances: wired.instances,
        summary_model: args.get_one::<String>("summary-model").map(|m| ModelRef {
            provider_instance: selected.into(),
            model: m.clone(),
        }),
        compaction_strategy,
        explicit_strategy,
        prompt: args.get_one::<String>("prompt").expect("required").clone(),
        approve_read_only: args.get_flag("approve-read-only"),
        driver: wired.driver,
        sandbox: Some(SandboxConfig::default()),
        codex: codex_hint,
    };
    let mut terminal = ControllingTerminal::open();
    ask::run(
        options,
        wired.provider,
        ask::Io {
            stdin: &mut io::stdin(),
            stdout: &mut io::stdout(),
            stderr: &mut io::stderr(),
            stderr_is_tty: io::stderr().is_terminal(),
            terminal: terminal.as_mut().map(|t| t as &mut dyn ask::Terminal),
            interrupts,
        },
    )
    .await
}

/// Both client parsers must resolve the same new-session creation default, and
/// an omitted flag must stay omitted so a continuation follows its recorded
/// strategy instead of overriding it.
#[cfg(test)]
mod strategy_selection_tests {
    use super::*;

    fn ask(args: &[&str]) -> ArgMatches {
        command()
            .try_get_matches_from(std::iter::once("ask").chain(args.iter().copied()))
            .expect("ask arguments")
    }

    fn plain_harness(args: &[&str]) -> ArgMatches {
        Command::new("harness")
            .args(crate::tui_cli::args())
            .try_get_matches_from(std::iter::once("harness").chain(args.iter().copied()))
            .expect("plain harness arguments")
    }

    #[test]
    fn ask_parser_defaults_a_new_session_to_observation_masking() {
        let matches = ask(&["--workspace", "/tmp/workspace", "hello"]);
        assert_eq!(
            strategy_selection(&matches),
            (StrategyId::ObservationMasking, false),
            "the ask parser no longer defaults to the creation strategy"
        );
        assert_eq!(
            harness::wiring::CREATION_COMPACTION_STRATEGY,
            "observation_masking"
        );
    }

    #[test]
    fn plain_harness_parser_defaults_a_new_session_to_observation_masking() {
        let matches = plain_harness(&[]);
        assert_eq!(
            strategy_selection(&matches),
            (StrategyId::ObservationMasking, false),
            "the plain-harness parser has its own, divergent default"
        );
    }

    #[test]
    fn both_parsers_detect_every_explicit_strategy_selection() {
        for (name, strategy) in [
            ("rolling_summary", StrategyId::RollingSummary),
            ("observation_masking", StrategyId::ObservationMasking),
            ("structured_state", StrategyId::StructuredState),
        ] {
            let ask = ask(&[
                "--workspace",
                "/tmp/workspace",
                "--compaction-strategy",
                name,
                "hi",
            ]);
            assert_eq!(
                strategy_selection(&ask),
                (strategy, true),
                "ask did not detect --compaction-strategy {name}"
            );
            let tui = plain_harness(&["--compaction-strategy", name]);
            assert_eq!(
                strategy_selection(&tui),
                (strategy, true),
                "plain harness did not detect --compaction-strategy {name}"
            );
        }
    }
}
