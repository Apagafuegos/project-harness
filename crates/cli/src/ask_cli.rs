//! Real adapter and process I/O wiring for the injected headless client.
use clap::{Arg, ArgAction, ArgMatches, Command, parser::ValueSource, value_parser};
use harness::ask::{self, ControllingTerminal, Options};
use harness_core::{
    context::ModelCatalog,
    runtime::DriverConfig,
    session::{ModelError, ModelErrorCode, ModelRef, Portability, SessionId},
    tools::SandboxConfig,
};
use harness_providers::{
    anthropic::{self, AnthropicMessages},
    openai::{self, OpenAiResponses, Profile},
    router::Router,
};
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
};

/// Provider instance for the Anthropic adapter. One account, one owner of
/// native state.
const ANTHROPIC: &str = "anthropic";
/// OpenAI's two authentication profiles are separate instances: subscription
/// state must never be replayed to the API-key endpoint, or the reverse.
const CODEX: &str = "openai-codex";
const API_KEY: &str = "openai-api-key";

pub fn command() -> Command {
    Command::new("ask")
        .about("Stream one durable turn; continue or switch models with --session")
        .long_about("Stream assistant text to stdout; diagnostics go to stderr. Tool approvals use /dev/tty, never stdin. Without a terminal every call is denied. Ctrl-C cancels durably; a second Ctrl-C exits immediately. A continuation that names another --provider or --model switches explicitly under --switch-policy: `reject` (default) refuses any optional native-state loss after printing the portability report, `omit-optional` drops it and prints the same report. Exit codes: 0 completed, 1 turn failure, 2 usage/configuration (including a refused switch), 3 authentication, 130 cancelled.")
        .arg(Arg::new("data-root").long("data-root").value_name("DIR").value_parser(value_parser!(PathBuf)).help("Private storage [default: $XDG_DATA_HOME/harness or ~/.local/share/harness]"))
        .arg(Arg::new("workspace").long("workspace").value_name("DIR").required(true).value_parser(value_parser!(PathBuf)).help("Workspace to authorize (relative paths are resolved)"))
        .arg(Arg::new("provider").long("provider").value_parser(["openai", "anthropic"]).default_value("openai").help("Provider for a new session, or the switch target on continuation"))
        .arg(Arg::new("auth").long("auth").value_parser(["codex", "api-key"]).default_value("codex").help("OpenAI authentication; not applicable to --provider anthropic"))
        .arg(Arg::new("model").long("model").value_name("MODEL").help("Default: gpt-6.1-sol (codex), gpt-5.5 (api-key), claude-sonnet-5-5 (anthropic); saved model on continuation"))
        .arg(Arg::new("summary-model").long("summary-model").value_name("MODEL").help("Independent summary model on the same provider instance; enables automatic compaction"))
        .arg(Arg::new("switch-policy").long("switch-policy").value_parser(["reject", "omit-optional"]).default_value("reject").help("On continuation, whether a model switch may omit optional native state"))
        .arg(Arg::new("session").long("session").value_name("ID").value_parser(|s: &str| s.parse::<SessionId>()).help("Continue an existing durable session"))
        .arg(Arg::new("approve-read-only").long("approve-read-only").action(ArgAction::SetTrue).help("With a terminal, auto-approve only registry-declared read-only file tools"))
        .arg(Arg::new("prompt").value_name("PROMPT").required(true).help("Prompt text, or - to read stdin"))
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

/// An adapter this process can actually serve, or one it skipped for lack of a
/// credential. Only the selected instance's failure ends the process.
fn skip_or_fail<T>(
    result: Result<T, ModelError>,
    selected: bool,
    codex: bool,
) -> Result<Option<T>, u8> {
    match result {
        Ok(adapter) => Ok(Some(adapter)),
        Err(_) if !selected => Ok(None),
        Err(error) => Err(provider_failure(error, codex)),
    }
}

/// Classify a setup failure without formatting the adapter's own message.
fn provider_failure(error: ModelError, codex: bool) -> u8 {
    if error.code == ModelErrorCode::Authentication {
        eprintln!("error: authentication");
        if codex {
            eprintln!("run `codex login`");
        }
        3
    } else {
        eprintln!("error: provider_setup_failed");
        2
    }
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
    if provider_name == "anthropic" && explicit_auth {
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
    let selected = match provider_name {
        "anthropic" => ANTHROPIC,
        _ if codex => CODEX,
        _ => API_KEY,
    };
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
    // Every instance whose credential exists is registered, so a session can
    // switch between providers mid-flight. The selected one is required.
    let mut router = Router::new();
    let mut catalog = ModelCatalog::new();
    for (instance, profile, result) in [
        (CODEX, Profile::ChatGptCodex, OpenAiResponses::codex_login()),
        (
            API_KEY,
            Profile::Platform,
            OpenAiResponses::platform_from_env(),
        ),
    ] {
        if let Some(adapter) = match skip_or_fail(result, selected == instance, codex_hint) {
            Ok(adapter) => adapter,
            Err(code) => return code,
        } {
            openai::extend_catalog(&mut catalog, profile, instance);
            router.register(instance, Arc::new(adapter));
        }
    }
    match skip_or_fail(
        AnthropicMessages::from_env(),
        selected == ANTHROPIC,
        codex_hint,
    ) {
        Ok(Some(adapter)) => {
            anthropic::extend_catalog(&mut catalog, ANTHROPIC);
            router.register(ANTHROPIC, Arc::new(adapter));
        }
        Ok(None) => {}
        Err(code) => return code,
    }
    let model =
        args.get_one::<String>("model")
            .cloned()
            .unwrap_or_else(|| match (provider_name, codex) {
                ("anthropic", _) => "claude-sonnet-5-5".into(),
                (_, true) => openai::LIVE_CHECK_MODEL.into(),
                _ => "gpt-5.5".into(),
            });
    let switch_policy = match args
        .get_one::<String>("switch-policy")
        .map(String::as_str)
        .unwrap_or("reject")
    {
        "omit-optional" => Portability::OmitOptional,
        _ => Portability::Reject,
    };
    let known_instances: Vec<String> = router.instances().map(str::to_owned).collect();
    let mut driver = DriverConfig::default();
    driver.runtime.context.catalog = catalog;
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
        known_instances,
        summary_model: args.get_one::<String>("summary-model").map(|m| ModelRef {
            provider_instance: selected.into(),
            model: m.clone(),
        }),
        prompt: args.get_one::<String>("prompt").expect("required").clone(),
        approve_read_only: args.get_flag("approve-read-only"),
        driver,
        sandbox: Some(SandboxConfig::default()),
        codex: codex_hint,
    };
    let mut terminal = ControllingTerminal::open();
    ask::run(
        options,
        Arc::new(router),
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
