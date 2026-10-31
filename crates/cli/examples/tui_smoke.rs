//! Opt-in real Codex TUI smoke. Only the PTY verifier's sanitized result prints.
#[path = "../tests/support/tui_pty.rs"]
mod support;
fn main() -> std::process::ExitCode {
    let binary = std::env::args_os().nth(1).expect("path to harness binary");
    match support::probe(std::path::Path::new(&binary), support::Scenario::Live) {
        Ok(evidence) => {
            println!("live tui: auth=codex\n{evidence}\nsummary: pass");
            std::process::ExitCode::SUCCESS
        }
        Err(code) => {
            eprintln!("{code}\nsummary: fail");
            std::process::ExitCode::FAILURE
        }
    }
}
