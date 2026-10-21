//! Command-line entry point for the future Ratatui client.

use std::{io, path::PathBuf, process::ExitCode};

use clap::{Arg, ArgMatches, Command, value_parser};
use harness_core::{
    context::{ContextBuilder, ContextConfig, ModelLimits, SessionConfiguration},
    session::{DataRoot, EntryId, JournalStore, ModelRef, SessionId, StoreError},
};

fn command() -> Command {
    Command::new("harness")
        .version(env!("CARGO_PKG_VERSION"))
        .about("A small, inspectable, context-first agent harness")
        .long_about(
            "A small, inspectable, context-first agent harness.\n\n\
             Session storage, the runtime core and the context builder are \
             implemented in harness-core. Model providers, tools and the \
             terminal UI are not implemented yet. This CLI provides read-only \
             context inspection.\n\n\
             Start with README.md and docs/README.md in the repository.",
        )
        .subcommand(
            Command::new("context")
                .about("Print the context report of a stored session as JSON")
                .long_about(
                    "Print the context report of a stored session as JSON.\n\n\
                     Read-only: the journal is inspected under a shared lock and \
                     nothing is written. A session held by a running engine is \
                     refused at once; ask that engine instead. Without \
                     --context-tokens the model's limit is unknown, so the \
                     budget is reported as unknown.",
                )
                .arg(
                    Arg::new("data-root")
                        .required(true)
                        .value_parser(value_parser!(PathBuf))
                        .help("Absolute path of the private data root"),
                )
                .arg(
                    Arg::new("session-id")
                        .required(true)
                        .value_parser(|value: &str| value.parse::<SessionId>())
                        .help("Session UUIDv7"),
                )
                .arg(
                    Arg::new("head")
                        .long("head")
                        .value_name("ID")
                        .value_parser(|value: &str| value.parse::<EntryId>())
                        .help("Entry to project from [default: the session's head]"),
                )
                .arg(
                    Arg::new("model")
                        .long("model")
                        .value_name("PROVIDER/MODEL")
                        .value_parser(model)
                        .help("Preview for another model instead of the head's"),
                )
                .arg(
                    Arg::new("context-tokens")
                        .long("context-tokens")
                        .value_name("N")
                        .value_parser(value_parser!(u32))
                        .help("Context limit of the projected model"),
                ),
        )
}

fn model(value: &str) -> Result<ModelRef, String> {
    match value.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => Ok(ModelRef {
            provider_instance: provider.into(),
            model: model.into(),
        }),
        _ => Err("expected PROVIDER/MODEL".into()),
    }
}

fn main() -> ExitCode {
    let mut command = command();
    let matches = command.get_matches_mut();
    match matches.subcommand() {
        Some(("context", arguments)) => match context(arguments) {
            Ok(report) => {
                println!("{report}");
                ExitCode::SUCCESS
            }
            Err(message) => {
                eprintln!("harness context: {message}");
                ExitCode::FAILURE
            }
        },
        _ => match command.print_long_help() {
            Ok(()) => {
                println!();
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("harness: {error}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Build the report for the requested head and print it as pretty JSON.
fn context(arguments: &ArgMatches) -> Result<String, String> {
    let path = arguments.get_one::<PathBuf>("data-root").expect("required");
    let session = arguments
        .get_one::<SessionId>("session-id")
        .expect("required");
    let root = DataRoot::open(path, false)
        .map_err(|error| format!("cannot open data root {}: {error}", path.display()))?;
    let inspection = match JournalStore::inspect(&root, session) {
        Ok(inspection) => inspection,
        Err(StoreError::Locked) => {
            return Err(format!(
                "session {session} is locked by a running engine; this command does not \
                 wait. Stop the engine, or inspect through its SessionHandle::inspect_context"
            ));
        }
        Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Err(format!("session {session} not found"));
        }
        Err(error) => return Err(format!("cannot inspect session {session}: {error}")),
    };
    let snapshot = inspection
        .snapshot
        .ok_or_else(|| format!("session {session} has no valid header"))?;
    if let Some(diagnostic) = &inspection.diagnostic {
        eprintln!(
            "harness context: warning: journal rejected at byte {} ({:?}: {}); reporting \
             its valid prefix through sequence {}",
            diagnostic.offset,
            diagnostic.kind,
            diagnostic.code,
            snapshot.sequence()
        );
    }
    let head = arguments
        .get_one::<EntryId>("head")
        .or(snapshot.head())
        .cloned();
    let preview = arguments.get_one::<ModelRef>("model");
    let configuration = SessionConfiguration::load(&root, snapshot.configuration())
        .map_err(|error| format!("cannot read the session configuration: {error}"))?;
    let mut config = ContextConfig::default();
    if let Some(tokens) = arguments.get_one::<u32>("context-tokens") {
        let model = match preview {
            Some(model) => model.clone(),
            None => snapshot
                .model_at(head.as_ref())
                .map_err(|_| "unknown head".to_string())?
                .clone(),
        };
        config.catalog.insert(
            model,
            ModelLimits {
                context_tokens: Some(*tokens),
                max_output_tokens: None,
            },
        );
    }
    let built = ContextBuilder::new(&config, &configuration)
        .build(&snapshot, head.as_ref(), preview)
        .map_err(|error| format!("cannot build context: {error}"))?;
    serde_json::to_string_pretty(&built.report).map_err(|error| error.to_string())
}
