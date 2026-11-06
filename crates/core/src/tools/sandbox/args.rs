use super::{SandboxConfig, SandboxUnavailable};
use crate::tools::{Plan, WorkspacePath};
use serde::Deserialize;
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Deserialize)]
struct Wire {
    workspace: String,
    name: String,
    backend: String,
    network: String,
    cwd: String,
    timeout_ms: u64,
    arguments: Arguments,
    mounts: Vec<Mount>,
    #[serde(default)]
    sandbox: super::ToolchainSettings,
    #[serde(default)]
    shell: Option<super::ShellOptions>,
}
#[derive(Deserialize)]
struct Arguments {
    command: String,
}
#[derive(Deserialize)]
struct Mount {
    path: String,
    mode: String,
}

pub(super) struct Spec {
    pub args: Vec<OsString>,
    pub timeout: Duration,
    pub absent: Vec<PathBuf>,
}
impl Spec {
    pub fn argv(&self, info: i32, status: i32) -> Vec<OsString> {
        let mut args = vec![
            "--info-fd".into(),
            info.to_string().into(),
            "--json-status-fd".into(),
            status.to_string().into(),
        ];
        args.extend(self.args.clone());
        args
    }
}
fn fail(reason: impl Into<String>) -> SandboxUnavailable {
    SandboxUnavailable::one(reason)
}

pub(super) fn validate_config(config: &SandboxConfig) -> Result<(), SandboxUnavailable> {
    if !config.workspace_root.is_absolute() || !config.workspace_root.is_dir() {
        return Err(fail("configured workspace must be an absolute directory"));
    }
    if !config.bwrap.is_absolute()
        || !config.bash.is_absolute()
        || !config.systemd_run.is_absolute()
    {
        return Err(fail("program paths must be absolute"));
    }
    if config.capture_limit == 0
        || config.capture_limit > 64 * 1024 * 1024
        || config.model_text_limit < 128
        || config.model_text_limit > 32 * 1024
        || config.kill_grace > Duration::from_secs(2)
        || config.tasks_max < 4
    {
        return Err(fail("invalid trusted sandbox limits"));
    }
    config.toolchains.validate().map_err(fail)?;
    for path in config.toolchains.mount_roots() {
        if !path.is_dir()
            || fs::canonicalize(path).ok().as_ref() != Some(path)
            || checked_path(
                Path::new("/"),
                path.strip_prefix("/")
                    .map_err(|_| fail("invalid toolchain root"))?
                    .to_str()
                    .ok_or_else(|| fail("non UTF-8 toolchain root"))?,
            )
            .is_err()
        {
            return Err(fail(format!(
                "unavailable configured toolchain mount {}",
                path.display()
            )));
        }
    }
    for path in &config.system_paths {
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            || !(path.starts_with("/usr")
                || [
                    "/bin",
                    "/lib",
                    "/lib64",
                    "/lib32",
                    "/sbin",
                    "/etc/alternatives",
                    "/etc/ld.so.cache",
                    "/etc/ssl/certs",
                ]
                .iter()
                .any(|p| path == Path::new(p)))
        {
            return Err(fail(
                "system mounts must be explicit runtime paths, never root, home or all of etc",
            ));
        }
    }
    Ok(())
}

pub(super) fn build(
    config: &SandboxConfig,
    plan: &Plan,
    root: &Path,
) -> Result<Spec, SandboxUnavailable> {
    let wire: Wire =
        serde_json::from_slice(plan.bytes()).map_err(|_| fail("invalid internal plan"))?;
    if Some(wire.workspace.as_str()) != root.to_str() {
        return Err(fail("plan workspace identity mismatch"));
    }
    build_wire(config, wire, root)
}
pub(super) fn probe_spec(config: &SandboxConfig, root: &Path) -> Result<Spec, SandboxUnavailable> {
    build_wire(
        config,
        Wire {
            workspace: root.to_string_lossy().into_owned(),
            name: "bash".into(),
            backend: "bubblewrap".into(),
            network: "deny".into(),
            cwd: ".".into(),
            timeout_ms: 20_000,
            arguments: Arguments {
                command: ":".into(),
            },
            mounts: vec![Mount {
                path: ".".into(),
                mode: "rw".into(),
            }],
            sandbox: config.toolchains.clone(),
            shell: None,
        },
        root,
    )
}
fn build_wire(config: &SandboxConfig, wire: Wire, root: &Path) -> Result<Spec, SandboxUnavailable> {
    validate_config(config)?;
    if wire.sandbox != config.toolchains {
        return Err(fail("plan sandbox configuration mismatch"));
    }
    if wire.name != "bash"
        || wire.backend != "bubblewrap"
        || wire.network != "deny"
        || !(1..=600_000).contains(&wire.timeout_ms)
    {
        return Err(fail("not a deny-network bash plan"));
    }
    if !root.is_absolute() || !fs::symlink_metadata(root).is_ok_and(|m| m.is_dir()) {
        return Err(fail("workspace root must be an absolute real directory"));
    }
    let cwd = WorkspacePath::new(&wire.cwd).map_err(|_| fail("invalid plan cwd"))?;
    let cwd_host = checked_path(root, cwd.as_str())?;
    if !cwd_host.is_dir() {
        return Err(fail("plan cwd is not a directory"));
    }
    let mut args: Vec<OsString> = [
        "--unshare-all",
        "--unshare-user",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
    ]
    .map(Into::into)
    .into();
    let path = config.sandbox_path();
    for (key, value) in [
        ("PATH", path.as_str()),
        ("HOME", "/home/sandbox"),
        ("TMPDIR", "/tmp"),
        ("LANG", "C.UTF-8"),
        ("TERM", "dumb"),
    ] {
        args.extend(["--setenv".into(), key.into(), value.into()]);
    }
    for (key, value) in &config.toolchains.env {
        args.extend(["--setenv".into(), key.into(), value.into()]);
    }
    if !config.toolchains.is_empty() {
        // CARGO_HOME stays private and per command. Cargo's build output is
        // ordinary workspace output and is never redirected (M8 decision 14).
        args.extend([
            "--setenv".into(),
            "CARGO_HOME".into(),
            super::ToolchainSettings::cargo_home_dest().into(),
        ]);
    }
    for path in &config.system_paths {
        match fs::read_link(path) {
            Ok(target) => args.extend([
                "--symlink".into(),
                target.into_os_string(),
                path.as_os_str().into(),
            ]),
            Err(_) => {
                fs::metadata(path).map_err(|_| {
                    fail(format!(
                        "missing configured runtime mount {}",
                        path.display()
                    ))
                })?;
                args.extend([
                    "--ro-bind".into(),
                    path.as_os_str().into(),
                    path.as_os_str().into(),
                ]);
            }
        }
    }
    args.extend(
        [
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/home/sandbox",
        ]
        .map(Into::into),
    );
    if !config.toolchains.is_empty() {
        // Private, per-command cargo state only. Build output is not redirected:
        // cargo writes the workspace's own `target/` (M8 decision 14).
        args.extend([
            "--perms".into(),
            "0700".into(),
            "--dir".into(),
            super::ToolchainSettings::cargo_home_dest().into(),
        ]);
    }
    // Install every read-only bind before masks, including overlapping mounts.
    let mounts = config.toolchains.mounts();
    for (source, destination) in &mounts {
        args.extend([
            "--ro-bind".into(),
            source.as_os_str().into(),
            destination.as_os_str().into(),
        ]);
    }
    let mut masked_credentials = Vec::<PathBuf>::new();
    for (root, destination) in &mounts {
        for leaf in config.toolchains.credential_paths(root) {
            let relative = leaf
                .strip_prefix(root)
                .map_err(|_| fail("invalid credential path"))?;
            let target = destination.join(relative);
            if masked_credentials
                .iter()
                .any(|masked| target.starts_with(masked))
            {
                continue;
            }
            checked_path(
                root,
                relative
                    .to_str()
                    .ok_or_else(|| fail("non UTF-8 credential path"))?,
            )?;
            match fs::symlink_metadata(&leaf) {
                Ok(m) if m.is_file() => args.extend([
                    "--ro-bind".into(),
                    "/dev/null".into(),
                    target.into_os_string(),
                ]),
                Ok(m) if m.is_dir() => {
                    args.extend([
                        "--tmpfs".into(),
                        target.as_os_str().into(),
                        "--remount-ro".into(),
                        target.as_os_str().into(),
                    ]);
                    masked_credentials.push(target);
                }
                // D13: absent leaves cannot be masked beneath read-only binds.
                // Never create host placeholders or fail setup for absence.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(fail("cannot mask configured toolchain credential path")),
            }
        }
    }
    let Some(workspace) = wire.mounts.first() else {
        return Err(fail("missing workspace mount"));
    };
    if workspace.path != "." || !["rw", "ro"].contains(&workspace.mode.as_str()) {
        return Err(fail("invalid workspace mount"));
    }
    args.extend([
        if workspace.mode == "rw" {
            "--bind"
        } else {
            "--ro-bind"
        }
        .into(),
        root.as_os_str().into(),
        "/workspace".into(),
    ]);
    let mut absent = Vec::new();
    let mut masked = Vec::<PathBuf>::new();
    // Install ancestor binds before descendant masks. Canonical plans append
    // .git's ro bind last; applying that wire order would expose .git/private
    // again after masking it. Sorting does not alter the approved plan bytes.
    let mut mounts: Vec<_> = wire.mounts[1..].iter().collect();
    mounts.sort_by_key(|mount| Path::new(&mount.path).components().count());
    for mount in mounts {
        let relative = WorkspacePath::new(&mount.path).map_err(|_| fail("invalid mount path"))?;
        if relative.as_str() == "." {
            return Err(fail("duplicate workspace mount"));
        }
        let host = checked_path(root, relative.as_str())?;
        let dest = Path::new("/workspace").join(relative.as_str());
        if masked.iter().any(|ancestor| dest.starts_with(ancestor)) {
            continue;
        }
        match mount.mode.as_str() {
            "ro" => {
                fs::metadata(&host).map_err(|_| fail("read-only plan mount disappeared"))?;
                args.extend([
                    "--ro-bind".into(),
                    host.into_os_string(),
                    dest.into_os_string(),
                ]);
            }
            "masked" => match fs::symlink_metadata(&host) {
                Ok(metadata) if metadata.is_dir() => {
                    args.extend([
                        "--tmpfs".into(),
                        dest.as_os_str().into(),
                        "--remount-ro".into(),
                        dest.as_os_str().into(),
                    ]);
                    masked.push(dest);
                }
                Ok(metadata) if metadata.is_file() => {
                    args.extend([
                        "--ro-bind".into(),
                        "/dev/null".into(),
                        dest.into_os_string(),
                    ]);
                }
                Ok(_) => return Err(fail("unsupported protected-path kind")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => absent.push(host),
                Err(_) => return Err(fail("cannot inspect protected path")),
            },
            _ => return Err(fail("unsupported plan mount mode")),
        }
    }
    let inside_cwd = if cwd.as_str() == "." {
        PathBuf::from("/workspace")
    } else {
        Path::new("/workspace").join(cwd.as_str())
    };
    args.extend([
        "--chdir".into(),
        inside_cwd.into_os_string(),
        config.bash.as_os_str().into(),
        "--noprofile".into(),
        "--norc".into(),
    ]);
    if wire.shell.is_some_and(|shell| shell.pipefail) {
        args.extend(["-o".into(), "pipefail".into()]);
    }
    args.extend(["-c".into(), wire.arguments.command.into()]);
    Ok(Spec {
        args,
        timeout: Duration::from_millis(wire.timeout_ms),
        absent,
    })
}

// Reject any symlink component at mount setup, including an absent leaf's
// ancestors. No path from a plan can bind a symlink's host target into the view.
fn checked_path(root: &Path, relative: &str) -> Result<PathBuf, SandboxUnavailable> {
    let mut path = root.to_owned();
    for part in Path::new(relative).components() {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(fail("symlink in sandbox mount/cwd path"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(fail("cannot inspect sandbox mount/cwd path")),
        }
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn toolchain_arguments_golden_and_configuration_binding() {
        let workspace = tempfile::tempdir().unwrap();
        let toolchain = tempfile::tempdir().unwrap();
        fs::create_dir(toolchain.path().join("bin")).unwrap();
        fs::write(toolchain.path().join("credentials"), "secret").unwrap();
        let registry = tempfile::tempdir().unwrap();
        fs::create_dir(registry.path().join("cache")).unwrap();
        let config = SandboxConfig {
            system_paths: vec!["/usr".into()],
            toolchains: super::super::ToolchainSettings {
                toolchain_paths: vec![toolchain.path().into()],
                path_prepend: vec![toolchain.path().join("bin")],
                cargo_registry: Some(registry.path().into()),
                env: [("RUSTUP_TOOLCHAIN".into(), "1.99.1".into())].into(),
            },
            ..SandboxConfig::default()
        };
        let spec = probe_spec(&config, workspace.path()).unwrap();
        let normalized: Vec<_> = spec
            .argv(10, 11)
            .iter()
            .map(|s| {
                s.to_str()
                    .unwrap()
                    .replace(toolchain.path().to_str().unwrap(), "<toolchain>")
                    .replace(registry.path().to_str().unwrap(), "<registry>")
                    .replace(workspace.path().to_str().unwrap(), "<workspace>")
            })
            .collect();
        assert_eq!(
            serde_json::to_string(&normalized).unwrap(),
            include_str!("../golden/toolchains-argv.json").trim()
        );
        let mut wire: Wire = serde_json::from_str(include_str!("../golden/bash.json")).unwrap();
        wire.workspace = workspace.path().to_str().unwrap().into();
        assert!(
            build_wire(&config, wire, workspace.path())
                .err()
                .unwrap()
                .to_string()
                .contains("configuration mismatch")
        );
    }

    #[test]
    fn exact_arguments_for_both_bash_golden_plans() {
        let root = tempfile::tempdir().unwrap();
        for path in [".git", "a/b", "private"] {
            fs::create_dir_all(root.path().join(path)).unwrap();
        }
        let config = SandboxConfig::default();
        for (golden, extra) in [
            (include_str!("../golden/bash.json"), false),
            (include_str!("../golden/bash-protected.json"), true),
        ] {
            let spec =
                build_wire(&config, serde_json::from_str(golden).unwrap(), root.path()).unwrap();
            let mut expected: Vec<OsString> = [
                "--info-fd",
                "10",
                "--json-status-fd",
                "11",
                "--unshare-all",
                "--unshare-user",
                "--die-with-parent",
                "--new-session",
                "--clearenv",
                "--setenv",
                "PATH",
                "/usr/bin:/bin",
                "--setenv",
                "HOME",
                "/home/sandbox",
                "--setenv",
                "TMPDIR",
                "/tmp",
                "--setenv",
                "LANG",
                "C.UTF-8",
                "--setenv",
                "TERM",
                "dumb",
                "--ro-bind",
                "/usr",
                "/usr",
            ]
            .map(Into::into)
            .into();
            // Host-dependent merged-/usr aliases and architecture-specific paths
            // are facts, while every flag and destination is checked exactly.
            for path in config.system_paths.iter().skip(1) {
                if let Ok(target) = fs::read_link(path) {
                    expected.extend([
                        "--symlink".into(),
                        target.into_os_string(),
                        path.as_os_str().into(),
                    ]);
                } else {
                    expected.extend([
                        "--ro-bind".into(),
                        path.as_os_str().into(),
                        path.as_os_str().into(),
                    ]);
                }
            }
            expected.extend(
                [
                    "--proc",
                    "/proc",
                    "--dev",
                    "/dev",
                    "--tmpfs",
                    "/tmp",
                    "--tmpfs",
                    "/home/sandbox",
                    "--bind",
                ]
                .map(Into::into),
            );
            expected.extend([
                root.path().as_os_str().into(),
                "/workspace".into(),
                "--tmpfs".into(),
                "/workspace/.git".into(),
                "--remount-ro".into(),
                "/workspace/.git".into(),
            ]);
            if extra {
                for path in ["/workspace/private", "/workspace/a/b"] {
                    expected.extend([
                        "--tmpfs".into(),
                        path.into(),
                        "--remount-ro".into(),
                        path.into(),
                    ]);
                }
            }
            expected.extend(
                [
                    "--chdir",
                    "/workspace",
                    "/bin/bash",
                    "--noprofile",
                    "--norc",
                    "-c",
                    "echo hello",
                ]
                .map(Into::into),
            );
            assert_eq!(spec.argv(10, 11), expected);
            assert_eq!(
                spec.absent,
                if extra {
                    vec![root.path().join("z/absent")]
                } else {
                    Vec::new()
                }
            );
        }
    }
}

#[cfg(test)]
mod shell_tests {
    use super::*;

    #[test]
    fn recorded_shell_options_control_exact_argv() {
        let root = tempfile::tempdir().unwrap();
        let config = SandboxConfig::default();
        for shell in [None, Some(false), Some(true)] {
            let mut wire: Wire = serde_json::from_str(include_str!("../golden/bash.json")).unwrap();
            wire.shell = shell.map(|pipefail| super::super::ShellOptions { pipefail });
            wire.arguments.command = "(exit 7) | cat".into();
            let spec = build_wire(&config, wire, root.path()).unwrap();
            let start = spec.args.iter().position(|s| s == "/bin/bash").unwrap();
            let argv = &spec.args[start + 1..];
            let expected_argv = if shell == Some(true) {
                vec![
                    "--noprofile",
                    "--norc",
                    "-o",
                    "pipefail",
                    "-c",
                    "(exit 7) | cat",
                ]
            } else {
                vec!["--noprofile", "--norc", "-c", "(exit 7) | cat"]
            };
            assert_eq!(
                argv,
                expected_argv.iter().map(OsString::from).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn shell_options_are_strict_and_legacy_bytes_are_unchanged() {
        let legacy = include_bytes!("../golden/bash.json");
        let value: serde_json::Value = serde_json::from_slice(legacy).unwrap();
        let wire: Wire = serde_json::from_slice(legacy).unwrap();
        assert!(wire.shell.is_none());
        for shell in [
            serde_json::json!({}),
            serde_json::json!({"pipefail":"true"}),
            serde_json::json!({"pipefail":true,"extra":true}),
        ] {
            let mut invalid = value.clone();
            invalid["shell"] = shell;
            assert!(serde_json::from_value::<Wire>(invalid).is_err());
        }
    }
}
