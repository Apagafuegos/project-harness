//! Process-spawning shell semantics checks are isolated from journal-lock tests.
use std::process::Command;

#[test]
fn native_pipeline_status_matches_the_bound_shell_options() {
    for (pipefail, expected) in [(None, 0), (Some(false), 0), (Some(true), 7)] {
        let mut command = Command::new("/bin/bash");
        command.args(["--noprofile", "--norc"]);
        if pipefail == Some(true) {
            command.args(["-o", "pipefail"]);
        }
        let status = command
            .args(["-c", "(exit 7) | cat"])
            .env_clear()
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(expected));
    }
}
