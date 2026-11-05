//! Fail-closed Bubblewrap process backend. This does not enable/register bash.
//!
//! Plans are trusted immutable proposals, not authorization tickets: the driver
//! must authorize the exact plan before calling [`Sandbox::run`].

mod args;
mod output;
mod probe;
mod process;
mod settings;
pub use settings::{ToolchainSettings, credential_name};

use super::{AuthorizedOperation, CancelToken, ExecutionOutcome, Executor, Plan};
use crate::session::{ContentBlock, ExecutionBackend, ResultCode, ToolStatus};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub use probe::{DEFAULT_BWRAP, DEFAULT_SYSTEMD_RUN, Programs, report as host_report};
/// Trusted session configuration, never shell arguments supplied by the model.
#[derive(Clone, Debug)]
pub struct SandboxConfig {
    /// Authorized workspace root used by the Executor adapter (defaults to cwd).
    pub workspace_root: PathBuf,
    /// Absolute Bubblewrap executable. Never searched on PATH.
    pub bwrap: PathBuf,
    /// Absolute shell inside the sandbox.
    pub bash: PathBuf,
    /// Read-only host runtime paths; symlinks are recreated, not followed.
    pub system_paths: Vec<PathBuf>,
    /// Sandbox executable search path (not the host's PATH).
    pub path: String,
    /// Opt-in user toolchains, recorded in the session policy and bash plan.
    pub toolchains: ToolchainSettings,
    /// Absolute systemd-run executable used when its user manager is available.
    pub systemd_run: PathBuf,
    /// Maximum aggregate stdout and stderr bytes retained.
    pub capture_limit: usize,
    /// Maximum compact-JSON content bytes, including escaping and truncation.
    pub model_text_limit: usize,
    /// Grace between SIGTERM and SIGKILL of the PID namespace init.
    pub kill_grace: Duration,
    /// Maximum tasks in an available systemd user scope.
    pub tasks_max: u32,
}
impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            workspace_root: std::env::current_dir().unwrap_or_default(),
            bwrap: DEFAULT_BWRAP.into(),
            bash: "/bin/bash".into(),
            system_paths: [
                "/usr",
                "/bin",
                "/lib",
                "/lib64",
                "/sbin",
                "/etc/alternatives",
                "/etc/ld.so.cache",
                "/etc/ssl/certs",
            ]
            .into_iter()
            .map(PathBuf::from)
            .filter(|path| std::fs::symlink_metadata(path).is_ok())
            .collect(),
            path: "/usr/bin:/bin".into(),
            toolchains: ToolchainSettings::default(),
            systemd_run: DEFAULT_SYSTEMD_RUN.into(),
            capture_limit: 64 * 1024 * 1024,
            model_text_limit: 32 * 1024,
            kill_grace: Duration::from_secs(2),
            tasks_max: 64,
        }
    }
}

/// Failure to establish the required isolation; never permits a host fallback.
#[derive(Clone, Debug, thiserror::Error)]
#[error("sandbox unavailable: {reasons:?}")]
pub struct SandboxUnavailable {
    /// All probe or setup reasons, without command text or captured secrets.
    pub reasons: Vec<String>,
}
impl SandboxUnavailable {
    pub(super) fn one(reason: impl Into<String>) -> Self {
        Self {
            reasons: vec![reason.into()],
        }
    }
}

pub(super) fn unavailable(start: Instant, reason: &str, cap: usize) -> ExecutionOutcome {
    let (content, output_truncated) =
        output::content(format!("sandbox_unavailable: {reason}").as_bytes(), cap);
    ExecutionOutcome {
        status: ToolStatus::InfrastructureFailure,
        code: Some(ResultCode::SandboxUnavailable),
        content,
        exit_code: None,
        duration_ms: Some(start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)),
        output_truncated,
        artifacts: Vec::new(),
    }
}

/// A successfully probed process backend; construction cannot bypass the probe.
#[derive(Debug)]
pub struct Sandbox {
    config: SandboxConfig,
    report: Value,
    scope_runtime: Option<Option<PathBuf>>,
    gap: Option<String>,
}
impl Sandbox {
    /// Probe the configured absolute executable and required host controls.
    /// Shares the doctor's version, in-sandbox and host-side id-map checks.
    pub fn probe(config: SandboxConfig) -> Result<Self, SandboxUnavailable> {
        args::validate_config(&config)?;
        let report = host_report(&Programs {
            bwrap: config.bwrap.clone(),
            systemd_run: config.systemd_run.clone(),
        })
        .map_err(SandboxUnavailable::one)?;
        if report["bubblewrap"]["status"] != "available" {
            return Err(SandboxUnavailable {
                reasons: report["bubblewrap"]["failures"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            });
        }
        let available = report["cgroup"]["status"] == "available";
        if available
            && (report["cgroup"]["probe"]["bwrap_in_scope"]["exit_code"] != 0
                || report["cgroup"]["probe"]["bwrap_in_scope"]["timed_out"] != false)
        {
            return Err(SandboxUnavailable::one(
                "reachable TasksMax manager could not launch the sandbox",
            ));
        }
        let scope_runtime = available.then(|| {
            report["cgroup"]["bus_runtime_dir"]
                .as_str()
                .map(PathBuf::from)
        });
        let gap = (!available).then(|| {
            format!(
                "TasksMax unavailable: {}",
                report["cgroup"]["reason"]
                    .as_str()
                    .unwrap_or("bwrap in user scope failed")
            )
        });
        let sandbox = Self {
            config,
            report,
            scope_runtime,
            gap,
        };
        // Also exercise the actual configured runtime mounts, shell, control-fd
        // protocol and optional scope. Doctor's report remains byte-compatible.
        let scratch =
            probe::Scratch::create().map_err(|e| SandboxUnavailable::one(e.to_string()))?;
        let spec = args::probe_spec(&sandbox.config, scratch.path())?;
        let outcome = process::run(&sandbox, spec, &CancelToken::new(), Instant::now());
        if outcome.status != ToolStatus::Succeeded {
            let detail: String = outcome
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            return Err(SandboxUnavailable::one(format!(
                "configured sandbox probe failed: status={:?}, code={:?}: {detail}",
                outcome.status, outcome.code
            )));
        }
        Ok(sandbox)
    }
    /// The unchanged doctor report, including version and cgroup evidence.
    pub fn probe_report(&self) -> &Value {
        &self.report
    }
    /// Explicitly recorded when no usable systemd user manager exists.
    pub fn process_limit_gap(&self) -> Option<&str> {
        self.gap.as_deref()
    }
    /// Run only this plan's shell, mounts, cwd and deadline. Never retries on host.
    /// Returns only after the PID namespace is gone and bwrap has been reaped.
    pub fn run(
        &self,
        plan: &Plan,
        workspace_root: &Path,
        cancel: &CancelToken,
    ) -> ExecutionOutcome {
        let start = Instant::now();
        if plan.name() != "bash" || plan.backend() != ExecutionBackend::Bubblewrap {
            let mut outcome = unavailable(
                start,
                "non-bash plan passed to the process backend",
                self.config.model_text_limit,
            );
            outcome.code = Some(ResultCode::InfrastructureFailure);
            let (content, truncated) = output::content(
                b"infrastructure_failure: non-bash plan passed to the process backend",
                self.config.model_text_limit,
            );
            outcome.content = content;
            outcome.output_truncated = truncated;
            return outcome;
        }
        if cancel.is_cancelled() {
            return ExecutionOutcome {
                status: ToolStatus::Cancelled,
                code: Some(ResultCode::Cancelled),
                content: vec![ContentBlock::Text {
                    text: "cancelled: cancelled before dispatch".into(),
                }],
                exit_code: None,
                duration_ms: Some(0),
                output_truncated: false,
                artifacts: Vec::new(),
            };
        }
        let spec = match args::build(&self.config, plan, workspace_root) {
            Ok(spec) => spec,
            Err(error) => {
                return unavailable(start, &error.to_string(), self.config.model_text_limit);
            }
        };
        process::run(self, spec, cancel, start)
    }
}

impl Executor for Sandbox {
    fn backend(&self) -> ExecutionBackend {
        ExecutionBackend::Bubblewrap
    }
    fn execute(&self, operation: &AuthorizedOperation, cancel: &CancelToken) -> ExecutionOutcome {
        self.run(operation.plan(), &self.config.workspace_root, cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        session::{ArtifactRef, CallRef, Digest},
        tools::{PolicySnapshot, Registry, tests::FakeWorkspace},
    };

    #[test]
    fn shared_executor_rejects_non_bash_and_honours_shared_cancellation() {
        let bytes = include_bytes!(
            "../../../../../docs/rfcs/001/examples/artifacts/7e66535a506e1c533025006e25e2fced162ef19d026d0db999154218ae7a30ea"
        );
        let policy = PolicySnapshot::parse(
            ArtifactRef {
                sha256: Digest::of(bytes),
                bytes: bytes.len() as u64,
            },
            bytes,
        )
        .unwrap();
        let registry = Registry::builtin();
        let view = FakeWorkspace::new();
        let sandbox = Sandbox {
            config: SandboxConfig::default(),
            report: Value::Null,
            scope_runtime: None,
            gap: Some("unit test".into()),
        };
        let call = CallRef {
            response_entry_id: "019a0000-0000-7000-8000-00000000019a".parse().unwrap(),
            call_id: "019a0000-0000-7000-8000-00000000019b".parse().unwrap(),
        };
        let executor: &dyn Executor = &sandbox;
        assert_eq!(executor.backend(), ExecutionBackend::Bubblewrap);
        for (name, arguments) in [
            ("read", serde_json::json!({"path":"a"})),
            ("bash", serde_json::json!({"command":"must-not-run"})),
        ] {
            let tool = registry.resolve(name, &policy).unwrap();
            let plan = tool
                .plan(&tool.validate(&arguments).unwrap(), &call, &view, &policy)
                .unwrap();
            let operation = AuthorizedOperation::new(
                plan,
                "019a0000-0000-7000-8000-00000000019c".parse().unwrap(),
                "019a0000-0000-7000-8000-00000000019d".parse().unwrap(),
            );
            let cancel = CancelToken::new();
            cancel.clone().cancel();
            let outcome = executor.execute(&operation, &cancel);
            if name == "read" {
                assert_eq!(outcome.status, ToolStatus::InfrastructureFailure);
                assert_eq!(outcome.code, Some(ResultCode::InfrastructureFailure));
            } else {
                assert_eq!(outcome.status, ToolStatus::Cancelled);
                assert_eq!(outcome.code, Some(ResultCode::Cancelled));
                let mismatch = executor.execute(&operation, &CancelToken::new());
                assert_eq!(mismatch.status, ToolStatus::InfrastructureFailure);
                assert_eq!(mismatch.code, Some(ResultCode::SandboxUnavailable));
                assert!(
                    matches!(&mismatch.content[0], ContentBlock::Text { text } if text.contains("plan workspace identity mismatch"))
                );
            }
        }
    }
}
