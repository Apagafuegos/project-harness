//! The hidden completion check, executed inside the existing Bubblewrap sandbox.
//!
//! The check script is never part of the model's workspace while the model runs.
//! Immediately before the check, the runner copies its bytes into the workspace
//! as [`.harness-eval-check.sh`](CHECK_FILE), runs it through the product's
//! `Sandbox` backend (`--unshare-all`, network denied, workspace bound read-write
//! at `/workspace`) and then removes the file. There is no host fallback: when
//! the sandbox is unavailable the check fails with `sandbox_unavailable` rather
//! than running unsandboxed.

use std::{fs, path::Path, time::Instant};

use harness_core::{
    session::{ArtifactRef, CallId, ContentBlock, Digest, EntryId, ToolStatus},
    tools::{
        CancelToken, FsProtectedPaths, FsWorkspace, PolicySnapshot, Registry, Sandbox,
        SandboxConfig,
    },
};
use serde_json::json;

/// Name the check script temporarily takes inside the evaluated workspace.
pub const CHECK_FILE: &str = ".harness-eval-check.sh";
/// The single command the check runs. `sh` is `/bin/sh` inside the sandbox.
pub const CHECK_COMMAND: &str = "sh .harness-eval-check.sh";
/// Deadline for one hidden check, in milliseconds.
const CHECK_TIMEOUT_MS: u64 = 120_000;

/// What one hidden check reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckOutcome {
    /// Whether the task is complete: the check exited 0 inside the sandbox.
    pub passed: bool,
    /// Executor status spelling, such as `succeeded` or `failed`.
    pub status: String,
    /// Shell exit status, when the sandbox produced one.
    pub exit_code: Option<i32>,
    /// Stable executor error code, when the run failed at the sandbox level.
    pub code: Option<String>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Bounded capture, kept for diagnostics and never sent to a model.
    pub output: String,
}

/// A probed sandbox plus the trusted plan policy the check is planned under.
pub struct HiddenCheck {
    sandbox: Sandbox,
    policy: PolicySnapshot,
    registry: Registry,
}

impl HiddenCheck {
    /// Probe the product sandbox once per run. A missing or unusable Bubblewrap
    /// is an error here, never a later host fallback.
    pub fn probe(workspace_for_probe: &Path) -> Result<Self, String> {
        let config = SandboxConfig {
            workspace_root: workspace_for_probe.to_path_buf(),
            ..SandboxConfig::default()
        };
        let sandbox = Sandbox::probe(config).map_err(|error| error.to_string())?;
        let bytes = serde_json::to_vec(&json!({
            "policy_revision": "eval-check-v1",
            "enabled_tools": ["bash"],
            "workspace_read": "allow",
            "workspace_mutation": "ask",
            "process": "ask",
            "network": "deny",
            "protected_paths": [],
            "allowances": [],
        }))
        .map_err(|error| error.to_string())?;
        let policy = PolicySnapshot::parse(
            ArtifactRef {
                sha256: Digest::of(&bytes),
                bytes: bytes.len() as u64,
            },
            &bytes,
        )
        .map_err(|error| error.code().to_owned())?;
        Ok(Self {
            sandbox,
            policy,
            registry: Registry::builtin(),
        })
    }

    /// Run `command` in the sandbox against `workspace`. Used by the check and by
    /// the gate's own environment assertions.
    pub fn run_command(&self, workspace: &Path, command: &str) -> CheckOutcome {
        let started = Instant::now();
        let refuse = |code: &str, status: ToolStatus, detail: String| CheckOutcome {
            passed: false,
            status: status_name(status).into(),
            exit_code: None,
            code: Some(code.into()),
            duration_ms: millis(started),
            output: detail,
        };
        let call = harness_core::session::CallRef {
            response_entry_id: EntryId::new(),
            call_id: CallId::new(),
        };
        let view = match FsWorkspace::open(workspace, &FsProtectedPaths::default()) {
            Ok(view) => view,
            Err(error) => {
                return refuse(
                    "infrastructure_failure",
                    ToolStatus::InfrastructureFailure,
                    error.reason,
                );
            }
        };
        let plan = self
            .registry
            .resolve("bash", &self.policy)
            .and_then(|tool| {
                tool.validate(&json!({"command": command, "timeout_ms": CHECK_TIMEOUT_MS}))
            })
            .and_then(|operation| {
                self.registry.resolve("bash", &self.policy)?.plan(
                    &operation,
                    &call,
                    &view,
                    &self.policy,
                )
            });
        let plan = match plan {
            Ok(plan) => plan,
            Err(refusal) => {
                return refuse(
                    refusal.code.as_str(),
                    ToolStatus::InvalidArguments,
                    refusal.text(),
                );
            }
        };
        let outcome = self.sandbox.run(&plan, workspace, &CancelToken::new());
        let output = outcome
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        CheckOutcome {
            passed: outcome.status == ToolStatus::Succeeded,
            status: status_name(outcome.status).into(),
            exit_code: outcome.exit_code,
            code: outcome.code.map(|code| code.as_str().to_owned()),
            duration_ms: millis(started),
            output,
        }
    }

    /// Copy the hidden check into the workspace, run it, and remove it again.
    pub fn run_check(&self, task_check: &Path, workspace: &Path) -> CheckOutcome {
        let script = match fs::read(task_check) {
            Ok(bytes) => bytes,
            Err(error) => {
                return CheckOutcome {
                    passed: false,
                    status: "infrastructure_failure".into(),
                    exit_code: None,
                    code: Some("check_unreadable".into()),
                    duration_ms: 0,
                    output: error.to_string(),
                };
            }
        };
        let published = workspace.join(CHECK_FILE);
        if let Err(error) = fs::write(&published, &script) {
            return CheckOutcome {
                passed: false,
                status: "infrastructure_failure".into(),
                exit_code: None,
                code: Some("check_unwritable".into()),
                duration_ms: 0,
                output: error.to_string(),
            };
        }
        let outcome = self.run_command(workspace, CHECK_COMMAND);
        let _ = fs::remove_file(&published);
        outcome
    }
}

fn millis(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

fn status_name(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Succeeded => "succeeded",
        ToolStatus::Failed => "failed",
        ToolStatus::InvalidArguments => "invalid_arguments",
        ToolStatus::Denied => "denied",
        ToolStatus::TimedOut => "timed_out",
        ToolStatus::Cancelled => "cancelled",
        ToolStatus::InfrastructureFailure => "infrastructure_failure",
        ToolStatus::Uncertain => "uncertain",
    }
}
