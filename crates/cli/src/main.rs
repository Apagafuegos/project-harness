//! Command-line entry point for the future Ratatui client.

use std::io;

use clap::Command;

fn main() -> io::Result<()> {
    let mut command = Command::new("harness")
        .version(env!("CARGO_PKG_VERSION"))
        .about("A small, inspectable, context-first agent harness")
        .long_about(
            "A small, inspectable, context-first agent harness.\n\n\
             Foundation scaffold: the engine, model providers, tools, sessions, \
             and terminal UI are not implemented yet.\n\n\
             Start with README.md and docs/README.md in the repository.",
        );

    command.get_matches_mut();
    command.print_long_help()?;
    println!();
    Ok(())
}
