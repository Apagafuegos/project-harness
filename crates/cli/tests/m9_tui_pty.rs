//! Fresh real-terminal coverage of the batch preview and undo flow.
#![cfg(all(feature = "test-provider", debug_assertions))]

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    fs,
    io::{Read, Write},
    sync::mpsc,
    time::{Duration, Instant},
};

#[test]
fn batch_approval_and_undo_in_the_running_tui() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    fs::create_dir(&work).unwrap();
    for path in ["a.rs", "b.rs", "c.rs"] {
        fs::write(work.join(path), "fn old_name() {}\n").unwrap();
    }
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 44,
            cols: 110,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let before = pair.master.get_termios().unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_harness"));
    command.cwd(&work);
    command.args(["--no-config", "--data-root"]);
    command.arg(temp.path().join("state"));
    command.env("HOME", temp.path());
    command.env("HARNESS_TUI_TEST_PROVIDER", "m9-edits");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (tx, rx) = mpsc::channel();
    let read = std::thread::spawn(move || {
        let mut buf = [0; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut output = Vec::new();
    let mut stage = 0;
    let mut since = 0;
    loop {
        while let Ok(bytes) = rx.try_recv() {
            output.extend(bytes);
        }
        let screen = String::from_utf8_lossy(&output[since..]);
        let send = match stage {
            0 if screen.contains("context") => Some(&b"Rename old_name across three files\r"[..]),
            1 if screen.contains("approve tool call")
                && ["a.rs", "b.rs", "c.rs", "old_name", "new_name"]
                    .iter()
                    .all(|s| screen.contains(s)) =>
            {
                capture("approval", &output);
                Some(&b"a"[..])
            }
            2 if screen.contains("Batch edit finished.") && screen.contains("idle") => {
                for path in ["a.rs", "b.rs", "c.rs"] {
                    assert_eq!(
                        fs::read_to_string(work.join(path)).unwrap(),
                        "fn new_name() {}\n"
                    );
                }
                Some(&b"\x1a"[..])
            }
            3 if screen.contains("undo")
                && ["a.rs", "b.rs", "c.rs"].iter().all(|s| screen.contains(s)) =>
            {
                capture("undo", &output);
                Some(&b"\r"[..])
            }
            4 if screen.contains("idle") && ["a.rs", "b.rs", "c.rs"].iter().all(|path| {
                fs::read_to_string(work.join(path)).unwrap() == "fn old_name() {}\n"
            }) =>
            {
                Some(&b"\x04"[..])
            }
            _ => None,
        };
        if let Some(bytes) = send {
            writer.write_all(bytes).unwrap();
            writer.flush().unwrap();
            since = output.len();
            stage += 1;
        }
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "exit={status:?}; stage={stage}; screen={screen}"
            );
            assert_eq!(stage, 5, "flow did not finish; screen={screen}");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("M9 PTY deadline at stage {stage}: {screen}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(writer);
    read.join().unwrap();
    assert_eq!(pair.master.get_termios().unwrap(), before);
}

fn capture(name: &str, bytes: &[u8]) {
    if let Some(dir) = std::env::var_os("HARNESS_M9_SCREEN_DIR") {
        let dir = std::path::PathBuf::from(dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{name}.ansi")), bytes).unwrap();
    }
}
