//! `harness sessions`: the read-only listing plus export, delete and collection.
//!
//! The legacy command with no subcommand keeps its exact JSON listing. Every
//! destructive or exporting subcommand resolves the same private data root as
//! `ask`, refuses with a stable code on stderr, and never waits on a journal
//! lock that a running engine holds.
use crate::{config, export};
use clap::{Arg, ArgAction, ArgMatches, Command, value_parser};
use harness_core::session::{
    ARTIFACT_GRACE, DataRoot, GcOptions, GcReport, MaintenanceError, SessionId, StoreError,
};
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

/// A failure with the exit status the command contract assigns to it.
pub(super) enum Failure {
    /// A stable refusal code, or a usage message: exit status 2.
    Refusal(String),
    /// A storage or I/O failure: exit status 1.
    Storage(String),
}

pub fn command() -> Command {
    Command::new("sessions")
        .about("List sessions, then export, delete or collect artifact space")
        .long_about(
            "List durable sessions as JSON, export one as Markdown or as a data \
             root with its artifacts, delete one journal, or collect artifact \
             space no readable journal references.\n\n\
             With no subcommand this prints the read-only JSON catalog, exactly \
             as before. Export holds the shared catalog lock; deletion takes the \
             exclusive writer lock and removes only the journal. It never removes \
             an artifact, because artifacts are shared and content-addressed: \
             `harness sessions gc` reclaims those, and only when nothing readable \
             references them and they are older than 24 hours. Deletion is \
             irreversible.",
        )
        .arg(
            Arg::new("data-root")
                .long("data-root")
                .value_name("DIR")
                .value_parser(value_parser!(PathBuf))
                .global(true)
                .help(
                    "Private storage [default: $XDG_DATA_HOME/harness or ~/.local/share/harness]",
                ),
        )
        .subcommand(
            Command::new("list")
                .about("List durable sessions as JSON, exactly as without a subcommand"),
        )
        .subcommand(export::command())
        .subcommand(
            Command::new("delete")
                .about("Delete one journal; artifacts are left to `gc` (irreversible)")
                .long_about(
                    "Delete one session journal under the exclusive writer lock. \
                     A session a running engine holds is refused with \
                     `session_locked`. Without a terminal, `--yes` is required.\n\n\
                     Only the journal is removed. Artifacts are shared and \
                     content-addressed across sessions, so deletion never removes \
                     them; run `harness sessions gc` to reclaim artifact space. \
                     Deletion is irreversible.",
                )
                .arg(session_id())
                .arg(
                    Arg::new("yes")
                        .long("yes")
                        .action(ArgAction::SetTrue)
                        .help("Delete without asking, required without a terminal"),
                ),
        )
        .subcommand(
            Command::new("gc")
                .about("Remove artifacts no readable journal references and older than 24 hours")
                .long_about(
                    "Remove artifact bytes that no readable journal references and \
                     whose modification time is more than 24 hours old.\n\n\
                     The grace window is what makes this safe: a writer stores an \
                     artifact before the journal commit that references it, and a \
                     dedupe hit refreshes the modification time, so an in-flight \
                     reuse is never old. Any journal that cannot be locked and \
                     validated refuses the whole run rather than risk deleting \
                     bytes it might need. `--dry-run` lists the candidates and \
                     their total size without removing anything; a real run \
                     without a terminal needs `--yes`.",
                )
                .arg(
                    Arg::new("dry-run")
                        .long("dry-run")
                        .action(ArgAction::SetTrue)
                        .help("List the candidates and their total size, remove nothing"),
                )
                .arg(
                    Arg::new("yes")
                        .long("yes")
                        .action(ArgAction::SetTrue)
                        .help("Remove without asking, required without a terminal"),
                ),
        )
}

fn session_id() -> Arg {
    Arg::new("session-id")
        .value_name("ID")
        .required(true)
        .value_parser(|value: &str| value.parse::<SessionId>())
        .help("Session UUIDv7")
}

/// Map a maintenance refusal to its exit status. The error's own text is the
/// stable code clients print and scripts match.
pub(super) fn maintenance(error: MaintenanceError) -> Failure {
    match error {
        MaintenanceError::Storage(error) => Failure::Storage(error.to_string()),
        refusal => Failure::Refusal(refusal.to_string()),
    }
}

/// Resolve `--data-root`, then the user config's `data_root`, then the default.
pub(super) fn data_root(arguments: &ArgMatches) -> Result<PathBuf, Failure> {
    if let Some(path) = arguments.get_one::<PathBuf>("data-root") {
        return Ok(path.clone());
    }
    config::resolve(arguments)
        .map(|settings| settings.data_root())
        .map_err(Failure::Refusal)
}

/// Open an existing private root. Nothing here creates storage.
pub(super) fn open(path: &Path) -> Result<DataRoot, Failure> {
    DataRoot::open(path, false).map_err(|error| match error {
        StoreError::Io(error) if error.kind() == io::ErrorKind::NotFound => Failure::Storage(
            format!("no private data root at {}; nothing to do", path.display()),
        ),
        error => Failure::Storage(format!("cannot open data root {}: {error}", path.display())),
    })
}

/// Ask a yes/no question on the terminal. Callers check for a terminal first.
fn confirm(prompt: &str) -> Result<bool, Failure> {
    eprint!("{prompt} [y/N] ");
    io::stderr().flush().ok();
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| Failure::Storage(format!("cannot read the answer: {error}")))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

pub(super) fn run(arguments: &ArgMatches) -> ExitCode {
    let result = match arguments.subcommand() {
        Some(("list", arguments)) => list(arguments),
        Some(("export", arguments)) => export::run(arguments),
        Some(("delete", arguments)) => delete(arguments),
        Some(("gc", arguments)) => gc(arguments),
        _ => list(arguments),
    };
    match result {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(Failure::Refusal(code)) => {
            eprintln!("{code}");
            ExitCode::from(2)
        }
        Err(Failure::Storage(message)) => {
            eprintln!("harness sessions: {message}");
            ExitCode::FAILURE
        }
    }
}

/// The legacy listing: a pretty JSON array, and `[]` for a missing root.
fn list(arguments: &ArgMatches) -> Result<String, Failure> {
    let path = data_root(arguments)?;
    let root = match DataRoot::open(&path, false) {
        Ok(root) => root,
        Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok("[]".into());
        }
        Err(error) => {
            return Err(Failure::Storage(format!("cannot open data root: {error}")));
        }
    };
    let entries = root
        .sessions()
        .map_err(|error| Failure::Storage(error.to_string()))?;
    serde_json::to_string_pretty(&entries).map_err(|error| Failure::Storage(error.to_string()))
}

fn delete(arguments: &ArgMatches) -> Result<String, Failure> {
    let path = data_root(arguments)?;
    let id = arguments
        .get_one::<SessionId>("session-id")
        .expect("required");
    let root = open(&path)?;
    // Reading the title also refuses a locked session before any question, and
    // validates the journal while the shared lock is held.
    let title = root.session_title(id).map_err(maintenance)?;
    if !arguments.get_flag("yes") {
        if !io::stdin().is_terminal() {
            return Err(Failure::Refusal("confirmation_required".into()));
        }
        let label = title.unwrap_or_else(|| "(no message)".into());
        if !confirm(&format!("Delete session {label} ({id})?"))? {
            return Ok(format!("cancelled: session {id} was not deleted"));
        }
    }
    root.delete_session(id).map_err(maintenance)?;
    Ok(format!(
        "deleted session {id}\nrun 'harness sessions gc' to reclaim artifact space"
    ))
}

fn gc(arguments: &ArgMatches) -> Result<String, Failure> {
    let path = data_root(arguments)?;
    let root = open(&path)?;
    let dry_run = arguments.get_flag("dry-run");
    if !dry_run && !arguments.get_flag("yes") && !io::stdin().is_terminal() {
        return Err(Failure::Refusal("confirmation_required".into()));
    }
    if !dry_run
        && !arguments.get_flag("yes")
        && !confirm("Remove unreferenced artifacts older than 24 hours?")?
    {
        return Ok("cancelled: nothing was removed".into());
    }
    let report = root
        .collect_artifacts(&GcOptions {
            dry_run,
            grace: ARTIFACT_GRACE,
        })
        .map_err(maintenance)?;
    Ok(report_text(&report))
}

/// `1 artifact`, `2 artifacts`: a report must read like a sentence.
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

fn report_text(report: &GcReport) -> String {
    let mut text = format!(
        "{} {}, {}; scanned {}",
        if report.dry_run {
            "would remove"
        } else {
            "removed"
        },
        count(report.candidates.len(), "artifact"),
        count(report.bytes() as usize, "byte"),
        count(report.journals, "journal")
    );
    for candidate in &report.candidates {
        text.push_str(&format!(
            "\n  {} {} bytes",
            candidate.sha256, candidate.bytes
        ));
    }
    if report.dry_run && !report.candidates.is_empty() {
        text.push_str("\nrun 'harness sessions gc --yes' to remove them");
    }
    text
}
