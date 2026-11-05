//! User-selected read-only toolchains. No project configuration is authority.
#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};

/// Additive sandbox policy, omitted entirely for the v0.1 defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolchainSettings {
    /// Absolute directories bound read-only at the same path.
    pub toolchain_paths: Vec<PathBuf>,
    /// Ordered executable search directories before /usr/bin:/bin.
    pub path_prepend: Vec<PathBuf>,
    /// Explicit, noncredential environment; never inherited from the host.
    pub env: BTreeMap<String, String>,
}

/// Same credential-name rule as the user TOML boundary.
pub fn credential_name(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "api_key"
        || key.ends_with("_key")
        || matches!(key.as_str(), "token" | "secret" | "password")
}

impl ToolchainSettings {
    /// Whether the unconfigured, byte-identical v0.1 behavior applies.
    pub fn is_empty(&self) -> bool {
        self.toolchain_paths.is_empty() && self.path_prepend.is_empty() && self.env.is_empty()
    }

    /// Pure structural checks, also used when decoding a recorded policy.
    /// Errors name keys/paths, never environment values.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::BTreeSet::new();
        for path in &self.toolchain_paths {
            absolute(path)?;
            if !seen.insert(path) {
                return Err(format!("duplicate toolchain path: {}", path.display()));
            }
        }
        for path in &self.path_prepend {
            absolute(path)?;
            if !path.starts_with("/usr")
                && !self
                    .toolchain_paths
                    .iter()
                    .any(|root| path.starts_with(root))
            {
                return Err(format!(
                    "path_prepend outside toolchain mounts or /usr: {}",
                    path.display()
                ));
            }
            if path.as_os_str().as_encoded_bytes().contains(&b':') {
                return Err(format!("path_prepend contains colon: {}", path.display()));
            }
        }
        for (key, value) in &self.env {
            let upper = key.to_ascii_uppercase();
            let owned = matches!(
                upper.as_str(),
                "PATH"
                    | "HOME"
                    | "TMPDIR"
                    | "LANG"
                    | "TERM"
                    | "BASH_ENV"
                    | "ENV"
                    | "SHELLOPTS"
                    | "BASHOPTS"
            ) || [
                "CARGO_",
                "LD_",
                "OPENAI_",
                "ANTHROPIC_",
                "DEEPSEEK_",
                "CODEX_",
            ]
            .iter()
            .any(|p| upper.starts_with(p));
            let valid = !key.is_empty()
                && key.bytes().enumerate().all(|(i, b)| {
                    b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
                });
            if credential_name(key) || owned || !valid || value.contains('\0') {
                return Err(format!(
                    "sandbox.env.{}: refused environment key or invalid string",
                    key.escape_default()
                ));
            }
        }
        Ok(())
    }

    /// Validate real locations at config load, without creating missing roots.
    /// Resolve aliases for exclusions, but refuse symlink mount roots/components.
    pub fn validate_locations(
        &self,
        workspace: &Path,
        data: &Path,
        home: &Path,
        config_dirs: &[PathBuf],
    ) -> Result<(), String> {
        self.validate()?;
        let workspace = location(workspace).ok_or("cannot resolve workspace")?;
        let data = location(data).ok_or("cannot resolve data root")?;
        let home = location(home).ok_or("cannot resolve home")?;
        let mut protected = vec![
            home.join(".ssh"),
            home.join(".gnupg"),
            home.join(".codex"),
            home.join(".aws"),
            home.join(".config"),
        ];
        protected.extend(config_dirs.iter().cloned());
        for root in &self.toolchain_paths {
            let refuse = || format!("refused toolchain path: {}", root.display());
            let actual = fs::canonicalize(root).map_err(|_| refuse())?;
            if !actual.is_dir() || actual != *root || symlink_component(root) {
                return Err(refuse());
            }
            if root == Path::new("/")
                || actual == home
                || home.starts_with(&actual)
                || overlaps(&actual, &workspace)
                || overlaps(&actual, &data)
                || actual == Path::new("/tmp")
                || [
                    "/workspace",
                    "/home/sandbox",
                    "/tmp/cargo-target",
                    "/proc",
                    "/dev",
                ]
                .iter()
                .any(|p| overlaps(&actual, Path::new(p)))
                || protected
                    .iter()
                    .any(|p| location(p).is_none_or(|p| overlaps(&actual, &p)))
            {
                return Err(refuse());
            }
        }
        for path in &self.path_prepend {
            let actual = fs::canonicalize(path)
                .map_err(|_| format!("unavailable path_prepend: {}", path.display()))?;
            if !actual.is_dir()
                || (!actual.starts_with("/usr")
                    && !self
                        .toolchain_paths
                        .iter()
                        .any(|root| actual.starts_with(root)))
            {
                return Err(format!(
                    "path_prepend resolves outside toolchain mounts or /usr: {}",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    /// The sandbox PATH, independent of the invoking process's environment.
    pub fn path(&self) -> String {
        self.path_prepend
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .chain(["/usr/bin".into(), "/bin".into()])
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Fixed credential leaves at each mount root. Ancestors of the user's home
    /// are forbidden, so ~/.cargo credentials can only be exposed by its root.
    pub fn credential_paths(&self, root: &Path) -> Vec<PathBuf> {
        vec![
            root.join(".git-credentials"),
            root.join("credentials"),
            root.join("credentials.toml"),
        ]
    }

    /// Read-only diagnostic facts. Environment values never enter this report.
    pub fn report(&self) -> Value {
        let mounts: Vec<_> = self.toolchain_paths.iter().map(|root| {
            let masks: Vec<_> = self.credential_paths(root).iter().map(|path| json!({"path":path, "status":match fs::symlink_metadata(path) {
                Ok(_) => "masked", Err(e) if e.kind() == std::io::ErrorKind::NotFound => "absent", Err(_) => "unreadable"
            }})).collect();
            json!({"path":root,"mode":"ro","exists":root.exists(),"readable":fs::read_dir(root).is_ok(),"credential_masks":masks})
        }).collect();
        json!({"toolchain_paths":mounts,"path_prepend":self.path_prepend,"env_keys":self.env.keys().collect::<Vec<_>>()})
    }
}
fn absolute(path: &Path) -> Result<(), String> {
    let text = path.to_str().unwrap_or("");
    if !path.is_absolute()
        || text.contains('\0')
        || text.ends_with('/') && text != "/"
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        || path.components().collect::<PathBuf>().as_os_str() != path.as_os_str()
    {
        return Err(format!(
            "expected normalized absolute directory: {}",
            path.display()
        ));
    }
    Ok(())
}
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
fn symlink_component(path: &Path) -> bool {
    let mut current = PathBuf::new();
    path.components().any(|part| {
        current.push(part);
        fs::symlink_metadata(&current).is_ok_and(|m| m.file_type().is_symlink())
    })
}
fn location(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return None;
    }
    match fs::canonicalize(path) {
        Ok(p) => Some(p),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound && fs::symlink_metadata(path).is_err() =>
        {
            Some(location(path.parent()?)?.join(path.file_name()?))
        }
        Err(_) => None,
    }
}
