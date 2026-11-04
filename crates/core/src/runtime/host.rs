//! Startup assembly of the real workspace backends. Never a host-shell fallback.
use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};

use super::ToolHost;
use crate::tools::{
    FsProtectedPaths, FsWorkspace, PolicySnapshot, Sandbox, SandboxConfig, WorkspacePath,
};

/// Trusted locations and optional sandbox settings for one session.
#[derive(Clone, Debug)]
pub struct ToolHostConfig {
    /// Absolute authorized workspace root; also overrides the sandbox root.
    pub workspace_root: PathBuf,
    /// Absolute harness data root (must match the session's storage root).
    pub data_root: PathBuf,
    /// None explicitly disables shell execution.
    pub sandbox: Option<SandboxConfig>,
}

/// Stable JSON backend state. Reasons contain startup diagnostics, never captures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum BackendAvailability {
    /// The backend was opened/probed successfully.
    Available,
    /// No executor was registered; all startup reasons are retained.
    Unavailable {
        /// Content-free startup diagnostics, including stable assembly codes.
        reasons: Vec<String>,
    },
}
impl BackendAvailability {
    fn from_reasons(reasons: Vec<String>) -> Self {
        if reasons.is_empty() {
            Self::Available
        } else {
            Self::Unavailable { reasons }
        }
    }
    /// Whether this backend is installed, not merely configured.
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Availability of the trusted file tools and sandboxed shell, independently reported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolAvailability {
    /// read/write/edit share the same descriptor-rooted backend.
    pub file_tools: BackendAvailability,
    /// bash requires both a successful probe and the file planning view (D31).
    pub shell: BackendAvailability,
}
impl ToolAvailability {
    pub(super) fn unavailable(reason: &str) -> Self {
        let backend = BackendAvailability::Unavailable {
            reasons: vec![reason.into()],
        };
        Self {
            file_tools: backend.clone(),
            shell: backend,
        }
    }
}

impl ToolHost {
    /// Open the workspace with exactly the policy's exclusions, and probe the
    /// optional sandbox. A failed backend is never replaced by weaker execution.
    pub fn assemble(config: &ToolHostConfig, policy: &PolicySnapshot) -> (Self, ToolAvailability) {
        if !config.workspace_root.is_absolute() || !config.data_root.is_absolute() {
            return (
                Self::none(),
                ToolAvailability::unavailable("absolute_path_required"),
            );
        }
        // Compare physical roots when they exist, so aliases of the data root
        // cannot evade masking. Resolve existing ancestors of absent roots in
        // read-only doctor reports, without creating or guessing anything.
        let (Some(workspace_root), Some(data_root)) = (
            location(&config.workspace_root),
            location(&config.data_root),
        ) else {
            return (
                Self::none(),
                ToolAvailability::unavailable("root_resolution_failed"),
            );
        };
        let inside = data_root.strip_prefix(&workspace_root).ok();
        let harness_data_root = match inside {
            Some(relative) => {
                let relative = if relative.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    relative
                };
                let Some(relative) = relative.to_str().and_then(|p| WorkspacePath::new(p).ok())
                else {
                    return (
                        Self::none(),
                        ToolAvailability::unavailable("data_root_unprotected"),
                    );
                };
                if !policy.protected_paths().iter().any(|p| {
                    p.as_str() == "." || Path::new(relative.as_str()).starts_with(p.as_str())
                }) {
                    return (
                        Self::none(),
                        ToolAvailability::unavailable("data_root_unprotected"),
                    );
                }
                Some(relative)
            }
            None => None,
        };
        let protected = FsProtectedPaths {
            configured: policy.protected_paths().to_vec(),
            harness_data_root,
        };
        let files = FsWorkspace::open(&config.workspace_root, &protected);
        let file_reasons = files
            .as_ref()
            .err()
            .map(|e| vec![e.reason.clone()])
            .unwrap_or_default();
        // Even a failed file layer must not hide the sandbox's own probe result.
        let sandbox = config.sandbox.clone().map(|mut sandbox| {
            sandbox.workspace_root = config.workspace_root.clone();
            Sandbox::probe(sandbox)
        });
        let mut shell_reasons = match &sandbox {
            None => vec!["shell_disabled".into()],
            Some(Err(error)) => {
                // Probe failures can embed arbitrary executable stdout/stderr
                // (including a configured environment). Publish categories only.
                let mut reasons: Vec<String> = error
                    .reasons
                    .iter()
                    .map(|reason| sandbox_reason(reason).into())
                    .collect();
                if reasons.is_empty() {
                    reasons.push("sandbox_probe_failed".into());
                }
                reasons
            }
            Some(Ok(_)) => Vec::new(),
        };
        if files.is_err() {
            shell_reasons.push("planning_view_unavailable".into());
            shell_reasons.extend(file_reasons.clone());
        }
        let availability = ToolAvailability {
            file_tools: BackendAvailability::from_reasons(file_reasons),
            shell: BackendAvailability::from_reasons(shell_reasons),
        };
        let mut host = Self::none();
        if let Ok(files) = files {
            let files = Arc::new(files);
            host.workspace = files.clone();
            host.files = Some(files.clone());
            host.executors.push(files);
            if let Some(Ok(sandbox)) = sandbox {
                host.executors.push(Arc::new(sandbox));
            }
        }
        (host, availability)
    }
}

fn sandbox_reason(reason: &str) -> &'static str {
    if reason.contains("descriptor audit") {
        "descriptor_audit_failed"
    } else if reason.starts_with("unrecognised version output") {
        "bwrap_version_unrecognized"
    } else if reason.starts_with("bubblewrap ") && reason.contains("older than the required") {
        "bwrap_version_unsupported"
    } else if reason.starts_with("configured sandbox probe failed") {
        "configured_sandbox_probe_failed"
    } else if reason.starts_with("environment leaked:") {
        "sandbox_environment_probe_failed"
    } else if reason.contains("namespace")
        || reason.starts_with("uid_map")
        || reason.starts_with("gid_map")
    {
        "sandbox_namespace_probe_failed"
    } else {
        "sandbox_probe_failed"
    }
}

// Canonicalize the longest existing ancestor, without lexically collapsing
// `..` across symlinks. Ambiguous roots and unresolved symlink ancestors are
// refused, not guessed; doctor must not create directories to resolve them.
pub(super) fn location(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return None;
    }
    if let Ok(path) = std::fs::canonicalize(path) {
        return Some(path);
    }
    match std::fs::symlink_metadata(path) {
        // A dangling/looping symlink or an existing but unresolvable object
        // must not be mistaken for a harmless not-yet-created data directory.
        Ok(_) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = location(path.parent()?)?;
            Some(parent.join(path.file_name()?))
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests;
