use super::{
    Sandbox,
    args::Spec,
    output::{Capture, content},
    unavailable,
};
use crate::{
    session::{ResultCode, ToolStatus},
    tools::{CancelToken, ExecutionOutcome},
};
use rustix::{
    fd::OwnedFd,
    io::{FdFlags, fcntl_setfd},
    process::{
        Pid, PidfdFlags, Signal, WaitOptions, kill_process, pidfd_open, pidfd_send_signal, waitpid,
    },
};
use serde_json::Value;
use std::{
    fs,
    io::{self, Read},
    os::{fd::AsRawFd, unix::net::UnixStream},
    process::{Child, Command, Stdio},
    sync::{Mutex, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};

// The only non-CLOEXEC descriptors are control sockets, during spawn only.
// No unsafe pre_exec hook and no raw-fd borrowing is needed.
static SPAWN: Mutex<()> = Mutex::new(());

struct Channel {
    socket: UnixStream,
    bytes: Vec<u8>,
    eof: bool,
}
impl Channel {
    fn new(socket: UnixStream) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            bytes: Vec::new(),
            eof: false,
        })
    }
    fn drain(&mut self) -> io::Result<()> {
        let mut buffer = [0u8; 4096];
        loop {
            match self.socket.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(());
                }
                Ok(n) if self.bytes.len() + n <= 65536 => {
                    self.bytes.extend_from_slice(&buffer[..n])
                }
                Ok(_) => return Err(io::Error::other("oversized bwrap control message")),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
    fn info(&self) -> Option<Value> {
        serde_json::from_slice(&self.bytes).ok()
    }
    fn exit_code(&self) -> Option<i32> {
        serde_json::Deserializer::from_slice(&self.bytes)
            .into_iter::<Value>()
            .filter_map(Result::ok)
            .find_map(|v| v["exit-code"].as_i64().and_then(|n| n.try_into().ok()))
    }
}

struct Supervisor {
    child: Child,
    init: Option<OwnedFd>,
    reaped: bool,
}
impl Supervisor {
    fn signal_init(&self, signal: Signal) {
        if let Some(fd) = &self.init {
            let _ = pidfd_send_signal(fd, signal);
        }
    }
    fn init_gone(&self) -> bool {
        self.init.as_ref().is_none_or(|fd| {
            fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd()))
                .is_ok_and(|text| text.lines().any(|line| line.trim() == "Pid:\t-1"))
        })
    }
    fn poll(&mut self) -> io::Result<()> {
        if !self.reaped {
            let pid = Pid::from_raw(self.child.id() as i32)
                .ok_or_else(|| io::Error::other("invalid child pid"))?;
            if waitpid(Some(pid), WaitOptions::NOHANG)?.is_some() {
                self.reaped = true;
            }
        }
        Ok(())
    }
    fn kill_launcher(&self) {
        if !self.reaped
            && let Some(pid) = Pid::from_raw(self.child.id() as i32)
        {
            let _ = kill_process(pid, Signal::KILL);
        }
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        self.signal_init(Signal::KILL);
        if !self.reaped {
            self.kill_launcher();
            if let Some(pid) = Pid::from_raw(self.child.id() as i32) {
                let _ = waitpid(Some(pid), WaitOptions::empty());
            }
        }
    }
}

/// std creates owned descriptors CLOEXEC. Fail closed if an embedding harness
/// supplied a non-CLOEXEC fd: safe Rust cannot mutate arbitrary borrowed raw
/// fds. This also covers descriptors opened by foreign libraries. Never guess.
fn check_descriptors() -> io::Result<()> {
    for entry in fs::read_dir("/proc/self/fdinfo")? {
        let entry = entry?;
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if fd <= 2 {
            continue;
        }
        let text = match fs::read_to_string(entry.path()) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let flags = text
            .lines()
            .find_map(|line| line.strip_prefix("flags:\t"))
            .and_then(|s| u32::from_str_radix(s.trim(), 8).ok());
        if flags.is_none_or(|flags| flags & 0o2000000 == 0) {
            return Err(io::Error::other(
                "inherited harness descriptor lacks close-on-exec",
            ));
        }
    }
    Ok(())
}

fn spawn(sandbox: &Sandbox, spec: &Spec) -> io::Result<(Supervisor, Channel, Channel, Capture)> {
    let _lock = SPAWN
        .lock()
        .map_err(|_| io::Error::other("spawn lock poisoned"))?;
    let (info_read, info_write) = UnixStream::pair()?;
    let (status_read, status_write) = UnixStream::pair()?;
    let info = Channel::new(info_read)?;
    let status = Channel::new(status_read)?;
    check_descriptors()?;
    let mut command = if let Some(runtime) = &sandbox.scope_runtime {
        let mut command = Command::new(&sandbox.config.systemd_run);
        command.env_clear();
        for key in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        if let Some(runtime) = runtime {
            command.env("XDG_RUNTIME_DIR", runtime);
        }
        command
            .args([
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--expand-environment=no",
                "-p",
            ])
            .arg(format!("TasksMax={}", sandbox.config.tasks_max))
            .arg("--")
            .arg(&sandbox.config.bwrap);
        command
    } else {
        let mut command = Command::new(&sandbox.config.bwrap);
        command.env_clear();
        command
    };
    command
        .args(spec.argv(info_write.as_raw_fd(), status_write.as_raw_fd()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // These fds belong to us. bwrap consumes and closes them before execing bash.
    fcntl_setfd(&info_write, FdFlags::empty())?;
    if let Err(error) = fcntl_setfd(&status_write, FdFlags::empty()) {
        let _ = fcntl_setfd(&info_write, FdFlags::CLOEXEC);
        return Err(error.into());
    }
    let result = command.spawn();
    // Restore in the parent even on spawn failure, then immediately close.
    let info_reset = fcntl_setfd(&info_write, FdFlags::CLOEXEC);
    let status_reset = fcntl_setfd(&status_write, FdFlags::CLOEXEC);
    drop(info_write);
    drop(status_write);
    let mut supervisor = Supervisor {
        child: result?,
        init: None,
        reaped: false,
    };
    info_reset?;
    status_reset?;
    let stdout = supervisor
        .child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing stdout pipe"))?;
    let stderr = supervisor
        .child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing stderr pipe"))?;
    let capture = Capture::start(stdout, stderr, sandbox.config.capture_limit);
    Ok((supervisor, info, status, capture))
}

fn acquire_init(info: &Channel) -> io::Result<Option<OwnedFd>> {
    let Some(facts) = info.info() else {
        return Ok(None);
    };
    let raw = facts["child-pid"]
        .as_i64()
        .and_then(|n| i32::try_from(n).ok())
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::other("missing namespace init pid"))?;
    match pidfd_open(raw, PidfdFlags::empty()) {
        Ok(fd) => {
            // Bind the pidfd to the reported namespace, not a potentially reused pid.
            use std::os::unix::fs::MetadataExt;
            match fs::metadata(format!("/proc/{}/ns/pid", raw.as_raw_nonzero())) {
                Ok(metadata) if Some(metadata.ino()) == facts["pid-namespace"].as_u64() => {
                    Ok(Some(fd))
                }
                Ok(_) => Err(io::Error::other("namespace init pid identity mismatch")),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }
        Err(rustix::io::Errno::SRCH) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn run(
    sandbox: &Sandbox,
    spec: Spec,
    cancel: &CancelToken,
    start: Instant,
) -> ExecutionOutcome {
    let (mut supervisor, mut info, mut status, capture) = match spawn(sandbox, &spec) {
        Ok(parts) => parts,
        Err(error) => {
            return unavailable(start, &error.to_string(), sandbox.config.model_text_limit);
        }
    };
    let mut stop: Option<(ToolStatus, ResultCode, &'static str)> = None;
    let mut stopped_at = None;
    let mut term_sent = false;
    let mut init_seen = false;
    let mut infrastructure = false;
    loop {
        if info.drain().is_err() || status.drain().is_err() || supervisor.poll().is_err() {
            infrastructure = true;
        }
        if !init_seen && info.info().is_some() {
            init_seen = true;
            match acquire_init(&info) {
                Ok(fd) => supervisor.init = fd,
                Err(_) => infrastructure = true,
            }
        }
        if stop.is_none() {
            stop = if infrastructure || capture.io_error.load(Ordering::Acquire) {
                Some((
                    ToolStatus::InfrastructureFailure,
                    ResultCode::SandboxUnavailable,
                    "sandbox control or capture failed",
                ))
            } else if capture.overflow.load(Ordering::Acquire) {
                Some((
                    ToolStatus::Failed,
                    ResultCode::OutputLimit,
                    "output_limit: aggregate capture limit exceeded",
                ))
            } else if cancel.is_cancelled() {
                Some((
                    ToolStatus::Cancelled,
                    ResultCode::Cancelled,
                    "cancelled: command cancelled",
                ))
            } else if start.elapsed() >= spec.timeout {
                Some((
                    ToolStatus::TimedOut,
                    ResultCode::Timeout,
                    "timeout: command deadline exceeded",
                ))
            } else {
                None
            };
        }
        if stop.is_some() {
            let since = *stopped_at.get_or_insert_with(Instant::now);
            if supervisor.init.is_some() && !term_sent {
                supervisor.signal_init(Signal::TERM);
                term_sent = true;
            }
            if since.elapsed() >= sandbox.config.kill_grace {
                supervisor.signal_init(Signal::KILL);
                // Also covers failed setup before a namespace init was reported.
                if !init_seen || supervisor.init_gone() {
                    supervisor.kill_launcher();
                }
            }
        }
        if supervisor.reaped {
            let _ = info.drain();
            let _ = status.drain();
            if !init_seen && info.info().is_some() {
                init_seen = true;
                match acquire_init(&info) {
                    Ok(fd) => supervisor.init = fd,
                    Err(_) => infrastructure = true,
                }
            }
            if supervisor.init_gone() {
                break;
            }
            // A reaped launcher is not sufficient evidence of init disappearance:
            // pidfd retirement can lag waitpid. Wait, and still enforce cleanup.
            let since = *stopped_at.get_or_insert_with(Instant::now);
            if since.elapsed() >= sandbox.config.kill_grace {
                supervisor.signal_init(Signal::KILL);
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    let (bytes, artifacts, overflow, io_error) = capture.finish();
    let exit_code = status.exit_code();
    let (outcome_status, code, detail) = if infrastructure
        || io_error
        || !init_seen
        || (exit_code.is_none() && stop.is_none())
    {
        (
            ToolStatus::InfrastructureFailure,
            Some(ResultCode::SandboxUnavailable),
            "sandbox_unavailable: setup or supervision failed".to_owned(),
        )
    } else if spec.absent.iter().any(
        |path| !matches!(fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound),
    ) {
        // Post-run protection applies even if the workload was interrupted.
        (
            ToolStatus::Failed,
            Some(ResultCode::ProtectedPath),
            "protected_path: command created an absent protected path; no rollback".to_owned(),
        )
    } else if let Some((status, code, detail)) = stop {
        (status, Some(code), detail.to_owned())
    } else if overflow {
        (
            ToolStatus::Failed,
            Some(ResultCode::OutputLimit),
            "output_limit: aggregate capture limit exceeded".to_owned(),
        )
    } else if exit_code == Some(0) {
        (ToolStatus::Succeeded, None, String::new())
    } else {
        let exit = exit_code.unwrap_or(-1);
        let signal = if (129..=192).contains(&exit) {
            format!(
                " (128 + signal {}; signal death or explicit shell exit)",
                exit - 128
            )
        } else {
            String::new()
        };
        (
            ToolStatus::Failed,
            Some(ResultCode::NonzeroExit),
            format!("nonzero_exit: exit status {exit}{signal}"),
        )
    };
    let mut combined = bytes;
    if !detail.is_empty() {
        combined.extend_from_slice(format!("\n{detail}\n").as_bytes());
    }
    let (content, elided) = content(&combined, sandbox.config.model_text_limit);
    ExecutionOutcome {
        status: outcome_status,
        code,
        content,
        exit_code,
        duration_ms: Some(start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)),
        output_truncated: overflow || elided,
        artifacts,
    }
}
