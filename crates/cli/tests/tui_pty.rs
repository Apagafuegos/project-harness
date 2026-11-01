//! Process-spawning terminal tests are isolated from all journal/lock targets.
#[cfg(all(feature = "test-provider", debug_assertions))]
#[path = "support/tui_pty.rs"]
mod support;

/// Decisions 1 and 3: plain `harness` is the terminal client, it has no `tui`
/// alias, and its only options are the seven shared with `ask` (no config file).
#[test]
fn plain_harness_is_the_tui_with_seven_flags_and_no_config_file() {
    let binary = env!("CARGO_BIN_EXE_harness");
    let help = std::process::Command::new(binary)
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    for flag in [
        "--data-root",
        "--provider",
        "--auth",
        "--model",
        "--summary-model",
        "--compaction-strategy",
        "--session",
    ] {
        assert!(text.contains(flag), "missing {flag} in {text}");
    }
    assert!(!text.contains("--config"), "no config file is read in M5");
    let options = text
        .lines()
        .filter(|line| line.trim_start().starts_with("--"))
        .count();
    assert_eq!(options, 7, "unexpected top-level option:\n{text}");
    for args in [&["tui"][..], &["--config", "harness.toml"][..]] {
        let out = std::process::Command::new(binary)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(!String::from_utf8_lossy(&out.stderr).contains("panic"));
    }
}

#[cfg(all(feature = "test-provider", debug_assertions))]
#[test]
fn terminal_restored_on_quit_panic_and_sigterm() {
    for scenario in [
        support::Scenario::Quit,
        support::Scenario::Panic,
        support::Scenario::Terminate,
    ] {
        let evidence = support::probe(
            std::path::Path::new(env!("CARGO_BIN_EXE_harness")),
            scenario,
        )
        .unwrap();
        println!("{evidence}");
    }
}

#[cfg(all(feature = "test-provider", debug_assertions))]
#[test]
fn terminal_restored_after_returned_loop_error() {
    let evidence = support::probe(
        std::path::Path::new(env!("CARGO_BIN_EXE_harness")),
        support::Scenario::ReturnedError,
    )
    .unwrap();
    println!("{evidence}");
}

#[test]
fn non_tty_exits_two_without_terminal_sequences() {
    // Stdin is a genuine terminal but stdout is piped: raw mode must not be
    // entered even though crossterm could successfully enable it on stdin.
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let before = pair.master.get_termios().unwrap();
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pair.master.tty_name().unwrap())
        .unwrap();
    let piped = std::process::Command::new(env!("CARGO_BIN_EXE_harness"))
        .stdin(slave)
        .output()
        .unwrap();
    assert_eq!(piped.status.code(), Some(2));
    assert_eq!(before, pair.master.get_termios().unwrap());
    assert!(!piped.stdout.contains(&0x1b));
    assert!(String::from_utf8_lossy(&piped.stderr).contains("use harness ask"));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_harness"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("use harness ask"));
    assert!(!output.stdout.contains(&0x1b));
    assert!(!output.stderr.contains(&0x1b));
}
