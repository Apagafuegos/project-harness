//! Real adapter and process I/O wiring for the injected headless client.
use clap::{Arg, ArgAction, ArgMatches, Command, value_parser};
use harness::ask::{self, ControllingTerminal, Options};
use harness_core::{
    runtime::DriverConfig,
    session::{ModelErrorCode, ModelRef, SessionId},
    tools::SandboxConfig,
};
use harness_providers::openai::{self, OpenAiResponses, Profile};
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
};

pub fn command() -> Command {
    Command::new("ask")
        .about("Stream one durable OpenAI turn; continue it with --session")
        .long_about("Stream assistant text to stdout; diagnostics go to stderr. Tool approvals use /dev/tty, never stdin. Without a terminal every call is denied. Ctrl-C cancels durably; a second Ctrl-C exits immediately. Exit codes: 0 completed, 1 turn failure, 2 usage/configuration, 3 authentication, 130 cancelled.")
        .arg(Arg::new("data-root").long("data-root").value_name("DIR").value_parser(value_parser!(PathBuf)).help("Private storage [default: $XDG_DATA_HOME/harness or ~/.local/share/harness]"))
        .arg(Arg::new("workspace").long("workspace").value_name("DIR").required(true).value_parser(value_parser!(PathBuf)).help("Workspace to authorize (relative paths are resolved)"))
        .arg(Arg::new("provider").long("provider").value_parser(["openai"]).default_value("openai"))
        .arg(Arg::new("auth").long("auth").value_parser(["codex", "api-key"]).default_value("codex"))
        .arg(Arg::new("model").long("model").value_name("MODEL").help("Default: gpt-6.1-sol (codex), gpt-5.5 (api-key); saved model on continuation"))
        .arg(Arg::new("summary-model").long("summary-model").value_name("MODEL").help("Independent summary model; enables automatic compaction"))
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

async fn wired(args: &ArgMatches, interrupts: tokio::sync::mpsc::UnboundedReceiver<()>) -> u8 {
    let codex = args.get_one::<String>("auth").expect("default") == "codex";
    let profile = if codex {
        Profile::ChatGptCodex
    } else {
        Profile::Platform
    };
    // Authentication profiles have separate state owners even though their wire
    // provider is the same. Never replay subscription state to the platform.
    let instance = if codex {
        "openai-codex"
    } else {
        "openai-api-key"
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
    let provider = match if codex {
        OpenAiResponses::codex_login()
    } else {
        OpenAiResponses::platform_from_env()
    } {
        Ok(p) => Arc::new(p),
        Err(e) => {
            // Do not format even an adapter error: only its typed classification.
            if e.code == ModelErrorCode::Authentication {
                eprintln!("error: authentication");
                if codex {
                    eprintln!("run `codex login`");
                }
                return 3;
            }
            eprintln!("error: provider_setup_failed");
            return 2;
        }
    };
    let model = args.get_one::<String>("model").cloned().unwrap_or_else(|| {
        if codex {
            openai::LIVE_CHECK_MODEL.into()
        } else {
            "gpt-5.5".into()
        }
    });
    let mut driver = DriverConfig::default();
    driver.runtime.context.catalog = openai::catalog(profile, instance);
    let options = Options {
        data_root,
        workspace: args
            .get_one::<PathBuf>("workspace")
            .expect("required")
            .clone(),
        session: args.get_one::<SessionId>("session").cloned(),
        model: ModelRef {
            provider_instance: instance.into(),
            model,
        },
        explicit_model: args.contains_id("model"),
        summary_model: args.get_one::<String>("summary-model").map(|m| ModelRef {
            provider_instance: instance.into(),
            model: m.clone(),
        }),
        prompt: args.get_one::<String>("prompt").expect("required").clone(),
        approve_read_only: args.get_flag("approve-read-only"),
        driver,
        sandbox: Some(SandboxConfig::default()),
        codex,
    };
    let mut terminal = ControllingTerminal::open();
    ask::run(
        options,
        provider,
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
