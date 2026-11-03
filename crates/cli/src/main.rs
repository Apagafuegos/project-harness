//! Command-line entry point for the future Ratatui client.

mod ask_cli;
mod config;
mod doctor;
mod export;
mod sessions_cli;
mod tui_cli;

use std::{io, path::PathBuf, process::ExitCode, sync::Arc};

use clap::{Arg, ArgMatches, Command, value_parser};
use harness_core::{
    context::{ContextBuilder, ContextConfig, ModelLimits, SessionConfiguration},
    runtime::{ToolHost, ToolHostConfig},
    session::{
        DataRoot, DiagnosticKind, Digest, EntryId, Inspection, JournalEvent, JournalStore,
        ModelRef, SessionId, StoreError, Timestamp,
    },
    tools::{PolicySnapshot, SandboxConfig},
};
use serde_json::{Value, json};

fn command() -> Command {
    Command::new("harness")
        .version(env!("CARGO_PKG_VERSION"))
        .about("A small, inspectable, context-first agent harness")
        .long_about(
            "A small, inspectable, context-first agent harness.\n\n\
             Session storage, the runtime core and the context builder are \
             implemented in harness-core. This CLI provides headless OpenAI \
             turns with durable sessions and terminal approvals, read-only \
             context inspection, journal inspection, explicit torn-tail \
             repair and a host capability probe. With no subcommand, opens the \
             terminal chat in the current directory. Use harness ask for pipes.\n\n\
             Start with README.md and docs/README.md in the repository.",
        )
        .args(tui_cli::args())
        .args(config::globals())
        .subcommand(config::command())
        .subcommand(ask_cli::command())
        .subcommand(sessions_cli::command())
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
                .arg(data_root())
                .arg(session_id())
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
        .subcommand(
            Command::new("doctor")
                .about("Probe the host for the file-tool and sandbox capabilities as JSON")
                .long_about(
                    "Probe the host for the capabilities the Milestone 3 tools need, \
                     and print what was found as JSON: openat2 path resolution for \
                     the file tools, a Bubblewrap sandbox with an explicit user \
                     namespace, no network and a cleared environment for the shell, \
                     and a cgroup process limit through a systemd user scope. \
                     Anything that cannot be confirmed is reported unavailable, with \
                     the reason; the exit status is success whenever the report was \
                     printed. Host configuration is never changed: the doctor only \
                     uses a private temporary directory, probe sandboxes and one \
                     transient user scope.",
                )
                .arg(
                    Arg::new("workspace")
                        .long("workspace")
                        .value_name("DIR")
                        .value_parser(absolute)
                        .requires("data-root")
                        .help("Report real tool availability for this absolute workspace"),
                )
                .arg(
                    Arg::new("data-root")
                        .long("data-root")
                        .value_name("DIR")
                        .value_parser(absolute)
                        .requires("workspace")
                        .help("Absolute private data root for the workspace tools report"),
                )
                .arg(
                    Arg::new("bwrap")
                        .long("bwrap")
                        .value_name("PATH")
                        .default_value(doctor::DEFAULT_BWRAP)
                        .value_parser(absolute)
                        .help("Absolute path of bwrap; PATH is never searched"),
                )
                .arg(
                    Arg::new("systemd-run")
                        .long("systemd-run")
                        .value_name("PATH")
                        .default_value(doctor::DEFAULT_SYSTEMD_RUN)
                        .value_parser(absolute)
                        .help("Absolute path of systemd-run"),
                ),
        )
        .subcommand(
            Command::new("journal")
                .about("Inspect a session journal, or repair its torn tail")
                .subcommand_required(true)
                .subcommand(
                    Command::new("inspect")
                        .about("Print the journal's inspection as JSON")
                        .long_about(
                            "Print the journal's inspection as JSON: valid and total \
                             bytes, the whole file's SHA-256 fingerprint, the number of \
                             valid records of each format version, the first rejected \
                             record, whether that is a repairable torn tail, and where \
                             the valid prefix ends. Read-only, under a \
                             shared lock; a session held by a running engine is \
                             refused at once.",
                        )
                        .arg(data_root())
                        .arg(session_id()),
                )
                .subcommand(
                    Command::new("repair")
                        .about("Quarantine an inspected torn tail and truncate it")
                        .long_about(
                            "Quarantine the torn tail of a journal into an artifact, \
                             truncate it, and record tail_repaired. Only the exact \
                             file you inspected is repaired: pass the fingerprint and \
                             size that `harness journal inspect` printed. A changed \
                             file, a complete corrupt record or a missing header is \
                             refused, and nothing is chosen for you. Recovery of an \
                             interrupted turn happens when an engine next opens the \
                             session.",
                        )
                        .arg(data_root())
                        .arg(session_id())
                        .arg(
                            Arg::new("fingerprint")
                                .long("fingerprint")
                                .value_name("SHA256")
                                .required(true)
                                .value_parser(|value: &str| value.parse::<Digest>())
                                .help("Inspected fingerprint of the whole file"),
                        )
                        .arg(
                            Arg::new("size")
                                .long("size")
                                .value_name("BYTES")
                                .required(true)
                                .value_parser(value_parser!(u64))
                                .help("Inspected size of the whole file"),
                        ),
                ),
        )
}

fn data_root() -> Arg {
    Arg::new("data-root")
        .required(true)
        .value_parser(value_parser!(PathBuf))
        .help("Absolute path of the private data root")
}

fn session_id() -> Arg {
    Arg::new("session-id")
        .required(true)
        .value_parser(|value: &str| value.parse::<SessionId>())
        .help("Session UUIDv7")
}

fn absolute(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("expected an absolute path".into())
    }
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
        Some(("ask", arguments)) => ask_cli::run(arguments),
        Some(("config", arguments)) => config::run(arguments),
        Some(("sessions", arguments)) => sessions_cli::run(arguments),
        Some(("context", arguments)) => print("harness context", context(arguments)),
        Some(("doctor", arguments)) => print("harness doctor", run_doctor(arguments)),
        Some(("journal", journal)) => match journal.subcommand() {
            Some(("inspect", arguments)) => print("harness journal inspect", inspect(arguments)),
            Some(("repair", arguments)) => print("harness journal repair", repair(arguments)),
            _ => unreachable!("clap requires a journal subcommand"),
        },
        _ => tui_cli::run(&matches),
    }
}

fn print(command: &str, result: Result<String, String>) -> ExitCode {
    match result {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{command}: {message}");
            ExitCode::FAILURE
        }
    }
}

fn arguments(arguments: &ArgMatches) -> (&PathBuf, &SessionId) {
    (
        arguments.get_one::<PathBuf>("data-root").expect("required"),
        arguments
            .get_one::<SessionId>("session-id")
            .expect("required"),
    )
}

fn open_root(path: &PathBuf) -> Result<DataRoot, String> {
    DataRoot::open(path, false)
        .map_err(|error| format!("cannot open data root {}: {error}", path.display()))
}

/// Explain a storage refusal in the user's terms.
fn storage_error(session: &SessionId, error: StoreError) -> String {
    match error {
        StoreError::Locked => format!(
            "session {session} is locked by a running engine; this command does not wait. \
             Stop the engine first"
        ),
        StoreError::Io(error) if error.kind() == io::ErrorKind::NotFound => {
            format!("session {session} not found")
        }
        StoreError::StaleRepair => format!(
            "the journal of session {session} changed since it was inspected \
             (stale_repair); nothing was repaired. Inspect it again and confirm the new \
             fingerprint and size"
        ),
        StoreError::NotRepairable => format!(
            "the journal of session {session} has no repairable torn tail \
             (tail_not_repairable); nothing was changed. A complete corrupt record or a \
             missing header is never repaired"
        ),
        error => format!("session {session}: {error}"),
    }
}

/// Print `JournalStore::inspect` as JSON.
fn inspect(arguments: &ArgMatches) -> Result<String, String> {
    let (path, session) = self::arguments(arguments);
    let root = open_root(path)?;
    let inspection =
        JournalStore::inspect(&root, session).map_err(|error| storage_error(session, error))?;
    serde_json::to_string_pretty(&inspection_json(session, &inspection))
        .map_err(|error| error.to_string())
}

fn inspection_json(session: &SessionId, inspection: &Inspection) -> Value {
    let diagnostic = inspection.diagnostic.as_ref();
    let repairable = inspection.snapshot.is_some()
        && diagnostic.is_some_and(|d| d.kind == DiagnosticKind::TornTail);
    let prefix = inspection.snapshot.as_ref().map(|snapshot| {
        let unresolved: Vec<_> = snapshot
            .unresolved_invocations()
            .iter()
            .map(|v| {
                json!({
                    "invocation_id": v.intent.invocation_id,
                    "turn_id": v.intent.turn_id,
                    "uncertainty_recorded": v.uncertainty_recorded,
                })
            })
            .collect();
        json!({
            "sequence": snapshot.sequence(),
            "head": snapshot.head(),
            "state": snapshot.state(),
            "active_turn": snapshot.active_turn(),
            "requires_recovery": snapshot.requires_recovery(),
            "unresolved_invocations": unresolved,
        })
    });
    json!({
        "session_id": session,
        "file_bytes": inspection.file_bytes,
        "valid_bytes": inspection.valid_bytes,
        "fingerprint": inspection.fingerprint,
        "versions": inspection.versions,
        "diagnostic": diagnostic.map(|d| json!({
            "offset": d.offset,
            "kind": match d.kind {
                DiagnosticKind::TornTail => "torn_tail",
                DiagnosticKind::Corrupt => "corrupt",
                DiagnosticKind::UnsupportedVersion => "unsupported_version",
                DiagnosticKind::Artifact => "artifact",
                DiagnosticKind::Limit => "limit",
            },
            "code": d.code,
        })),
        "repairable": repairable,
        "quarantine_bytes": repairable.then(|| inspection.file_bytes - inspection.valid_bytes),
        "valid_prefix": prefix,
    })
}

/// Repair the inspected torn tail and print the quarantine artifact.
fn repair(arguments: &ArgMatches) -> Result<String, String> {
    let (path, session) = self::arguments(arguments);
    let fingerprint = arguments
        .get_one::<Digest>("fingerprint")
        .expect("required");
    let size = *arguments.get_one::<u64>("size").expect("required");
    let root = Arc::new(open_root(path)?);
    let store =
        JournalStore::repair_inspected_tail(root, session, fingerprint, size, Timestamp::now())
            .map_err(|error| storage_error(session, error))?;
    let snapshot = store.snapshot();
    let repair = snapshot
        .events()
        .rev()
        .find_map(|(_, _, event)| match event {
            JournalEvent::TailRepaired(repair) => Some(repair.clone()),
            _ => None,
        })
        .ok_or("the repair record is missing")?;
    serde_json::to_string_pretty(&json!({
        "session_id": session,
        "quarantine_artifact": repair.quarantine_artifact,
        "original_size": repair.original_size,
        "valid_bytes": repair.valid_bytes,
        "sequence": snapshot.sequence(),
        "requires_recovery": snapshot.requires_recovery(),
    }))
    .map_err(|error| error.to_string())
}

/// Probe the host and print the report.
fn run_doctor(arguments: &ArgMatches) -> Result<String, String> {
    let programs = doctor::Programs {
        bwrap: arguments
            .get_one::<PathBuf>("bwrap")
            .expect("default")
            .clone(),
        systemd_run: arguments
            .get_one::<PathBuf>("systemd-run")
            .expect("default")
            .clone(),
    };
    let mut report = doctor::report(&programs)?;
    if let Some(workspace_root) = arguments.get_one::<PathBuf>("workspace") {
        let bytes = serde_json::to_vec(&json!({
            "policy_revision":"doctor", "enabled_tools":["read","write","edit","bash"],
            "workspace_read":"allow", "workspace_mutation":"ask", "process":"ask",
            "network":"deny", "protected_paths":[], "allowances":[]
        }))
        .map_err(|error| error.to_string())?;
        let policy = PolicySnapshot::parse(
            harness_core::session::ArtifactRef {
                sha256: Digest::of(&bytes),
                bytes: bytes.len() as u64,
            },
            &bytes,
        )
        .map_err(|error| error.to_string())?;
        let (_, tools) = ToolHost::assemble(
            &ToolHostConfig {
                workspace_root: workspace_root.clone(),
                data_root: arguments
                    .get_one::<PathBuf>("data-root")
                    .expect("required with workspace")
                    .clone(),
                sandbox: Some(SandboxConfig {
                    bwrap: programs.bwrap,
                    systemd_run: programs.systemd_run,
                    ..SandboxConfig::default()
                }),
            },
            &policy,
        );
        report["tools"] = serde_json::to_value(tools).map_err(|error| error.to_string())?;
    }
    serde_json::to_string_pretty(&report).map_err(|error| error.to_string())
}

/// Build the report for the requested head and print it as pretty JSON.
fn context(arguments: &ArgMatches) -> Result<String, String> {
    let (path, session) = self::arguments(arguments);
    let root = open_root(path)?;
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
