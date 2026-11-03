//! Opt-in real-provider TUI smoke. Only sanitized PTY evidence prints.
#[path = "../tests/support/tui_pty.rs"]
mod support;

fn usage() -> std::process::ExitCode {
    eprintln!("usage: tui_smoke BINARY [--provider openai|deepseek]");
    std::process::ExitCode::from(2)
}

fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(binary) = args.next() else {
        return usage();
    };
    let provider = match args.next() {
        None => "openai",
        Some(flag) if flag == "--provider" => match args.next().as_deref() {
            Some(value) if value == "openai" => "openai",
            Some(value) if value == "deepseek" => "deepseek",
            _ => return usage(),
        },
        _ => return usage(),
    };
    if args.next().is_some() {
        return usage();
    }
    println!("live tui: provider={provider}");
    if provider == "deepseek"
        && std::env::var_os("DEEPSEEK_API_KEY").is_none_or(|key| key.is_empty())
    {
        println!("skipped (no DEEPSEEK_API_KEY)");
        return std::process::ExitCode::SUCCESS;
    }
    let model = if provider == "openai" {
        println!("auth=codex");
        harness_providers::openai::LIVE_CHECK_MODEL
    } else {
        harness_providers::anthropic::DEEPSEEK_LIVE_CHECK_MODEL
    };
    println!("model={model}");
    match support::probe_live(std::path::Path::new(&binary), provider, model) {
        Ok(evidence) => {
            println!("{evidence}\nsummary: pass");
            std::process::ExitCode::SUCCESS
        }
        Err(code) => {
            eprintln!("{code}\nsummary: fail");
            std::process::ExitCode::FAILURE
        }
    }
}
