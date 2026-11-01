//! Process wiring for plain `harness`; all session assembly stays in wiring.
use clap::{Arg, ArgMatches, parser::ValueSource};
use harness::{
    tui::{
        runtime::{self, Signal, TerminalGuard},
        runtime_nav::SessionCommands,
    },
    wiring::{self, SessionOptions},
};
use harness_core::{
    context::StrategyId,
    session::{ModelRef, Portability, SessionId},
    tools::SandboxConfig,
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    process::ExitCode,
    str::FromStr,
};

pub fn args() -> Vec<Arg> {
    super::ask_cli::command()
        .get_arguments()
        .filter(|arg| {
            matches!(
                arg.get_id().as_str(),
                "data-root"
                    | "provider"
                    | "auth"
                    | "model"
                    | "summary-model"
                    | "compaction-strategy"
                    | "session"
            )
        })
        .cloned()
        .collect()
}

pub fn run(args: &ArgMatches) -> ExitCode {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("error: TUI requires a terminal; use harness ask");
        return ExitCode::from(2);
    }
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| (2, "runtime_setup_failed".into()))
        .and_then(|runtime| runtime.block_on(wired(args)));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, message)) => {
            eprintln!("error: {}", message.replace(['\n', '\r'], " "));
            ExitCode::from(code)
        }
    }
}

async fn wired(args: &ArgMatches) -> Result<(), wiring::Failure> {
    // Install before opening storage/entering raw mode. Signals remain queued
    // during setup and restore the terminal before durable shutdown in drive.
    use tokio::signal::unix::{SignalKind, signal};
    let mut term =
        signal(SignalKind::terminate()).map_err(|_| (2, "signal_setup_failed".into()))?;
    let mut hup = signal(SignalKind::hangup()).map_err(|_| (2, "signal_setup_failed".into()))?;
    let mut interrupt =
        signal(SignalKind::interrupt()).map_err(|_| (2, "signal_setup_failed".into()))?;
    let signals = async_stream::stream! {
        loop {
            let value = tokio::select! {
                _ = term.recv() => Signal::Terminate,
                _ = hup.recv() => Signal::Terminate,
                _ = interrupt.recv() => Signal::Interrupt,
            };
            yield value;
        }
    };
    let provider_name = args
        .get_one::<String>("provider")
        .expect("default")
        .as_str();
    let codex = args.get_one::<String>("auth").expect("default") == "codex";
    if !wiring::openai_auth_applies(provider_name)
        && args.value_source("auth") == Some(ValueSource::CommandLine)
    {
        return Err((2, "auth_not_applicable".into()));
    }
    let selected = wiring::provider_instance(provider_name, codex);
    let providers = providers(selected, codex && provider_name == "openai")?;
    let model = ModelRef {
        provider_instance: selected.into(),
        model: args
            .get_one::<String>("model")
            .cloned()
            .unwrap_or_else(|| wiring::default_model(provider_name, codex).into()),
    };
    let options = SessionOptions {
        data_root: args
            .get_one::<PathBuf>("data-root")
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| wiring::default_data_root().map_err(|code| (2, code.into())))?,
        workspace: std::env::current_dir()
            .and_then(std::fs::canonicalize)
            .map_err(|_| (2, "invalid_workspace".into()))?,
        session: args.get_one::<SessionId>("session").cloned(),
        summary_model: args
            .get_one::<String>("summary-model")
            .map(|model| ModelRef {
                provider_instance: selected.into(),
                model: model.clone(),
            }),
        compaction_strategy: args
            .get_one::<String>("compaction-strategy")
            .map(|strategy| StrategyId::from_str(strategy).expect("value parser"))
            .unwrap_or_default(),
        explicit_strategy: args.value_source("compaction-strategy")
            == Some(ValueSource::CommandLine),
        model: model.clone(),
        explicit_model: args.value_source("model") == Some(ValueSource::CommandLine),
        explicit_provider: args.value_source("provider") == Some(ValueSource::CommandLine),
        switch_policy: Portability::Reject,
        known_instances: providers.instances,
        driver: providers.driver,
        sandbox: Some(SandboxConfig::default()),
    };
    let opened = wiring::open_session(&options, providers.provider.clone()).await?;
    let warnings = opened.warnings.clone();
    let commands = SessionCommands::new(opened, options, providers.provider);
    let result = async {
        let mut guard = TerminalGuard::enter().map_err(|_| (2, "terminal_setup_failed".into()))?;
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
            .map_err(|_| (1, "terminal_io_failure".into()))?;
        let mut app = commands.app();
        if !warnings.is_empty() {
            app.set_error(warnings.join("; "));
        }
        let subscriptions = commands.subscriptions();
        #[cfg(all(feature = "test-provider", debug_assertions))]
        if std::env::var_os("HARNESS_TUI_TEST_PANIC").is_some() {
            panic!("forced TUI panic");
        }
        let events = crossterm::event::EventStream::new();
        // Debug-only PTY fault injection: return an I/O error through drive,
        // rather than unwinding or using a signal/ordinary quit path.
        #[cfg(all(feature = "test-provider", debug_assertions))]
        let events = futures_util::StreamExt::chain(
            futures_util::stream::iter(
                std::env::var_os("HARNESS_TUI_TEST_ERROR")
                    .map(|_| Err(io::Error::other("forced TUI input error"))),
            ),
            events,
        );
        runtime::drive(
            &mut terminal,
            &mut app,
            events,
            subscriptions,
            signals,
            &commands,
            || guard.restore(),
        )
        .await
        .map_err(|_| (1, "terminal_io_failure".into()))?;
        Ok(())
    }
    .await;
    // Also runs after terminal errors. shutdown owns cancellation and joins the
    // tool/storage workers; no process exit can race journal publication.
    commands.shutdown().await;
    result
}

fn providers(selected: &str, codex_hint: bool) -> Result<wiring::Providers, wiring::Failure> {
    #[cfg(all(feature = "test-provider", debug_assertions))]
    if std::env::var_os("HARNESS_TUI_TEST_PROVIDER").is_some() {
        use harness_core::{
            context::ModelLimits,
            model::{
                ProviderEvent,
                scripted::{Script, ScriptedProvider, Step},
            },
            runtime::DriverConfig,
            session::FinishReason,
        };
        use std::sync::Arc;
        let mut driver = DriverConfig::default();
        driver.runtime.context.catalog.insert(
            ModelRef {
                provider_instance: selected.into(),
                model: wiring::default_model("openai", true).into(),
            },
            ModelLimits {
                context_tokens: Some(128_000),
                max_output_tokens: None,
            },
        );
        let end = if std::env::var("HARNESS_TUI_TEST_PROVIDER").as_deref() == Ok("stall") {
            Step::Stall
        } else {
            Step::completed(FinishReason::Stop)
        };
        return Ok(wiring::Providers {
            provider: Arc::new(ScriptedProvider::new([Script::new([
                Step::Event(ProviderEvent::Started),
                Step::text(0, "pong"),
                end,
            ])])),
            driver,
            instances: vec![selected.into()],
        });
    }
    wiring::providers(selected, codex_hint).map_err(|error| {
        if error.codex_hint {
            eprintln!("run `codex login`");
        }
        (error.code, error.diagnostic.into())
    })
}
