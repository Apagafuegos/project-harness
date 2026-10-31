//! Shared PTY probe for the isolated integration target and opt-in live smoke.
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    path::Path,
    sync::mpsc,
    time::{Duration, Instant},
};

#[allow(
    dead_code,
    reason = "shared between offline PTY tests and the opt-in live executable"
)]
#[derive(Clone, Copy)]
pub enum Scenario {
    Quit,
    Panic,
    Terminate,
    Live,
}

pub fn probe(binary: &Path, scenario: Scenario) -> Result<String, String> {
    let temp = tempfile::tempdir().map_err(|_| "tempdir")?;
    let work = temp.path().join("work");
    std::fs::create_dir(&work).map_err(|_| "workspace")?;
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 32,
            cols: 110,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|_| "openpty")?;
    let before = pair.master.get_termios().ok_or("termios before")?;
    let mut command = CommandBuilder::new(binary);
    command.cwd(&work);
    command.arg("--data-root");
    command.arg(temp.path().join("state"));
    command.env_remove("HARNESS_TUI_TEST_PROVIDER");
    command.env_remove("HARNESS_TUI_TEST_PANIC");
    if !matches!(scenario, Scenario::Live) {
        command.env(
            "HARNESS_TUI_TEST_PROVIDER",
            if matches!(scenario, Scenario::Terminate) {
                "stall"
            } else {
                "scripted"
            },
        );
        command.env("HOME", temp.path());
    }
    if matches!(scenario, Scenario::Panic) {
        command.env("HARNESS_TUI_TEST_PANIC", "1");
    }
    let mut child = pair.slave.spawn_command(command).map_err(|_| "spawn")?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().map_err(|_| "reader")?;
    let mut writer = pair.master.take_writer().map_err(|_| "writer")?;
    let (tx, rx) = mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut bytes = [0; 4096];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 {
                break;
            }
            if tx.send(bytes[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now()
        + Duration::from_secs(if matches!(scenario, Scenario::Live) {
            120
        } else {
            30
        });
    let mut output = Vec::new();
    let mut acted = false;
    let mut prompted = false;
    let status = loop {
        while let Ok(bytes) = rx.try_recv() {
            output.extend(bytes);
        }
        let screen = String::from_utf8_lossy(&output);
        if !acted && screen.contains("ctx") {
            match scenario {
                Scenario::Quit => {
                    writer.write_all(b"\x04").map_err(|_| "quit key")?;
                    acted = true;
                }
                Scenario::Terminate if !prompted => {
                    writer
                        .write_all(b"run until cancelled\r")
                        .map_err(|_| "prompt")?;
                    prompted = true;
                }
                Scenario::Terminate if screen.contains("pong") => {
                    if pair.master.get_termios().as_ref() == Some(&before) {
                        return Err("terminal never entered raw mode".into());
                    }
                    let pid =
                        rustix::process::Pid::from_raw(child.process_id().ok_or("pid")? as i32)
                            .ok_or("pid")?;
                    rustix::process::kill_process(pid, rustix::process::Signal::TERM)
                        .map_err(|_| "SIGTERM")?;
                    acted = true;
                }
                Scenario::Live if !prompted => {
                    writer
                        .write_all(b"Reply with the single word: pong\r")
                        .map_err(|_| "prompt")?;
                    prompted = true;
                }
                Scenario::Live if screen.contains("rate_limit") => {
                    writer.write_all(b"\x04").map_err(|_| "quit key")?;
                    acted = true;
                }
                Scenario::Live
                    if screen.contains("completed") && screen.matches("pong").count() >= 2 =>
                {
                    writer.write_all(b"\x04").map_err(|_| "quit key")?;
                    acted = true;
                }
                _ => {}
            }
            writer.flush().map_err(|_| "flush")?;
        }
        if let Some(status) = child.try_wait().map_err(|_| "wait")? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(if screen.contains("rate_limit") {
                "live tui: rate_limit (not retried)"
            } else {
                "PTY deadline"
            }
            .into());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    reader_thread.join().map_err(|_| "reader panic")?;
    while let Ok(bytes) = rx.try_recv() {
        output.extend(bytes);
    }
    let after = pair.master.get_termios().ok_or("termios after")?;
    if before != after {
        return Err(format!(
            "termios differs: before={before:?}, after={after:?}"
        ));
    }
    let screen = String::from_utf8_lossy(&output);
    for sequence in [
        "\x1b[?1049h",
        "\x1b[?2004h",
        "\x1b[?2004l",
        "\x1b[?1049l",
        "\x1b[?25h",
    ] {
        if !screen.contains(sequence) {
            return Err(format!("missing sequence: {sequence:?}"));
        }
    }
    if matches!(scenario, Scenario::Panic) {
        if status.success() {
            return Err("panic exited successfully".into());
        }
        let restore = screen.find("\x1b[?1049l").ok_or("restore")?;
        let panic = screen.find("forced TUI panic").ok_or("panic message")?;
        if restore >= panic {
            return Err("panic printed before restoration".into());
        }
    } else if !status.success() {
        return Err(format!("PTY exit {}", status.exit_code()));
    }
    if matches!(scenario, Scenario::Terminate) {
        let root = harness_core::session::DataRoot::open(temp.path().join("state"), false)
            .map_err(|_| "reopen root")?;
        let entries = root.sessions().map_err(|_| "catalog")?;
        if entries.len() != 1 {
            return Err("missing durable session".into());
        }
        // Inspection under a fresh lock proves shutdown released the journal.
        let inspection =
            harness_core::session::JournalStore::inspect(&root, &entries[0].session_id)
                .map_err(|_| "reopen journal")?;
        if inspection.diagnostic.is_some() {
            return Err("journal not durable".into());
        }
        let snapshot = inspection.snapshot.ok_or("missing snapshot")?;
        if !snapshot.events().any(|(_, _, event)| matches!(event, harness_core::session::JournalEvent::TurnFinished(f) if f.outcome == harness_core::session::TurnOutcome::Cancelled)) {
            return Err("SIGTERM turn not durably cancelled".into());
        }
    }
    if matches!(scenario, Scenario::Live) && screen.contains("rate_limit") {
        return Err("rate_limit (not retried; terminal restored; exit=0)".into());
    }
    Ok(format!(
        "termios before={before:?}\ntermios after={after:?}\nsequences: enter alternate, enable paste, disable paste, leave alternate, show cursor; exit={}{}",
        status.exit_code(),
        if matches!(scenario, Scenario::Live) {
            "; pong observed"
        } else {
            ""
        }
    ))
}
