//! `harness doctor` reports host capabilities and fails closed.

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

use serde_json::Value;

fn doctor(arguments: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_harness"))
        .arg("doctor")
        .args(arguments)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The report has every section whatever the host offers.
#[test]
fn reports_every_capability() {
    let report = doctor(&[]);
    for (key, values) in [
        ("file_tools", &["available", "unavailable"][..]),
        ("shell_sandbox", &["available", "unavailable"]),
        ("process_limit", &["cgroup", "recorded_gap"]),
    ] {
        let value = report["summary"][key].as_str().unwrap();
        assert!(values.contains(&value), "{key}: {value}");
    }
    assert_eq!(report["bubblewrap"]["path"], "/usr/bin/bwrap");
    assert!(report["host"]["kernel"].is_string());
}

#[test]
fn missing_bwrap_is_unavailable() {
    let report = doctor(&["--bwrap", "/nonexistent/bwrap"]);
    assert_eq!(report["summary"]["shell_sandbox"], "unavailable");
    assert_eq!(report["bubblewrap"]["status"], "unavailable");
}

/// A `bwrap` that starts but sandboxes nothing must be caught. The fake
/// prints what the probe script would see on an unsandboxed host rather than
/// running it, so the test never writes outside its directory.
#[test]
fn unsandboxed_bwrap_is_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let fake = temp.path().join("bwrap");
    fs::write(
        &fake,
        "#!/bin/sh\n\
         [ \"$1\" = --version ] && { echo 'bubblewrap 0.9.0'; exit 0; }\n\
         echo 'uid_map=0 0 4294967295'\n\
         echo 'interfaces=lo,eth0,'\n\
         echo 'env=HOME,PATH,SSH_AUTH_SOCK,'\n\
         echo 'session=0'\n\
         echo 'workspace_write=refused'\n\
         echo 'outside_write=ok'\n\
         echo 'usr_write=ok'\n\
         echo 'ready=yes'\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let report = doctor(&["--bwrap", fake.to_str().unwrap()]);
    assert_eq!(report["bubblewrap"]["version"], "0.9.0");
    assert_eq!(report["bubblewrap"]["status"], "unavailable");
    let failures: Vec<&str> = report["bubblewrap"]["failures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|failure| failure.as_str().unwrap())
        .collect();
    assert_eq!(
        failures,
        [
            "no user namespace",
            "network interfaces besides lo: lo,eth0",
            "environment leaked: SSH_AUTH_SOCK",
            "--new-session did not start a new session",
            "the workspace mount is not writable from inside",
            "a write outside the workspace was not refused",
            "a write to /usr was not refused",
            "no sandbox process found to check its id maps from the host",
        ]
    );
}

/// A sandbox that reports a correct map about itself but whose process maps
/// to outside uid 0 from the host must be rejected. The fake answers every
/// in-sandbox check correctly and then waits in `cat`, a host process whose
/// maps are the host's identity maps of the whole range. The checks run in a
/// subshell that becomes `cat`, so the one descendant exists before
/// `ready=yes` is printed; forking `cat` afterwards raced the host's scan.
#[test]
fn mapping_to_outside_root_is_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let fake = temp.path().join("bwrap");
    fs::write(
        &fake,
        "#!/bin/sh\n\
         [ \"$1\" = --version ] && { echo 'bubblewrap 0.9.0'; exit 0; }\n\
         while [ $# -gt 0 ]; do [ \"$1\" = --bind ] && ws=$2; shift; done\n\
         (\n\
         echo inside > \"$ws/inside\"\n\
         echo 'uid_map=1001 0 1'\n\
         echo 'interfaces=lo,'\n\
         echo 'env=HOME,LANG,PATH,PWD,TERM,'\n\
         echo 'session=1'\n\
         echo 'workspace_write=ok'\n\
         echo 'outside_write=refused'\n\
         echo 'usr_write=refused'\n\
         echo 'ready=yes'\n\
         exec cat > /dev/null\n\
         )\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let report = doctor(&["--bwrap", fake.to_str().unwrap()]);
    let bubblewrap = &report["bubblewrap"];
    assert_eq!(bubblewrap["status"], "unavailable");
    assert_eq!(
        bubblewrap["help"],
        "docs/development.md#when-the-shell-sandbox-is-unavailable"
    );
    let failures = bubblewrap["failures"].as_array().unwrap();
    assert_eq!(failures.len(), 2, "{failures:?}");
    let uid = failures[0].as_str().unwrap();
    let gid = failures[1].as_str().unwrap();
    assert!(
        uid.starts_with("uid_map seen from the host is not one id mapped to euid"),
        "{uid}"
    );
    assert!(uid.ends_with(": 0 0 4294967295"), "{uid}");
    assert!(
        gid.starts_with("gid_map seen from the host is not one id mapped to egid"),
        "{gid}"
    );
    assert_eq!(
        bubblewrap["probe"]["host_maps"][0]["uid_map"][0],
        "0 0 4294967295"
    );
}

#[test]
fn old_bwrap_is_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let fake = temp.path().join("bwrap");
    fs::write(&fake, "#!/bin/sh\necho 'bubblewrap 0.4.1'\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let report = doctor(&["--bwrap", fake.to_str().unwrap()]);
    assert_eq!(report["bubblewrap"]["status"], "unavailable");
    let failures = report["bubblewrap"]["failures"][0].as_str().unwrap();
    assert!(
        failures.contains("older than the required 0.5.0"),
        "{failures}"
    );
}

#[test]
fn relative_program_paths_are_refused() {
    let output = Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(["doctor", "--bwrap", "bwrap"])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("expected an absolute path"));
}

#[test]
fn workspace_requires_absolute_data_root_and_vice_versa() {
    for arguments in [
        vec!["--workspace", "/workspace"],
        vec!["--data-root", "/state"],
        vec!["--workspace", "relative", "--data-root", "/state"],
        vec!["--workspace", "/workspace", "--data-root", "relative"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_harness"))
            .arg("doctor")
            .args(arguments)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
    }
}

#[test]
fn workspace_report_with_broken_bwrap_keeps_file_tools() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let data = temp.path().join("state");
    let fake = temp.path().join("bwrap");
    fs::write(&fake, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let report = doctor(&[
        "--workspace",
        workspace.to_str().unwrap(),
        "--data-root",
        data.to_str().unwrap(),
        "--bwrap",
        fake.to_str().unwrap(),
    ]);
    assert_eq!(report["tools"]["file_tools"]["status"], "available");
    assert_eq!(report["tools"]["shell"]["status"], "unavailable");
    assert!(
        !report["tools"]["shell"]["reasons"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!data.exists(), "doctor must not create storage");
}

/// Probe subprocesses stay in integration binaries, not alongside core's
/// journal-lock/reopen unit tests. A failed file layer must still probe bwrap.
#[test]
fn workspace_report_retains_file_and_probe_failures() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let alias = temp.path().join("workspace-alias");
    std::os::unix::fs::symlink(&workspace, &alias).unwrap();
    let data = temp.path().join("state");
    let fake = temp.path().join("bwrap");
    fs::write(&fake, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let report = doctor(&[
        "--workspace",
        alias.to_str().unwrap(),
        "--data-root",
        data.to_str().unwrap(),
        "--bwrap",
        fake.to_str().unwrap(),
    ]);
    assert_eq!(report["tools"]["file_tools"]["status"], "unavailable");
    assert_eq!(report["tools"]["shell"]["status"], "unavailable");
    let file_reasons = report["tools"]["file_tools"]["reasons"].as_array().unwrap();
    let shell_reasons = report["tools"]["shell"]["reasons"].as_array().unwrap();
    assert!(!file_reasons.is_empty());
    assert!(shell_reasons.contains(&Value::from("planning_view_unavailable")));
    assert!(shell_reasons.contains(&Value::from("bwrap_version_unrecognized")));
    assert!(
        file_reasons
            .iter()
            .all(|reason| shell_reasons.contains(reason))
    );
    assert!(!data.exists());
}

#[test]
fn workspace_report_refuses_unprotected_data_root() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("state");
    let report = doctor(&[
        "--workspace",
        temp.path().to_str().unwrap(),
        "--data-root",
        data.to_str().unwrap(),
        "--bwrap",
        "/missing/bwrap",
    ]);
    let unavailable =
        serde_json::json!({"status":"unavailable", "reasons":["data_root_unprotected"]});
    assert_eq!(report["tools"]["file_tools"], unavailable);
    assert_eq!(report["tools"]["shell"], unavailable);
    assert!(!data.exists());
}

#[test]
#[ignore = "S02 real Linux sandbox: forwarding fake bwrap"]
fn workspace_report_with_working_fake_bwrap() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let data = temp.path().join("state");
    let fake = temp.path().join("bwrap");
    // A fake that fabricates success cannot pass the host-side map check.
    // Forwarding exercises the configurable path with real namespace evidence.
    fs::write(&fake, "#!/bin/sh\nexec /usr/bin/bwrap \"$@\"\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let report = doctor(&[
        "--workspace",
        workspace.to_str().unwrap(),
        "--data-root",
        data.to_str().unwrap(),
        "--bwrap",
        fake.to_str().unwrap(),
    ]);
    assert_eq!(
        report["tools"],
        serde_json::json!({"file_tools":{"status":"available"}, "shell":{"status":"available"}}),
        "{report}"
    );
    assert!(!data.exists());
}
