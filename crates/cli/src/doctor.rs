//! `harness doctor`: probe the host capabilities Milestone 3 depends on.
//!
//! This is the Milestone 3 step 1 spike. It reports what the host offers and
//! never changes host configuration: it creates and removes a private
//! temporary directory, starts probe sandboxes, and starts one transient
//! systemd user scope. Every check fails closed: anything the doctor cannot
//! confirm is reported as unavailable, with the reason.

use std::{
    fs,
    io::Read,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use rustix::{
    fs::{CWD, Mode, OFlags, ResolveFlags, openat2},
    io::Errno,
};
use serde_json::{Value, json};

/// Default absolute path of Bubblewrap. It is never looked up through `PATH`.
pub const DEFAULT_BWRAP: &str = "/usr/bin/bwrap";
/// Default absolute path of `systemd-run`.
pub const DEFAULT_SYSTEMD_RUN: &str = "/usr/bin/systemd-run";
/// Oldest Bubblewrap accepted: 0.5.0 added `--clearenv`.
const MIN_BWRAP: (u32, u32, u32) = (0, 5, 0);
/// The `TasksMax` the cgroup probe asks for and must read back as `pids.max`.
const TASKS_MAX: u32 = 64;
/// Upper bound on any probe process.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// The only variables the sandbox may see, with their fixed values.
const SANDBOX_ENV: [(&str, &str); 4] = [
    ("PATH", "/usr/bin:/bin"),
    ("HOME", "/home/sandbox"),
    ("LANG", "C.UTF-8"),
    ("TERM", "dumb"),
];

/// Runs inside the probe sandbox and prints one `key=value` line per fact.
const SANDBOX_SCRIPT: &str = r#"
echo "uid_map=$(tr -s ' ' < /proc/self/uid_map | sed 's/^ //')"
echo "interfaces=$(sed -n '3,$p' /proc/net/dev | cut -d: -f1 | tr -d ' ' | tr '\n' ',')"
echo "env=$(tr '\0' '\n' < /proc/$$/environ | cut -d= -f1 | sort | tr '\n' ',')"
echo "session=$(cut -d' ' -f6 /proc/$$/stat)"
(echo inside > /workspace/inside) 2>/dev/null && echo workspace_write=ok || echo workspace_write=refused
(echo escape > /outside/escape) 2>/dev/null && echo outside_write=ok || echo outside_write=refused
(echo escape > /usr/harness-doctor-escape) 2>/dev/null && echo usr_write=ok || echo usr_write=refused
echo done=yes
"#;

/// Paths of the external programs the doctor probes.
pub struct Programs {
    /// Absolute path of `bwrap`.
    pub bwrap: PathBuf,
    /// Absolute path of `systemd-run`.
    pub systemd_run: PathBuf,
}

/// Probe the host and return the report.
pub fn report(programs: &Programs) -> Result<Value, String> {
    let scratch = tempfile::Builder::new()
        .prefix("harness-doctor-")
        .tempdir()
        .map_err(|error| format!("cannot create a scratch directory: {error}"))?;
    let openat2 = openat2_probe(scratch.path());
    let bubblewrap = bwrap_probe(&programs.bwrap, scratch.path());
    let cgroup = cgroup_probe(programs, bubblewrap["status"] == "available");
    let summary = json!({
        "file_tools": openat2["status"],
        "shell_sandbox": bubblewrap["status"],
        "process_limit": if cgroup["status"] == "available" {
            "cgroup"
        } else {
            "recorded_gap"
        },
    });
    Ok(json!({
        "harness_version": env!("CARGO_PKG_VERSION"),
        "summary": summary,
        "host": host(&programs.bwrap),
        "openat2": openat2,
        "bubblewrap": bubblewrap,
        "cgroup": cgroup,
    }))
}

fn read_trimmed(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

fn host(bwrap: &Path) -> Value {
    let os = fs::read_to_string("/etc/os-release").ok().and_then(|text| {
        text.lines()
            .find_map(|line| line.strip_prefix("PRETTY_NAME="))
            .map(|name| name.trim_matches('"').to_owned())
    });
    // Profiles that attach to our bwrap path, by file name. Their content is
    // what lets bwrap create user namespaces on Ubuntu, so name them.
    let needle = bwrap.to_string_lossy();
    let mut profiles: Vec<String> = fs::read_dir("/etc/apparmor.d")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_file())
        .filter(|entry| {
            fs::read_to_string(entry.path()).is_ok_and(|text| text.contains(needle.as_ref()))
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    profiles.sort();
    json!({
        "os": os,
        "kernel": read_trimmed("/proc/sys/kernel/osrelease"),
        "apparmor_restrict_unprivileged_userns":
            read_trimmed("/proc/sys/kernel/apparmor_restrict_unprivileged_userns"),
        "unprivileged_userns_clone": read_trimmed("/proc/sys/kernel/unprivileged_userns_clone"),
        "max_user_namespaces": read_trimmed("/proc/sys/user/max_user_namespaces"),
        "apparmor_profiles_naming_bwrap": profiles,
        "apparmor_confinement": read_trimmed("/proc/self/attr/current"),
    })
}

fn errno_name(errno: Errno) -> String {
    let name = match errno {
        Errno::XDEV => "EXDEV",
        Errno::LOOP => "ELOOP",
        Errno::NOSYS => "ENOSYS",
        Errno::PERM => "EPERM",
        Errno::ACCESS => "EACCES",
        Errno::NOENT => "ENOENT",
        Errno::INVAL => "EINVAL",
        _ => return format!("errno {}", errno.raw_os_error()),
    };
    name.to_owned()
}

/// Resolve paths beneath a directory exactly as D6 specifies, and check that
/// the in-tree open succeeds while every escape is refused.
fn openat2_probe(scratch: &Path) -> Value {
    let root = scratch.join("openat2");
    let setup = fs::create_dir(&root)
        .and_then(|()| fs::write(root.join("inside"), b"inside"))
        .and_then(|()| symlink("inside", root.join("link")));
    if let Err(error) = setup {
        return json!({ "status": "unavailable", "reason": format!("setup failed: {error}") });
    }
    let resolve = ResolveFlags::BENEATH
        | ResolveFlags::NO_SYMLINKS
        | ResolveFlags::NO_MAGICLINKS
        | ResolveFlags::NO_XDEV;
    let dir = match openat2(
        CWD,
        &root,
        OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::empty(),
    ) {
        Ok(dir) => dir,
        Err(errno) => {
            return json!({
                "status": "unavailable",
                "reason": format!("openat2 failed on the workspace root: {}", errno_name(errno)),
            });
        }
    };
    let escape = format!(
        "../{}/inside",
        root.file_name().unwrap_or_default().to_string_lossy()
    );
    let cases = [
        ("inside", "inside", true),
        ("dotdot_escape", escape.as_str(), false),
        ("absolute", "/proc/self/cwd", false),
        ("symlink", "link", false),
    ];
    let mut results = serde_json::Map::new();
    let mut ok = true;
    for (name, path, should_open) in cases {
        let outcome = openat2(
            &dir,
            path,
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
            resolve,
        );
        ok &= outcome.is_ok() == should_open;
        results.insert(
            name.into(),
            match outcome {
                Ok(_) => json!("opened"),
                Err(errno) => json!(errno_name(errno)),
            },
        );
    }
    json!({
        "status": if ok { "available" } else { "unavailable" },
        "reason": (!ok).then_some("an in-tree open failed or an escape was not refused"),
        "resolve": ["BENEATH", "NO_SYMLINKS", "NO_MAGICLINKS", "NO_XDEV"],
        "cases": results,
    })
}

/// The outcome of a probe process.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

impl Run {
    fn json(&self) -> Value {
        json!({
            "exit_code": self.code,
            "timed_out": self.timed_out,
            "stderr": self.stderr.trim(),
        })
    }
}

/// Run `command` with a timeout; the child is killed when it expires.
fn spawn(command: &mut Command) -> Result<Run, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start {:?}: {error}", command.get_program()))?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let out = thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let err = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                let _ = child.kill();
                break child.wait().ok();
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(format!("cannot wait for the probe: {error}")),
        }
    };
    Ok(Run {
        code: status.and_then(|status| status.code()),
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
        timed_out,
    })
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let version = text.trim().strip_prefix("bubblewrap ")?;
    let mut parts = version.split('.').map(|part| part.parse::<u32>().ok());
    Some((
        parts.next()??,
        parts.next()??,
        parts.next().flatten().unwrap_or(0),
    ))
}

/// Read-only binds of the system directories. Merged-`/usr` symlinks such as
/// `/bin -> usr/bin` are recreated as symlinks.
fn system_mounts() -> Vec<String> {
    let mut args: Vec<String> = ["--ro-bind", "/usr", "/usr"].map(Into::into).to_vec();
    for dir in ["/bin", "/sbin", "/lib", "/lib32", "/lib64"] {
        match fs::read_link(dir) {
            Ok(target) => args.extend([
                "--symlink".into(),
                target.to_string_lossy().into_owned(),
                dir.into(),
            ]),
            Err(_) if Path::new(dir).is_dir() => {
                args.extend(["--ro-bind".into(), dir.into(), dir.into()])
            }
            Err(_) => {}
        }
    }
    args
}

/// The D8 sandbox shape around `/bin/sh -c script`.
fn sandbox_args(workspace: &Path, outside: &Path, script: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "--unshare-all",
        "--unshare-user",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
    ]
    .map(Into::into)
    .to_vec();
    for (name, value) in SANDBOX_ENV {
        args.extend(["--setenv".into(), name.into(), value.into()]);
    }
    args.extend(system_mounts());
    for arg in [
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
        "--tmpfs",
        "/home/sandbox",
        "--bind",
    ] {
        args.push(arg.into());
    }
    args.extend([
        workspace.to_string_lossy().into_owned(),
        "/workspace".into(),
        "--ro-bind".into(),
        outside.to_string_lossy().into_owned(),
        "/outside".into(),
        "--chdir".into(),
        "/workspace".into(),
        "/bin/sh".into(),
        "-c".into(),
        script.into(),
    ]);
    args
}

fn bwrap_probe(bwrap: &Path, scratch: &Path) -> Value {
    let metadata = match fs::metadata(bwrap) {
        Ok(metadata) => metadata,
        Err(error) => {
            return json!({
                "path": bwrap,
                "status": "unavailable",
                "reason": format!("cannot stat {}: {error}", bwrap.display()),
            });
        }
    };
    let setuid = metadata.permissions().mode() & 0o4000 != 0;
    let version_text = spawn(Command::new(bwrap).arg("--version"))
        .map(|run| run.stdout.trim().to_owned())
        .unwrap_or_default();
    let version = parse_version(&version_text);
    let mut failures = Vec::new();
    match version {
        None => failures.push(format!("unrecognised version output {version_text:?}")),
        Some(version) if version < MIN_BWRAP => failures.push(format!(
            "bubblewrap {}.{}.{} is older than the required {}.{}.{}",
            version.0, version.1, version.2, MIN_BWRAP.0, MIN_BWRAP.1, MIN_BWRAP.2
        )),
        Some(_) => {}
    }
    let base = json!({
        "path": bwrap,
        "version": version.map(|(a, b, c)| format!("{a}.{b}.{c}")),
        "minimum_version": format!("{}.{}.{}", MIN_BWRAP.0, MIN_BWRAP.1, MIN_BWRAP.2),
        "setuid": setuid,
    });
    let workspace = scratch.join("workspace");
    let outside = scratch.join("outside");
    if let Err(error) = fs::create_dir(&workspace).and_then(|()| fs::create_dir(&outside)) {
        failures.push(format!("setup failed: {error}"));
        return finish(base, failures, Value::Null);
    }
    let run =
        match spawn(Command::new(bwrap).args(sandbox_args(&workspace, &outside, SANDBOX_SCRIPT))) {
            Ok(run) => run,
            Err(error) => {
                failures.push(error);
                return finish(base, failures, Value::Null);
            }
        };
    let facts: serde_json::Map<String, Value> = run
        .stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), json!(value.trim_end_matches(','))))
        .collect();
    let fact = |key: &str| facts.get(key).and_then(Value::as_str).unwrap_or("");
    if run.code != Some(0) || fact("done") != "yes" {
        failures.push("the probe sandbox did not start and exit cleanly".into());
    } else {
        // An identity map of the whole range means no new user namespace.
        if fact("uid_map").is_empty() || fact("uid_map") == "0 0 4294967295" {
            failures.push("no user namespace".into());
        }
        if fact("interfaces") != "lo" {
            failures.push(format!(
                "network interfaces besides lo: {}",
                fact("interfaces")
            ));
        }
        // bwrap itself sets PWD to the --chdir directory.
        let mut allowed: Vec<&str> = SANDBOX_ENV.iter().map(|(name, _)| *name).collect();
        allowed.push("PWD");
        let leaked: Vec<&str> = fact("env")
            .split(',')
            .filter(|name| !name.is_empty() && !allowed.contains(name))
            .collect();
        if !leaked.is_empty() {
            failures.push(format!("environment leaked: {}", leaked.join(",")));
        }
        // Without --new-session the session leader is outside the pid
        // namespace, so the session id reads as 0 inside it.
        if matches!(fact("session"), "" | "0") {
            failures.push("--new-session did not start a new session".into());
        }
        if fact("workspace_write") != "ok"
            || fs::read_to_string(workspace.join("inside")).ok().as_deref() != Some("inside\n")
        {
            failures.push("the workspace mount is not writable from inside".into());
        }
        if fact("outside_write") != "refused" || outside.join("escape").exists() {
            failures.push("a write outside the workspace was not refused".into());
        }
        if fact("usr_write") != "refused" {
            failures.push("a write to /usr was not refused".into());
        }
    }
    let mut probe = run.json();
    probe["facts"] = Value::Object(facts);
    finish(base, failures, probe)
}

fn finish(mut base: Value, failures: Vec<String>, probe: Value) -> Value {
    base["status"] = json!(if failures.is_empty() {
        "available"
    } else {
        "unavailable"
    });
    base["failures"] = json!(failures);
    base["probe"] = probe;
    base
}

fn cgroup_probe(programs: &Programs, bwrap_available: bool) -> Value {
    let controllers = read_trimmed("/sys/fs/cgroup/cgroup.controllers");
    let own = read_trimmed("/proc/self/cgroup");
    let base = json!({
        "cgroup2": controllers.is_some(),
        "root_controllers": controllers,
        "own_cgroup": own,
        "systemd_run": programs.systemd_run,
        "xdg_runtime_dir_set": std::env::var_os("XDG_RUNTIME_DIR").is_some(),
        "dbus_session_bus_address_set": std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some(),
        "tasks_max_requested": TASKS_MAX,
    });
    let unavailable = |mut base: Value, reason: String, probe: Value| {
        base["status"] = json!("unavailable");
        base["reason"] = json!(reason);
        base["probe"] = probe;
        base
    };
    if !programs.systemd_run.is_file() {
        let reason = format!("{} is missing", programs.systemd_run.display());
        return unavailable(base, reason, Value::Null);
    }
    let scope = |command: &mut Command| {
        command
            .arg("--user")
            .arg("--scope")
            .arg("--quiet")
            .arg("--collect")
            .arg(format!("--property=TasksMax={TASKS_MAX}"))
            .arg("--");
    };
    let mut command = Command::new(&programs.systemd_run);
    scope(&mut command);
    command.args([
        "/bin/sh",
        "-c",
        "c=$(cut -d: -f3 /proc/self/cgroup); echo \"$c\"; cat \"/sys/fs/cgroup$c/pids.max\"",
    ]);
    let run = match spawn(&mut command) {
        Ok(run) => run,
        Err(error) => return unavailable(base, error, Value::Null),
    };
    let mut lines = run.stdout.lines();
    let mut probe = run.json();
    probe["scope_cgroup"] = json!(lines.next());
    probe["pids_max"] = json!(lines.next());
    if run.code != Some(0) {
        let reason = "systemd-run --user --scope failed (no delegated user manager?)".into();
        return unavailable(base, reason, probe);
    }
    if probe["pids_max"] != json!(TASKS_MAX.to_string()) {
        let reason = "the scope started but pids.max does not hold TasksMax".into();
        return unavailable(base, reason, probe);
    }
    // The shape D8 will use: bwrap inside the limited scope.
    if bwrap_available {
        let mut command = Command::new(&programs.systemd_run);
        scope(&mut command);
        command.arg(&programs.bwrap).args([
            "--unshare-all",
            "--unshare-user",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
        ]);
        command.args(system_mounts()).arg("/bin/true");
        probe["bwrap_in_scope"] = match spawn(&mut command) {
            Ok(run) => run.json(),
            Err(error) => json!({ "error": error }),
        };
    }
    let mut base = base;
    base["status"] = json!("available");
    base["probe"] = probe;
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bubblewrap_versions() {
        assert_eq!(parse_version("bubblewrap 0.9.0\n"), Some((0, 9, 0)));
        assert_eq!(parse_version("bubblewrap 0.11"), Some((0, 11, 0)));
        assert_eq!(parse_version("bwrap 0.9.0"), None);
        assert_eq!(parse_version("bubblewrap x.1"), None);
        assert!(parse_version("bubblewrap 0.4.1").unwrap() < MIN_BWRAP);
        assert!(parse_version("bubblewrap 0.10.0").unwrap() > MIN_BWRAP);
    }

    #[test]
    fn sandbox_requests_a_user_namespace_explicitly() {
        let args = sandbox_args(Path::new("/w"), Path::new("/o"), "true");
        for flag in [
            "--unshare-all",
            "--unshare-user",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
        ] {
            assert!(args.iter().any(|arg| arg == flag), "missing {flag}");
        }
        assert!(!args.iter().any(|arg| arg == "--share-net"));
    }
}
