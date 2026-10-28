//! Advisory current-file evidence. Never constructs a Plan or an execution result.
use serde::{Deserialize, Serialize};

use super::{FileKind, Protection, WorkspacePath, WorkspaceView};
use crate::session::Digest;

/// Ephemeral advice only; the user must still resolve the invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryHint {
    /// Current file has the authorized post-image.
    MatchesPostImage,
    /// Current file has the authorized pre-image (including absence).
    MatchesPreImage,
    /// Current file matches neither image.
    ChangedSince,
    /// A previously present target is absent.
    Missing,
    /// Evidence could not be obtained, with a machine-readable reason only.
    Unavailable {
        /// Refusal or infrastructure code, without file contents.
        reason: String,
    },
}
impl RecoveryHint {
    pub(crate) fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }
}

#[derive(Deserialize)]
struct Binding {
    name: String,
    workspace: String,
    target: Option<Target>,
    arguments: Arguments,
}
#[derive(Deserialize)]
struct Arguments {
    old_text: Option<String>,
    new_text: Option<String>,
}
#[derive(Deserialize)]
struct Target {
    path: String,
    pre_image: String,
    content_sha256: Option<Digest>,
}

/// Compare only the authorized wire binding with trusted workspace facts.
/// Refusals are advice, never failures of recovery or proof of execution.
pub fn recovery_hint(plan_bytes: &[u8], workspace: &dyn WorkspaceView) -> RecoveryHint {
    hint(plan_bytes, workspace).unwrap_or_else(RecoveryHint::unavailable)
}
fn hint(bytes: &[u8], workspace: &dyn WorkspaceView) -> Result<RecoveryHint, String> {
    use RecoveryHint::*;
    let binding: Binding = serde_json::from_slice(bytes).map_err(|_| "invalid_plan")?;
    if !matches!(binding.name.as_str(), "write" | "edit") {
        return Err("no_hint_for_tool".into());
    }
    if binding.workspace != workspace.identity().0 {
        return Err("workspace_mismatch".into());
    }
    let target = binding.target.ok_or("invalid_plan")?;
    let path = WorkspacePath::new(&target.path).map_err(|r| r.code.as_str().to_owned())?;
    let current = workspace
        .inspect(&path)
        .map_err(|r| r.code.as_str().to_owned())?;
    if current.protection != Protection::None {
        return Err("protected_path".into());
    }
    if !matches!(current.kind, FileKind::Regular | FileKind::Absent) {
        return Err("not_regular_file".into());
    }
    if current.kind == FileKind::Regular && current.nlink > 1 {
        return Err("hard_link_rejected".into());
    }
    let absent = current.kind == FileKind::Absent;
    if !absent && current.digest.is_none() {
        return Err("infrastructure_failure".into());
    }
    let pre: Option<Digest> = if target.pre_image == "absent" {
        None
    } else {
        Some(target.pre_image.parse().map_err(|_| "invalid_plan")?)
    };
    if binding.name == "write" {
        let post = target.content_sha256.ok_or("invalid_plan")?;
        if !absent && current.digest.as_ref() == Some(&post) {
            return Ok(MatchesPostImage);
        }
        if (absent && pre.is_none()) || (!absent && current.digest == pre) {
            return Ok(MatchesPreImage);
        }
        return Ok(if absent { Missing } else { ChangedSince });
    }
    let pre = pre.ok_or("invalid_plan")?;
    if !absent && current.digest.as_ref() == Some(&pre) {
        return Ok(MatchesPreImage);
    }
    if absent {
        return Ok(Missing);
    }
    let old = binding
        .arguments
        .old_text
        .as_deref()
        .ok_or("invalid_plan")?;
    let new = binding
        .arguments
        .new_text
        .as_deref()
        .ok_or("invalid_plan")?;
    let text = workspace
        .read_text(&path, 1024 * 1024)
        .map_err(|r| r.code.as_str().to_owned())?;
    if text.len() > 1024 * 1024 {
        return Err("too_large".into());
    }
    // Empty new_text has a match at every UTF-8 boundary. Only empty text
    // has exactly one; byte windows cannot represent that special case.
    let offsets: Vec<_> = if new.is_empty() {
        text.char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(text.len()))
            .take(2)
            .collect()
    } else {
        text.as_bytes()
            .windows(new.len())
            .enumerate()
            .filter_map(|(i, w)| (w == new.as_bytes()).then_some(i))
            .take(2)
            .collect()
    };
    if let [offset] = offsets.as_slice() {
        let mut reverse = text.clone();
        reverse.replace_range(*offset..*offset + new.len(), old);
        if Digest::of(reverse.as_bytes()) == pre {
            return Ok(MatchesPostImage);
        }
    }
    Ok(ChangedSince)
}

#[cfg(test)]
mod tests;
