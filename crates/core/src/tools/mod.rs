//! Pure built-in tool validation, planning, and configuration-bound policy.
//!
//! No executor is registered here. Workspace facts are supplied by a trusted
//! [`WorkspaceView`]; descriptor resolution and enforcement belong to step 5.

mod builtin;
mod exec;
pub mod fs;
mod path;
mod plan;
mod policy;
mod recovery;
pub mod sandbox;

pub use builtin::{Registry, Tool, ToolDefinition, ValidatedOperation};
pub use exec::{AuthorizedOperation, CancelToken, ExecutionOutcome, Executor, ToolSet};
pub use fs::{FsProtectedPaths, FsUnavailable, FsWorkspace};
pub use path::WorkspacePath;
pub use plan::{
    FileFacts, FileKind, Plan, PlanFacts, Protection, WorkspaceIdentity, WorkspaceView,
};
pub use policy::{Allowance, ApprovalRequest, Authorization, PolicyError, PolicySnapshot, decide};
pub use recovery::{RecoveryHint, recovery_hint};
pub use sandbox::{Sandbox, SandboxConfig, SandboxUnavailable};

use crate::session::{ResultCode, ToolStatus};

/// A bounded, model-facing refusal. Details never include workspace contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Stable machine-readable reason.
    pub code: ResultCode,
    /// Invalid proposal, policy denial, or expected workspace failure.
    pub status: ToolStatus,
    /// Short diagnostic without file contents.
    pub detail: String,
}

impl Refusal {
    pub(super) fn new(code: ResultCode, detail: &str) -> Self {
        use ResultCode::*;
        let status = match code {
            PolicyDenied | ProtectedPath | NetworkUnsupported => ToolStatus::Denied,
            InfrastructureFailure => ToolStatus::InfrastructureFailure,
            _ => ToolStatus::InvalidArguments,
        };
        Self {
            code,
            status,
            detail: detail.into(),
        }
    }

    /// Render the runtime's code-prefix convention.
    pub fn text(&self) -> String {
        format!("{}: {}", self.code.as_str(), self.detail)
    }
}

#[cfg(test)]
pub(crate) mod tests;
