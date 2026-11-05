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

/// Private, per-command cargo home created inside the sandbox.
pub const CARGO_HOME_DEST: &str = "/home/sandbox/.cargo";
/// Where a configured offline registry is bound, inside [`CARGO_HOME_DEST`].
pub const CARGO_REGISTRY_DEST: &str = "/home/sandbox/.cargo/registry";

/// Additive sandbox policy, omitted entirely for the v0.1 defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolchainSettings {
    /// Absolute directories bound read-only at the same path.
    pub toolchain_paths: Vec<PathBuf>,
    /// Ordered executable search directories before /usr/bin:/bin.
    pub path_prepend: Vec<PathBuf>,
    /// Optional read-only offline registry, bound at the private CARGO_HOME's
    /// `registry` so cargo finds it without a manual per-command link.
    pub cargo_registry: Option<PathBuf>,
    /// Explicit, noncredential environment; never inherited from the host.
    pub env: BTreeMap<String, String>,
}

/// Words that name a credential on their own or as the last `_`-separated
/// segment of a longer name: `TOKEN`/`GITHUB_TOKEN`, `DB_PASSWORD`,
/// `CLIENT_SECRET`, `AWS_SECRET_ACCESS_KEY`.
const CREDENTIAL_SEGMENTS: [&str; 7] = [
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "credentials",
    "key",
];

/// Same credential-name rule as the user TOML boundary: the whole name or its
/// last `_`-separated segment, case-insensitively.
pub fn credential_name(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let last = key.rsplit('_').next().unwrap_or(key.as_str());
    CREDENTIAL_SEGMENTS.contains(&last)
}

impl ToolchainSettings {
    /// Whether the unconfigured, byte-identical v0.1 behavior applies.
    pub fn is_empty(&self) -> bool {
        self.toolchain_paths.is_empty()
            && self.path_prepend.is_empty()
            && self.cargo_registry.is_none()
            && self.env.is_empty()
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
        if let Some(registry) = &self.cargo_registry {
            absolute(registry)?;
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
        for root in self.mount_roots() {
            refuse_location(root, &workspace, &data, &home, &protected)?;
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

    /// Every read-only host directory this sandbox binds: toolchain roots by
    /// depth, then the optional offline registry.
    pub fn mount_roots(&self) -> Vec<&PathBuf> {
        let mut roots: Vec<_> = self.toolchain_paths.iter().collect();
        roots.sort_by_key(|path| path.components().count());
        roots.extend(self.cargo_registry.iter());
        roots
    }

    /// `(source, sandbox destination)` pairs in install order. A toolchain keeps
    /// its own path; the registry lands in the private CARGO_HOME so cargo
    /// resolves it without any per-command link.
    pub fn mounts(&self) -> Vec<(&Path, &Path)> {
        let mut mounts: Vec<(&Path, &Path)> = self
            .toolchain_paths
            .iter()
            .map(|path| (path.as_path(), path.as_path()))
            .collect();
        mounts.sort_by_key(|(source, _)| source.components().count());
        if let Some(registry) = &self.cargo_registry {
            mounts.push((registry.as_path(), Path::new(CARGO_REGISTRY_DEST)));
        }
        mounts
    }

    /// The private cargo home created for every configured sandbox.
    pub fn cargo_home_dest() -> &'static str {
        CARGO_HOME_DEST
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
    /// are forbidden, so ~/.cargo credentials can only be exposed by its root,
    /// and a configured registry root is masked the same way.
    pub fn credential_paths(&self, root: &Path) -> Vec<PathBuf> {
        credential_leaves(root)
    }

    /// Read-only diagnostic facts, mirroring the masks `args.rs` installs.
    /// Environment values never enter this report.
    pub fn report(&self) -> Value {
        let mut value = json!({
            "toolchain_paths": self
                .toolchain_paths
                .iter()
                .map(|root| mount_report(root, root))
                .collect::<Vec<_>>(),
            "path_prepend": self.path_prepend,
            "env_keys": self.env.keys().collect::<Vec<_>>(),
        });
        if let Some(registry) = &self.cargo_registry {
            value["cargo_registry"] = mount_report(registry, Path::new(CARGO_REGISTRY_DEST));
        }
        value
    }
}

fn mount_report(root: &Path, dest: &Path) -> Value {
    let mut masks: Vec<Value> = Vec::new();
    let mut masked_dirs = Vec::<PathBuf>::new();
    for path in credential_leaves(root) {
        if masked_dirs
            .iter()
            .any(|ancestor| path.starts_with(ancestor))
        {
            continue;
        }
        let status = mask_status(&path);
        if status == "masked" && fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            masked_dirs.push(path.clone());
        }
        let exposed = dest.join(path.strip_prefix(root).unwrap_or(Path::new("")));
        masks.push(json!({"path": path, "sandbox_path": exposed, "status": status}));
    }
    json!({
        "path": root,
        "sandbox_path": dest,
        "mode": "ro",
        "exists": root.is_dir(),
        "readable": fs::read_dir(root).is_ok(),
        "credential_masks": masks,
    })
}

/// Exactly what `args.rs` does with one credential leaf.
fn mask_status(path: &Path) -> &'static str {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() || metadata.is_dir() => "masked",
        // D13: an absent leaf cannot be masked beneath a read-only bind, and any
        // other kind is refused rather than exposed.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "absent",
        _ => "refused",
    }
}

/// The fixed credential leaves, in a stable order.
fn credential_leaves(root: &Path) -> Vec<PathBuf> {
    vec![
        root.join(".git-credentials"),
        root.join("credentials"),
        root.join("credentials.toml"),
    ]
}

/// The toolchain-path refusal set, shared by every mounted host directory.
fn refuse_location(
    root: &Path,
    workspace: &Path,
    data: &Path,
    home: &Path,
    protected: &[PathBuf],
) -> Result<(), String> {
    let refuse = || format!("refused toolchain path: {}", root.display());
    let actual = fs::canonicalize(root).map_err(|_| refuse())?;
    if !actual.is_dir() || actual != *root || symlink_component(root) {
        return Err(refuse());
    }
    if root == Path::new("/")
        || actual == home
        || home.starts_with(&actual)
        || overlaps(&actual, workspace)
        || overlaps(&actual, data)
        || actual == Path::new("/tmp")
        || ["/workspace", "/home/sandbox", "/proc", "/dev"]
            .iter()
            .any(|p| overlaps(&actual, Path::new(p)))
        || protected
            .iter()
            .any(|p| location(p).is_none_or(|p| overlaps(&actual, &p)))
    {
        return Err(refuse());
    }
    Ok(())
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
