use serde::Serialize;

use super::Refusal;
use crate::session::ResultCode;

/// A normalized Linux workspace-relative path, including `.` for the root.
/// Construction always rejects traversal, absolute paths, NULs, and overlong input.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct WorkspacePath(String);

impl WorkspacePath {
    /// Validate at most 4096 UTF-8 bytes, then normalize separators and `.`.
    pub fn new(input: &str) -> Result<Self, Refusal> {
        if input.is_empty()
            || input.len() > 4096
            || input.starts_with('/')
            || input.contains('\0')
            || input.split('/').any(|part| part == "..")
        {
            return Err(Refusal::new(
                ResultCode::InvalidArguments,
                "expected a workspace-relative path without traversal (at most 4096 bytes)",
            ));
        }
        let parts: Vec<_> = input
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        Ok(Self(if parts.is_empty() {
            ".".into()
        } else {
            parts.join("/")
        }))
    }

    /// Canonical relative spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn beneath(&self, prefix: &Self) -> bool {
        prefix.0 == "."
            || self == prefix
            || self
                .0
                .strip_prefix(&prefix.0)
                .is_some_and(|s| s.starts_with('/'))
    }

    pub(super) fn parent(&self) -> Self {
        // Every prefix of a validated path is itself valid.
        Self(
            self.0
                .rsplit_once('/')
                .map_or(".", |(parent, _)| parent)
                .into(),
        )
    }
}
