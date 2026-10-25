//! `harness doctor`: probe the host capabilities Milestone 3 depends on.
//!
//! This is the Milestone 3 step 1 spike. It reports what the host offers and
//! never changes host configuration: it creates and removes a private
//! temporary directory, starts probe sandboxes, and starts one transient
//! systemd user scope. Every check fails closed: anything the doctor cannot
//! confirm is reported as unavailable, with the reason.

use std::{
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read},
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use rustix::{
    fs::{CWD, Mode, OFlags, ResolveFlags, openat2},
    io::Errno,
    process::{getegid, geteuid},
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
/// Where to read when the shell sandbox is unavailable.
const SANDBOX_HELP: &str = "docs/development.md#when-the-shell-sandbox-is-unavailable";
/// The only variables the sandbox may see, with their fixed values.
const SANDBOX_ENV: [(&str, &str); 4] = [
    ("PATH", "/usr/bin:/bin"),
    ("HOME", "/home/sandbox"),
    ("LANG", "C.UTF-8"),
    ("TERM", "dumb"),
];

/// Runs inside the probe sandbox and prints one `key=value` line per fact.
/// After `ready=yes` it waits for its stdin to close, so the doctor can
/// inspect the live sandbox from the host.
const SANDBOX_SCRIPT: &str = r#"
echo "uid_map=$(tr -s ' ' < /proc/self/uid_map | sed 's/^ //')"
echo "interfaces=$(sed -n '3,$p' /proc/net/dev | cut -d: -f1 | tr -d ' ' | tr '\n' ',')"
echo "env=$(tr '\0' '\n' < /proc/$$/environ | cut -d= -f1 | sort | tr '\n' ',')"
echo "session=$(cut -d' ' -f6 /proc/$$/stat)"
(echo inside > /workspace/inside) 2>/dev/null && echo workspace_write=ok || echo workspace_write=refused
(echo escape > /outside/escape) 2>/dev/null && echo outside_write=ok || echo outside_write=refused
(echo escape > /usr/harness-doctor-escape) 2>/dev/null && echo usr_write=ok || echo usr_write=refused
echo ready=yes
read -r _ || true
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
    spawn_with(command, None)
}

/// Like [`spawn`]. With `on_ready`, the child's stdin is a pipe held open
/// until it prints `ready=yes`; `on_ready` then runs with the child's pid
/// while the child waits, and closing stdin lets it finish.
fn spawn_with(command: &mut Command, on_ready: Option<&mut dyn FnMut(u32)>) -> Result<Run, String> {
    let mut child = command
        .stdin(if on_ready.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start {:?}: {error}", command.get_program()))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let (lines, received) = mpsc::channel();
    let out = thread::spawn(move || {
        let mut text = String::new();
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            text.push_str(&line);
            text.push('\n');
            let _ = lines.send(line);
        }
        text
    });
    let err = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    if let Some(on_ready) = on_ready {
        // Stops at ready, at the end of stdout, or at the deadline.
        while let Ok(line) =
            received.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            if line == "ready=yes" {
                on_ready(child.id());
                break;
            }
        }
    }
    drop(stdin);
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
            let failure = format!("cannot stat {}: {error}", bwrap.display());
            return finish(json!({ "path": bwrap }), vec![failure], Value::Null);
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
    // Checked from the host while the sandbox waits after `ready=yes`.
    let mut host_maps = Vec::new();
    let mut map_failures = Vec::new();
    let mut inspect = |pid: u32| {
        let sandboxed = descendants(pid);
        if sandboxed.is_empty() {
            map_failures.push("no sandbox process found to check its id maps from the host".into());
        }
        for pid in sandboxed {
            let uid = map_check(pid, "uid_map", "euid", geteuid().as_raw());
            let gid = map_check(pid, "gid_map", "egid", getegid().as_raw());
            host_maps.push(json!({ "pid": pid, "uid_map": uid.0, "gid_map": gid.0 }));
            map_failures.extend(uid.1.into_iter().chain(gid.1));
        }
    };
    let run = spawn_with(
        Command::new(bwrap).args(sandbox_args(&workspace, &outside, SANDBOX_SCRIPT)),
        Some(&mut inspect),
    );
    let run = match run {
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
    if run.code != Some(0) || fact("ready") != "yes" {
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
        failures.extend(map_failures);
    }
    let mut probe = run.json();
    probe["facts"] = Value::Object(facts);
    probe["host_maps"] = json!(host_maps);
    finish(base, failures, probe)
}

fn finish(mut base: Value, failures: Vec<String>, probe: Value) -> Value {
    base["status"] = json!(if failures.is_empty() {
        "available"
    } else {
        "unavailable"
    });
    if !failures.is_empty() {
        base["help"] = json!(SANDBOX_HELP);
    }
    base["failures"] = json!(failures);
    base["probe"] = probe;
    base
}

/// Every descendant of `root`, found through the parent pids in `/proc`.
fn descendants(root: u32) -> Vec<u32> {
    let parents: Vec<(u32, u32)> = fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            // The command name may hold spaces; the fields after it do not.
            let (_, rest) = stat.rsplit_once(')')?;
            let ppid = rest.split_whitespace().nth(1)?.parse().ok()?;
            Some((pid, ppid))
        })
        .collect();
    let mut found = vec![root];
    let mut next = 0;
    while let Some(&parent) = found.get(next) {
        for &(pid, ppid) in &parents {
            if ppid == parent && !found.contains(&pid) {
                found.push(pid);
            }
        }
        next += 1;
    }
    found.remove(0);
    found
}

/// Read `/proc/<pid>/<file>` from the host and require exactly one line
/// mapping one id to `outside`, the invoking user's effective id. Seen from
/// the host, the second field is relative to the host's namespace, unlike the
/// map the sandbox reads about itself.
fn map_check(pid: u32, file: &str, id: &str, outside: u32) -> (Value, Option<String>) {
    let text = match fs::read_to_string(format!("/proc/{pid}/{file}")) {
        Ok(text) => text,
        Err(error) => {
            let failure = format!("cannot read {file} of sandbox pid {pid} from the host: {error}");
            return (Value::Null, Some(failure));
        }
    };
    let lines: Vec<Vec<&str>> = text
        .lines()
        .map(|line| line.split_whitespace().collect())
        .collect();
    let outside = outside.to_string();
    let ok = matches!(lines.as_slice(), [line] if line.len() == 3 && line[1] == outside && line[2] == "1");
    let shown: Vec<String> = lines.iter().map(|line| line.join(" ")).collect();
    let failure = (!ok).then(|| {
        format!(
            "{file} seen from the host is not one id mapped to {id} {outside}: pid {pid}: {}",
            shown.join("; ")
        )
    });
    (json!(shown), failure)
}

fn cgroup_probe(programs: &Programs, bwrap_available: bool) -> Value {
    let controllers = read_trimmed("/sys/fs/cgroup/cgroup.controllers");
    let own = read_trimmed("/proc/self/cgroup");
    let base = json!({
        "cgroup2": controllers.is_some(),
        "root_controllers": controllers,
        "own_cgroup": own,
        "systemd_run": programs.systemd_run,
        "dbus_session_bus_address_set": std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some(),
        "tasks_max_requested": TASKS_MAX,
    });
    let bus = user_bus(
        std::env::var_os("XDG_RUNTIME_DIR"),
        Path::new("/run/user"),
        geteuid().as_raw(),
    );
    let mut base = base;
    base["bus_source"] = json!(bus.source);
    base["bus_runtime_dir"] = json!(bus.runtime_dir);
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
    if let Some(reason) = bus.reason {
        return unavailable(base, reason, Value::Null);
    }
    let scope = |command: &mut Command| {
        // Only this child sees a derived runtime directory.
        if let Some(dir) = &bus.runtime_dir {
            command.env("XDG_RUNTIME_DIR", dir);
        }
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
    base["status"] = json!("available");
    base["probe"] = probe;
    base
}

/// Where `systemd-run --user` finds the user bus.
struct Bus {
    /// `env`, `derived` or `none`.
    source: &'static str,
    /// A derived runtime directory, passed only to the systemd-run child.
    runtime_dir: Option<PathBuf>,
    /// Why there is no bus, when `source` is `none`.
    reason: Option<String>,
}

/// Use `XDG_RUNTIME_DIR` when it is set. Otherwise derive
/// `<run_user>/<euid>` and accept it only when [`runtime_dir_check`] passes;
/// there is no other fallback.
fn user_bus(env: Option<OsString>, run_user: &Path, euid: u32) -> Bus {
    if env.is_some_and(|value| !value.is_empty()) {
        return Bus {
            source: "env",
            runtime_dir: None,
            reason: None,
        };
    }
    let dir = run_user.join(euid.to_string());
    match runtime_dir_check(&dir, euid) {
        Ok(()) => Bus {
            source: "derived",
            runtime_dir: Some(dir),
            reason: None,
        },
        Err(reason) => Bus {
            source: "none",
            runtime_dir: None,
            reason: Some(format!(
                "XDG_RUNTIME_DIR is unset and {} was not accepted: {reason}",
                dir.display()
            )),
        },
    }
}

/// The directory must be a real directory owned by `euid` and not group- or
/// world-writable, and `bus` in it a socket owned by `euid`.
fn runtime_dir_check(dir: &Path, euid: u32) -> Result<(), String> {
    let metadata = fs::symlink_metadata(dir).map_err(|error| error.to_string())?;
    if !metadata.is_dir() {
        return Err("not a directory".into());
    }
    if metadata.uid() != euid {
        return Err(format!("owned by uid {}, not euid {euid}", metadata.uid()));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(format!(
            "mode {:o} is group- or world-writable",
            metadata.mode() & 0o7777
        ));
    }
    let bus = fs::symlink_metadata(dir.join("bus")).map_err(|error| format!("bus: {error}"))?;
    if !bus.file_type().is_socket() {
        return Err("bus is not a socket".into());
    }
    if bus.uid() != euid {
        return Err(format!(
            "bus is owned by uid {}, not euid {euid}",
            bus.uid()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `/run/user` stand-in holding `<euid>/bus` as a live socket.
    fn run_user() -> (tempfile::TempDir, PathBuf, std::os::unix::net::UnixListener) {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(geteuid().as_raw().to_string());
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = std::os::unix::net::UnixListener::bind(dir.join("bus")).unwrap();
        (temp, dir, socket)
    }

    #[test]
    fn derives_a_valid_runtime_dir_when_unset() {
        let (temp, dir, _socket) = run_user();
        let bus = user_bus(None, temp.path(), geteuid().as_raw());
        assert_eq!(bus.source, "derived");
        assert_eq!(bus.runtime_dir, Some(dir));
        assert_eq!(bus.reason, None);
        let empty = user_bus(Some("".into()), temp.path(), geteuid().as_raw());
        assert_eq!(empty.source, "derived");
    }

    #[test]
    fn an_inherited_runtime_dir_is_used_as_is() {
        let bus = user_bus(Some("/run/user/1".into()), Path::new("/nonexistent"), 1);
        assert_eq!(bus.source, "env");
        assert_eq!(bus.runtime_dir, None);
    }

    #[test]
    fn refuses_a_runtime_dir_of_another_owner() {
        let (temp, dir, _socket) = run_user();
        let other = geteuid().as_raw() + 1;
        fs::rename(&dir, temp.path().join(other.to_string())).unwrap();
        let bus = user_bus(None, temp.path(), other);
        assert_eq!(bus.source, "none");
        assert_eq!(bus.runtime_dir, None);
        assert!(bus.reason.unwrap().contains("not euid"));
    }

    #[test]
    fn refuses_a_world_writable_runtime_dir() {
        let (temp, dir, _socket) = run_user();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        let bus = user_bus(None, temp.path(), geteuid().as_raw());
        assert_eq!(bus.source, "none");
        assert!(bus.reason.unwrap().contains("world-writable"));
    }

    #[test]
    fn refuses_a_runtime_dir_without_a_bus_socket() {
        let (temp, dir, socket) = run_user();
        drop(socket);
        fs::remove_file(dir.join("bus")).unwrap();
        let missing = user_bus(None, temp.path(), geteuid().as_raw());
        assert_eq!(missing.source, "none");
        assert!(missing.reason.unwrap().contains("bus:"));
        fs::write(dir.join("bus"), b"").unwrap();
        let file = user_bus(None, temp.path(), geteuid().as_raw());
        assert_eq!(file.source, "none");
        assert!(file.reason.unwrap().contains("not a socket"));
    }

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
