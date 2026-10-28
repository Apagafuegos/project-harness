//! S02: real Linux OS-boundary tests. Required by check.sh, never silently skip.
//! Run explicitly: cargo test -p harness-core --test sandbox -- --ignored
use harness_core::{
    session::{ArtifactRef, CallRef, ContentBlock, Digest, ResultCode, ToolStatus},
    tools::{
        CancelToken, ExecutionOutcome, FileFacts, FileKind, Plan, PolicySnapshot, Protection,
        Refusal, Registry, Sandbox, SandboxConfig, WorkspaceIdentity, WorkspacePath, WorkspaceView,
    },
};
use rustix::io::{FdFlags, fcntl_setfd};
use serde_json::{Value, json};
use std::{
    fs, io,
    net::{TcpListener, TcpStream, ToSocketAddrs},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

fn backend() -> &'static Sandbox {
    static BACKEND: OnceLock<Sandbox> = OnceLock::new();
    BACKEND.get_or_init(|| {
        let sandbox = Sandbox::probe(SandboxConfig::default()).expect("S02 requires a working sandbox; only check.sh's HARNESS_ALLOW_NO_SANDBOX=1 can opt out");
        eprintln!("S02 bwrap={} process_limit_gap={:?}", sandbox.probe_report()["bubblewrap"]["version"], sandbox.process_limit_gap());
        sandbox
    })
}
struct View(WorkspaceIdentity);
impl WorkspaceView for View {
    fn identity(&self) -> &WorkspaceIdentity {
        &self.0
    }
    fn inspect(&self, path: &WorkspacePath) -> Result<FileFacts, Refusal> {
        let metadata = fs::symlink_metadata(Path::new(&self.0.0).join(path.as_str()));
        let (kind, size, nlink) = match metadata {
            Ok(m) => (
                if m.is_dir() {
                    FileKind::Directory
                } else {
                    FileKind::Regular
                },
                m.len(),
                m.nlink(),
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (FileKind::Absent, 0, 0),
            Err(e) => panic!("inspect: {e}"),
        };
        Ok(FileFacts {
            kind,
            size,
            nlink,
            digest: (kind == FileKind::Regular).then(|| Digest::of(b"fixture")),
            protection: Protection::None,
        })
    }
    fn read_text(&self, _: &WorkspacePath, _: u64) -> Result<String, Refusal> {
        panic!("bash planning never reads text")
    }
}
struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::write(root.path().join(".git/config"), b"git-secret").unwrap();
        fs::create_dir(root.path().join("private")).unwrap();
        fs::write(
            root.path().join("private/token"),
            b"protected-directory-secret",
        )
        .unwrap();
        fs::write(root.path().join("secret"), b"protected-file-secret").unwrap();
        let exe = std::env::current_exe().unwrap();
        let payload = root.path().join("payload");
        if fs::hard_link(&exe, &payload).is_err() {
            fs::copy(exe, payload).unwrap();
        }
        Self(root)
    }
    fn root(&self) -> &Path {
        self.0.path()
    }
    fn plan(&self, command: &str, timeout: u64, ro: bool, protected: &[&str]) -> Plan {
        self.plan_at(command, timeout, ro, protected, ".")
    }
    fn plan_at(
        &self,
        command: &str,
        timeout: u64,
        ro: bool,
        protected: &[&str],
        cwd: &str,
    ) -> Plan {
        let mut config: Value = serde_json::from_slice(include_bytes!("../../../docs/rfcs/001/examples/artifacts/7e66535a506e1c533025006e25e2fced162ef19d026d0db999154218ae7a30ea")).unwrap();
        config["protected_paths"] = json!(protected);
        if ro {
            config["workspace_mutation"] = json!("deny");
        }
        let bytes = serde_json::to_vec(&config).unwrap();
        let policy = PolicySnapshot::parse(
            ArtifactRef {
                sha256: Digest::of(&bytes),
                bytes: bytes.len() as u64,
            },
            &bytes,
        )
        .unwrap();
        let registry = Registry::builtin();
        let tool = registry.resolve("bash", &policy).unwrap();
        let operation = tool
            .validate(&json!({"command":command,"timeout_ms":timeout,"cwd":cwd}))
            .unwrap();
        tool.plan(
            &operation,
            &CallRef {
                response_entry_id: "019a0000-0000-7000-8000-00000000019a".parse().unwrap(),
                call_id: "019a0000-0000-7000-8000-00000000019b".parse().unwrap(),
            },
            &View(WorkspaceIdentity(self.root().to_string_lossy().into())),
            &policy,
        )
        .unwrap()
    }
    fn run(&self, command: &str) -> ExecutionOutcome {
        backend().run(
            &self.plan(command, 5000, false, &[]),
            self.root(),
            &CancelToken::new(),
        )
    }
    fn payload(&self, kind: &str, protected: &[&str], ro: bool) -> ExecutionOutcome {
        fs::write(self.root().join(".s02-kind"), kind).unwrap();
        backend().run(
            &self.plan(
                "/workspace/payload --exact payload --nocapture",
                5000,
                ro,
                protected,
            ),
            self.root(),
            &CancelToken::new(),
        )
    }
}
fn text(outcome: &ExecutionOutcome) -> String {
    outcome
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}
fn succeeded(outcome: &ExecutionOutcome) {
    assert!(serde_json::to_vec(&outcome.content).unwrap().len() <= 32768);
    assert_eq!(outcome.status, ToolStatus::Succeeded, "{outcome:?}");
    assert_eq!(outcome.code, None);
}
fn failed(outcome: &ExecutionOutcome, status: ToolStatus, code: ResultCode) {
    assert!(serde_json::to_vec(&outcome.content).unwrap().len() <= 32768);
    assert_eq!(outcome.status, status, "{outcome:?}");
    assert_eq!(outcome.code, Some(code), "{outcome:?}");
}
fn denied_write(path: &str, errno: i32) {
    let error = fs::write(path, "must-not-write").unwrap_err();
    assert_eq!(error.raw_os_error(), Some(errno), "{path}: {error}");
}

// An ordinary (non-ignored) test entry reused as an in-sandbox Rust payload.
// It is a no-op when run by the host's normal cargo test invocation.
#[test]
fn payload() {
    let Ok(kind) = fs::read_to_string("/workspace/.s02-kind") else {
        return;
    };
    match kind.as_str() {
        "network" => {
            assert!(
                "example.com:443".to_socket_addrs().is_err(),
                "DNS unexpectedly resolved"
            );
            let error = TcpStream::connect_timeout(
                &"1.1.1.1:443".parse().unwrap(),
                Duration::from_millis(250),
            )
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(101), "external TCP: {error}"); // ENETUNREACH
            let port = fs::read_to_string("/workspace/port").unwrap();
            let error = TcpStream::connect_timeout(
                &format!("127.0.0.1:{port}").parse().unwrap(),
                Duration::from_millis(250),
            )
            .unwrap_err();
            assert_eq!(
                error.kind(),
                io::ErrorKind::ConnectionRefused,
                "host loopback: {error}"
            );
            let mut interfaces: Vec<_> = fs::read_dir("/sys/class/net")
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            // sysfs is deliberately not mounted; /proc/net/dev is authoritative.
            assert!(interfaces.is_empty());
            let dev = fs::read_to_string("/proc/net/dev").unwrap();
            interfaces = dev
                .lines()
                .skip(2)
                .map(|l| l.split(':').next().unwrap().trim().into())
                .collect();
            assert_eq!(interfaces, [std::ffi::OsString::from("lo")]);
        }
        "secrets" => {
            let paths = fs::read_to_string("/workspace/host-secret-paths").unwrap();
            for path in paths
                .lines()
                .chain(["/root", "/home/ubuntu/.ssh", "/home/sandbox/.ssh"])
            {
                assert_eq!(
                    fs::metadata(path).unwrap_err().kind(),
                    io::ErrorKind::NotFound,
                    "{path}"
                );
                assert!(fs::read(path).is_err(), "read host secret: {path}");
            }
        }
        "outside-write" => {
            denied_write("/usr/harness-s02-escape", 30);
            denied_write("/unmounted/escape", 2);
        }
        "git-write" => denied_write("/workspace/.git/config", 30),
        "masked" => {
            assert_eq!(fs::read_dir("/workspace/private").unwrap().count(), 0);
            // ro-bind is nodev outside /dev: the null-device mask denies
            // access with EACCES, in addition to hiding the original bytes.
            use std::os::unix::fs::FileTypeExt;
            assert!(
                fs::metadata("/workspace/secret")
                    .unwrap()
                    .file_type()
                    .is_char_device()
            );
            assert_eq!(
                fs::read("/workspace/secret").unwrap_err().raw_os_error(),
                Some(13)
            );
            denied_write("/workspace/private/new", 30);
            denied_write("/workspace/secret", 13);
        }
        "readonly" => denied_write("/workspace/new", 30),
        "environment" => {
            assert!(
                std::env::var_os("HARNESS_S02_SECRET").is_none(),
                "secret environment leaked"
            );
            let initial = fs::read("/proc/self/environ").unwrap();
            let names: Vec<_> = initial
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| std::str::from_utf8(s.split(|b| *b == b'=').next().unwrap()).unwrap())
                .collect();
            for name in names {
                assert!(
                    [
                        "PATH", "HOME", "TMPDIR", "LANG", "TERM", "PWD", "SHLVL", "_"
                    ]
                    .contains(&name),
                    "unexpected environment key {name}"
                );
            }
            assert_eq!(std::env::var("HOME").unwrap(), "/home/sandbox");
            assert_eq!(std::env::var("TMPDIR").unwrap(), "/tmp");
        }
        "descriptors" => {
            for entry in fs::read_dir("/proc/self/fd").unwrap() {
                let entry = entry.unwrap();
                let fd: i32 = entry.file_name().to_str().unwrap().parse().unwrap();
                let target = fs::read_link(entry.path()).unwrap();
                assert!(
                    fd <= 2 || target.to_string_lossy().ends_with("/fd"),
                    "harness descriptor leaked: {fd} -> {}",
                    target.display()
                );
            }
        }
        "proc" => {
            for entry in fs::read_dir("/proc").unwrap().flatten() {
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|s| s.parse::<u32>().is_ok())
                {
                    let link = fs::read_link(entry.path().join("ns/pid")).unwrap();
                    assert_eq!(link, fs::read_link("/proc/self/ns/pid").unwrap());
                    let cmd = fs::read(entry.path().join("cmdline")).unwrap();
                    assert!(
                        !cmd.windows(b"harness-host-sentinel".len())
                            .any(|s| s == b"harness-host-sentinel")
                    );
                }
            }
            let host_pid = fs::read_to_string("/workspace/host-pid").unwrap();
            assert!(
                !Path::new(&format!("/proc/{host_pid}")).exists(),
                "host process visible"
            );
        }
        unknown => panic!("unknown payload {unknown}"),
    }
}

#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_probe_required() {
    let _ = backend();
}

#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_network_dns_external_tcp_host_loopback_and_only_lo() {
    let f = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    fs::write(
        f.root().join("port"),
        listener.local_addr().unwrap().port().to_string(),
    )
    .unwrap();
    succeeded(&f.payload("network", &[], false));
}
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_host_secrets_and_data_root_hidden() {
    let f = Fixture::new();
    let secrets = tempfile::tempdir().unwrap();
    fs::write(secrets.path().join("credential"), "secret").unwrap();
    fs::write(
        f.root().join("host-secret-paths"),
        format!(
            "{}\n{}/credential\n{}\n",
            secrets.path().display(),
            secrets.path().display(),
            PathBuf::from(std::env::var_os("HOME").expect("test process HOME"))
                .join(".ssh")
                .display()
        ),
    )
    .unwrap();
    succeeded(&f.payload("secrets", &[], false));
}
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_outside_workspace_writes_denied() {
    succeeded(&Fixture::new().payload("outside-write", &[], false));
}
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_git_write_denied() {
    let f = Fixture::new();
    succeeded(&f.payload("git-write", &[], false));
    assert_eq!(
        fs::read(f.root().join(".git/config")).unwrap(),
        b"git-secret"
    );
}
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_masked_directory_and_file_denied() {
    let f = Fixture::new();
    succeeded(&f.payload("masked", &["private", "secret"], false));
    assert_eq!(
        fs::read(f.root().join("secret")).unwrap(),
        b"protected-file-secret"
    );
    assert_eq!(
        fs::read(f.root().join("private/token")).unwrap(),
        b"protected-directory-secret"
    );
}
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_readonly_workspace_write_denied() {
    succeeded(&Fixture::new().payload("readonly", &[], true));
}

#[test]
fn environment_driver() {
    if std::env::var_os("HARNESS_S02_SECRET").is_none() {
        return;
    }
    succeeded(&Fixture::new().payload("environment", &[], false));
}
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_secret_environment_not_inherited() {
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "environment_driver", "--nocapture"])
        .env("HARNESS_S02_SECRET", "must-not-leak")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_harness_descriptors_not_inherited() {
    let _ = backend();
    let secret = tempfile::NamedTempFile::new().unwrap();
    // Explicitly require close-on-exec on owned harness descriptors.
    fcntl_setfd(&secret, FdFlags::CLOEXEC).unwrap();
    assert!(secret.as_raw_fd() > 2);
    let f = Fixture::new();
    succeeded(&f.payload("descriptors", &[], false));
    let direct = Sandbox::probe(SandboxConfig {
        systemd_run: "/nonexistent/systemd-run".into(),
        ..SandboxConfig::default()
    })
    .unwrap();
    succeeded(&direct.run(
        &f.plan(
            "/workspace/payload --exact payload --nocapture",
            5000,
            false,
            &[],
        ),
        f.root(),
        &CancelToken::new(),
    ));
}
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_new_session_prevents_tiocsti_injection() {
    // Python is a sandbox workload, not a project toolchain. Its stdlib supplies
    // the ioctl unavailable in rustix without forbidden dependency changes.
    let result = Fixture::new().run(
        r#"/usr/bin/python3 -c 'import os,fcntl,termios,errno
try:
 f=os.open("/dev/tty",os.O_RDWR)
except OSError as e:
 assert e.errno==errno.ENXIO,e
else:
 try: fcntl.ioctl(f,termios.TIOCSTI,b"x")
 except OSError as e: assert e.errno in (errno.EPERM,errno.EIO,errno.ENOTTY),e
 else: raise AssertionError("TIOCSTI succeeded")
try: fcntl.ioctl(0,termios.TIOCSTI,b"x")
except OSError as e: assert e.errno in (errno.EPERM,errno.EIO,errno.ENOTTY),e
else: raise AssertionError("stdin TIOCSTI succeeded")'"#,
    );
    succeeded(&result);
}
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_proc_only_namespace_processes() {
    let f = Fixture::new();
    fs::write(f.root().join("host-pid"), std::process::id().to_string()).unwrap();
    succeeded(&f.payload("proc", &[], false));
}
fn assert_namespace_gone(f: &Fixture) {
    let namespace = fs::read_link(f.root().join("namespace-link")).unwrap_or_else(|_| {
        PathBuf::from(
            fs::read_to_string(f.root().join("namespace"))
                .expect("workload started")
                .trim(),
        )
    });
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        if let Ok(link) = fs::read_link(entry.path().join("ns/pid")) {
            assert_ne!(
                link,
                namespace,
                "sandbox process survived at {}",
                entry.path().display()
            );
        }
    }
}
const NAMESPACE: &str = "readlink /proc/self/ns/pid > /workspace/namespace; ";
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_background_grandchild_cleaned_after_success() {
    let f = Fixture::new();
    succeeded(&f.run(&format!(
        "{NAMESPACE}(setsid /bin/bash -c 'sleep 30 & wait') & sleep 0.05; echo done"
    )));
    assert_namespace_gone(&f);
}
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_timeout_cleans_grandchildren_and_term_ignoring_process() {
    let f = Fixture::new();
    let command = format!("{NAMESPACE}trap '' TERM; (trap '' TERM; sleep 30 & wait) & wait");
    let outcome = backend().run(
        &f.plan(&command, 200, false, &[]),
        f.root(),
        &CancelToken::new(),
    );
    failed(&outcome, ToolStatus::TimedOut, ResultCode::Timeout);
    assert!(
        outcome.duration_ms.unwrap() >= 2000 && outcome.duration_ms.unwrap() < 5000,
        "{outcome:?}"
    );
    assert_namespace_gone(&f);
}
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_cancel_cleans_grandchildren() {
    let f = Fixture::new();
    let cancel = CancelToken::new();
    let token = cancel.clone();
    let root = f.root().to_owned();
    let task = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.join("namespace").exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        token.cancel();
    });
    let outcome = backend().run(
        &f.plan(
            &format!("{NAMESPACE}(sleep 30 & wait) & wait"),
            5000,
            false,
            &[],
        ),
        f.root(),
        &cancel,
    );
    task.join().unwrap();
    assert!(outcome.duration_ms.unwrap() < 5000, "{outcome:?}");
    failed(&outcome, ToolStatus::Cancelled, ResultCode::Cancelled);
    assert_namespace_gone(&f);
}
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_fork_bomb_deadline_cleanup_and_tasksmax_when_available() {
    let f = Fixture::new();
    let sandbox = Sandbox::probe(SandboxConfig {
        kill_grace: Duration::ZERO,
        ..SandboxConfig::default()
    })
    .unwrap();
    let outcome = sandbox.run(
        &f.plan(
            &format!("{NAMESPACE}bomb() {{ bomb & bomb & wait; }}; bomb"),
            if sandbox.process_limit_gap().is_none() {
                1000
            } else {
                250
            },
            false,
            &[],
        ),
        f.root(),
        &CancelToken::new(),
    );
    failed(&outcome, ToolStatus::TimedOut, ResultCode::Timeout);
    assert!(outcome.duration_ms.unwrap() < 3000, "{outcome:?}");
    assert_namespace_gone(&f);
    if sandbox.process_limit_gap().is_none() {
        assert!(
            text(&outcome).contains("Resource temporarily unavailable"),
            "TasksMax did not stop forks: {outcome:?}"
        );
    }
}
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_absent_mask_creation_is_failed_without_rollback() {
    let f = Fixture::new();
    let outcome = backend().run(
        &f.plan("echo created > absent", 5000, false, &["absent"]),
        f.root(),
        &CancelToken::new(),
    );
    failed(&outcome, ToolStatus::Failed, ResultCode::ProtectedPath);
    assert_eq!(fs::read(f.root().join("absent")).unwrap(), b"created\n");
    fs::remove_file(f.root().join("absent")).unwrap();
    let interrupted = backend().run(
        &f.plan("echo created > absent; sleep 30", 200, false, &["absent"]),
        f.root(),
        &CancelToken::new(),
    );
    failed(&interrupted, ToolStatus::Failed, ResultCode::ProtectedPath);
    assert_eq!(fs::read(f.root().join("absent")).unwrap(), b"created\n");
}
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_exit_status_signal_and_bounded_output() {
    let f = Fixture::new();
    succeeded(&f.run("echo stdout; echo stderr >&2"));
    let outcome = f.run("exit 7");
    failed(&outcome, ToolStatus::Failed, ResultCode::NonzeroExit);
    assert_eq!(outcome.exit_code, Some(7));
    let outcome = f.run("kill -TERM $$");
    failed(&outcome, ToolStatus::Failed, ResultCode::NonzeroExit);
    assert!(text(&outcome).contains("signal 15"));
    let outcome = f.run("printf head; head -c 40000 /dev/zero | tr '\\0' x; printf tail >&2");
    succeeded(&outcome);
    assert!(
        outcome.output_truncated
            && text(&outcome).len() <= 32768
            && text(&outcome).contains("truncated")
    );
    assert_eq!(outcome.artifacts.iter().map(Vec::len).sum::<usize>(), 40008);
    let outcome = f.run("head -c 67108865 /dev/zero");
    failed(&outcome, ToolStatus::Failed, ResultCode::OutputLimit);
    assert!(outcome.output_truncated);
    assert_eq!(
        outcome.artifacts.iter().map(Vec::len).sum::<usize>(),
        64 * 1024 * 1024
    );
}
#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_fail_closed_missing_relative_and_fake_bwrap() {
    use std::os::unix::fs::PermissionsExt;
    for path in ["bwrap", "/nonexistent/bwrap"] {
        assert!(
            Sandbox::probe(SandboxConfig {
                bwrap: path.into(),
                ..SandboxConfig::default()
            })
            .is_err()
        );
    }
    let f = Fixture::new();
    let fake = f.root().join("fake-bwrap");
    fs::write(&fake, "#!/bin/sh\n[ \"$1\" = --version ] && { echo 'bubblewrap 0.11.1'; exit 0; }\necho 'uid_map=0 0 4294967295'\necho 'interfaces=lo'\necho 'env=PATH,HOME,LANG,TERM,'\necho 'session=1'\necho 'workspace_write=refused'\necho 'outside_write=refused'\necho 'usr_write=refused'\necho 'ready=yes'\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let error = Sandbox::probe(SandboxConfig {
        bwrap: fake,
        ..SandboxConfig::default()
    })
    .unwrap_err();
    assert!(error.reasons.iter().any(|s| s == "no user namespace"));
    assert!(!f.root().join("command-ran").exists());
}

#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_protected_descendants_stay_masked_beneath_git_ro_bind() {
    let f = Fixture::new();
    fs::create_dir(f.root().join(".git/private")).unwrap();
    fs::write(f.root().join(".git/private/token"), "must-stay-hidden").unwrap();
    let plan = f.plan(
        "test -z \"$(ls -A .git/private)\" && ! cat .git/config && ! touch .git/new",
        5000,
        false,
        &[".git/private", ".git/config"],
    );
    succeeded(&backend().run(&plan, f.root(), &CancelToken::new()));
    assert_eq!(
        fs::read_to_string(f.root().join(".git/private/token")).unwrap(),
        "must-stay-hidden"
    );
}

#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_host_id_maps_reject_probe_that_lies_about_its_namespace() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let fake = f.root().join("lying-bwrap");
    fs::write(
        &fake,
        r#"#!/bin/sh
[ "$1" = --version ] && { echo 'bubblewrap 0.11.1'; exit 0; }
while [ $# -gt 0 ]; do [ "$1" = --bind ] && ws=$2; shift; done
(
echo inside > "$ws/inside"
echo 'uid_map=1001 0 1'
echo 'interfaces=lo,'
echo 'env=HOME,LANG,PATH,PWD,TERM,'
echo 'session=1'
echo 'workspace_write=ok'
echo 'outside_write=refused'
echo 'usr_write=refused'
echo 'ready=yes'
exec cat > /dev/null
)
"#,
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let error = Sandbox::probe(SandboxConfig {
        bwrap: fake,
        systemd_run: "/nonexistent/systemd-run".into(),
        ..SandboxConfig::default()
    })
    .unwrap_err();
    assert_eq!(error.reasons.len(), 2, "{error:?}");
    assert!(error.reasons[0].starts_with("uid_map seen from the host is not one id mapped"));
    assert!(error.reasons[1].starts_with("gid_map seen from the host is not one id mapped"));
}

#[test]
fn non_cloexec_descriptor_driver() {
    let Some(mode) = std::env::var_os("HARNESS_S02_FD_DRIVER") else {
        return;
    };
    if mode == "launcher" {
        use rustix::{
            fs::{OFlags, fcntl_getfl, fcntl_setfl},
            io::fcntl_dupfd_cloexec,
        };
        let mut child = Command::new("/bin/sh")
            .args(["-c", "printf token"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pipe = child.stdout.take().unwrap();
        assert!(child.wait().unwrap().success());
        let descriptor = fcntl_dupfd_cloexec(&pipe, 142).unwrap();
        fcntl_setfl(
            &descriptor,
            fcntl_getfl(&descriptor).unwrap() | OFlags::NONBLOCK,
        )
        .unwrap();
        fcntl_setfd(&descriptor, FdFlags::empty()).unwrap();
        let launcher =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/with-clean-fds.sh");
        let result = Command::new(launcher)
            .args(["/bin/sh", "-c"])
            .arg(format!(
                "test ! -e /proc/self/fd/{} && printf sanitized && printf diagnostics >&2",
                descriptor.as_raw_fd()
            ))
            .output();
        fcntl_setfd(&descriptor, FdFlags::CLOEXEC).unwrap();
        let result = result.unwrap();
        assert!(result.status.success(), "{result:?}");
        assert_eq!(result.stdout, b"sanitized");
        assert_eq!(result.stderr, b"diagnostics");
        return;
    }
    let f = Fixture::new();
    let sandbox = backend();
    let descriptor = tempfile::NamedTempFile::new().unwrap();
    fcntl_setfd(&descriptor, FdFlags::empty()).unwrap();
    let result = sandbox.run(
        &f.plan("touch command-ran", 5000, false, &[]),
        f.root(),
        &CancelToken::new(),
    );
    // Restore before assertions so panics do not contaminate another test.
    fcntl_setfd(&descriptor, FdFlags::CLOEXEC).unwrap();
    failed(
        &result,
        ToolStatus::InfrastructureFailure,
        ResultCode::SandboxUnavailable,
    );
    assert!(text(&result).contains("close-on-exec"));
    assert!(!f.root().join("command-ran").exists());
}

#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_probe_descendant_writer_cannot_hold_capture_open() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let fake = f.root().join("writer-bwrap");
    fs::write(&fake, "#!/bin/sh\nif [ \"$1\" = --version ]; then\n/bin/sleep 30 &\necho $! > \"${0}.writer-pid\"\necho 'bubblewrap 0.11.1'\nexit 0\nfi\nexit 1\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let start = Instant::now();
    assert!(
        Sandbox::probe(SandboxConfig {
            bwrap: fake.clone(),
            systemd_run: "/nonexistent/systemd-run".into(),
            ..SandboxConfig::default()
        })
        .is_err()
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    let pid = fs::read_to_string(fake.with_extension("writer-pid")).unwrap();
    while let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat", pid.trim())) {
        if stat
            .rsplit_once(')')
            .unwrap()
            .1
            .trim_start()
            .starts_with('Z')
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "probe writer survived: {stat}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_ci_launcher_closes_inherited_non_cloexec_pipe() {
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "non_cloexec_descriptor_driver", "--nocapture"])
        .env("HARNESS_S02_FD_DRIVER", "launcher")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_unknown_non_cloexec_descriptor_fails_before_execution() {
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "non_cloexec_descriptor_driver", "--nocapture"])
        .env("HARNESS_S02_FD_DRIVER", "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_exact_shell_bytes_cwd_and_recorded_tasksmax_fallback() {
    let f = Fixture::new();
    fs::create_dir(f.root().join("sub dir")).unwrap();
    let plan = f.plan_at(
        "printf '%s\\n' '$literal' \"$HOME\"; pwd; test -n \"$$\"",
        5000,
        false,
        &[],
        "sub dir",
    );
    let scoped = backend().run(&plan, f.root(), &CancelToken::new());
    succeeded(&scoped);
    assert_eq!(
        text(&scoped),
        "$literal\n/home/sandbox\n/workspace/sub dir\n"
    );
    let fallback = Sandbox::probe(SandboxConfig {
        systemd_run: "/nonexistent/systemd-run".into(),
        ..SandboxConfig::default()
    })
    .unwrap();
    assert!(fallback.process_limit_gap().is_some());
    let direct = fallback.run(&plan, f.root(), &CancelToken::new());
    succeeded(&direct);
    assert_eq!(text(&direct), text(&scoped));
}

#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_precancelled_plan_never_starts() {
    let f = Fixture::new();
    let token = CancelToken::new();
    token.cancel();
    let outcome = backend().run(
        &f.plan("touch command-ran", 5000, false, &[]),
        f.root(),
        &token,
    );
    failed(&outcome, ToolStatus::Cancelled, ResultCode::Cancelled);
    assert!(!f.root().join("command-ran").exists());
}

#[test]
#[ignore = "S02 real Linux sandbox"]
fn s02_setup_failures_never_fall_back_to_host() {
    use std::os::unix::fs::symlink;
    assert!(
        Sandbox::probe(SandboxConfig {
            bash: "/nonexistent/bash".into(),
            ..SandboxConfig::default()
        })
        .is_err()
    );
    let f = Fixture::new();
    let bwrap = f.root().join("configured-bwrap");
    symlink("/usr/bin/bwrap", &bwrap).unwrap();
    let sandbox = Sandbox::probe(SandboxConfig {
        bwrap: bwrap.clone(),
        systemd_run: "/nonexistent/systemd-run".into(),
        ..SandboxConfig::default()
    })
    .unwrap();
    fs::remove_file(bwrap).unwrap();
    failed(
        &sandbox.run(
            &f.plan("touch command-ran", 5000, false, &[]),
            f.root(),
            &CancelToken::new(),
        ),
        ToolStatus::InfrastructureFailure,
        ResultCode::SandboxUnavailable,
    );
    let other = Fixture::new();
    let bound = f.plan("touch command-ran", 5000, false, &[]);
    failed(
        &backend().run(&bound, other.root(), &CancelToken::new()),
        ToolStatus::InfrastructureFailure,
        ResultCode::SandboxUnavailable,
    );
    assert!(!other.root().join("command-ran").exists());
    fs::create_dir(f.root().join("cwd")).unwrap();
    let plan = f.plan_at("touch command-ran", 5000, false, &[], "cwd");
    fs::remove_dir(f.root().join("cwd")).unwrap();
    symlink("/", f.root().join("cwd")).unwrap();
    failed(
        &backend().run(&plan, f.root(), &CancelToken::new()),
        ToolStatus::InfrastructureFailure,
        ResultCode::SandboxUnavailable,
    );
    assert!(!f.root().join("command-ran").exists());
}
