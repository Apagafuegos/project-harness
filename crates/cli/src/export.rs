//! `harness sessions export`: a Markdown transcript, or a self-contained data root.
//!
//! Both forms are read-only: the journal is validated under the shared catalog
//! lock, and an existing output path is never overwritten.
use super::sessions_cli::{Failure, data_root, maintenance, open};
use clap::{Arg, ArgAction, ArgMatches, Command, value_parser};
use harness_core::session::SessionId;
use std::path::PathBuf;

pub fn command() -> Command {
    Command::new("export")
        .about("Export a session as Markdown, or as a data root with its artifacts")
        .long_about(
            "Export one session without modifying it.\n\n\
             `--format markdown` writes a readable transcript of the active branch \
             from the root to the head, or every branch with `--all-branches`. \
             Text is verbatim, tool calls and results are fenced (never truncated), \
             media is referenced by type, size and digest, and opaque provider \
             state is omitted with a note.\n\n\
             `--format journal` creates the output directory and copies the \
             verified journal to `sessions/<id>.jsonl` with every artifact it \
             references to `artifacts/<sha256>`, verifying each checksum. The \
             result is itself a data root.\n\n\
             The output path must not exist, and the file or directory is \
             published only when complete.",
        )
        .arg(
            Arg::new("session-id")
                .value_name("ID")
                .required(true)
                .value_parser(|value: &str| value.parse::<SessionId>())
                .help("Session UUIDv7"),
        )
        .arg(
            Arg::new("format")
                .long("format")
                .value_name("FORMAT")
                .value_parser(["markdown", "journal"])
                .default_value("markdown")
                .help("markdown: one file; journal: a data root with its artifacts"),
        )
        .arg(
            Arg::new("output")
                .long("output")
                .value_name("PATH")
                .required(true)
                .value_parser(value_parser!(PathBuf))
                .help("Destination file (markdown) or new directory (journal)"),
        )
        .arg(
            Arg::new("all-branches")
                .long("all-branches")
                .action(ArgAction::SetTrue)
                .help("Markdown only: name every branch head instead of the active branch"),
        )
}

pub fn run(arguments: &ArgMatches) -> Result<String, Failure> {
    let path = data_root(arguments)?;
    let id = arguments
        .get_one::<SessionId>("session-id")
        .expect("required");
    let output = arguments.get_one::<PathBuf>("output").expect("required");
    let all_branches = arguments.get_flag("all-branches");
    let root = open(&path)?;
    match arguments
        .get_one::<String>("format")
        .map(String::as_str)
        .unwrap_or("markdown")
    {
        "markdown" => {
            root.export_markdown_to(id, output, all_branches)
                .map_err(maintenance)?;
            Ok(format!("wrote Markdown export to {}", output.display()))
        }
        "journal" => {
            if all_branches {
                return Err(Failure::Refusal(
                    "--all-branches applies to --format markdown".into(),
                ));
            }
            root.export_journal(id, output).map_err(maintenance)?;
            Ok(format!(
                "wrote journal export to {} (sessions/ and artifacts/)",
                output.display()
            ))
        }
        other => Err(Failure::Refusal(format!("unknown format: {other}"))),
    }
}
