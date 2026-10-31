//! Process-spawning terminal tests are isolated from all journal/lock targets.
#[cfg(all(feature = "test-provider", debug_assertions))]
#[path = "support/tui_pty.rs"]
mod support;

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
