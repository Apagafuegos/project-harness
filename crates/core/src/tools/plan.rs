use serde::Serialize;
use std::sync::Arc;

use super::{
    PolicySnapshot, Refusal, ToolDefinition, WorkspacePath,
    builtin::{MIB, Operation},
};
use crate::session::{
    ArtifactRef, CallRef, DataRoot, Digest, EffectSet, ExecutionBackend, FileImage, ResultCode,
};

/// The session's recorded workspace string, not a path resolved by this layer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct WorkspaceIdentity(pub String);

/// Facts available without exposing a filesystem handle to the policy layer.
pub trait WorkspaceView {
    /// The session workspace identity.
    fn identity(&self) -> &WorkspaceIdentity;
    /// Inspect using trusted resolution; absence is a fact, not an error.
    fn inspect(&self, path: &WorkspacePath) -> Result<FileFacts, Refusal>;
    /// Inspect kind/protection without content hashing for bounded search.
    /// Views without a separate metadata implementation may reuse inspection.
    fn inspect_metadata(&self, path: &WorkspacePath) -> Result<FileFacts, Refusal> {
        self.inspect(path)
    }
    /// Return UTF-8 text or `not_utf8`/`too_large`; never truncate silently.
    fn read_text(&self, path: &WorkspacePath, max_bytes: u64) -> Result<String, Refusal>;
    /// Capture a trusted write/edit pre-image and publish its raw bytes. A
    /// view without a mutable filesystem refuses, which turns the call into a
    /// nonexecuted refusal: nothing is dispatched without an image.
    fn capture(&self, _plan: &Plan, _artifacts: &DataRoot) -> Result<FileImage, Refusal> {
        Err(Refusal::new(
            ResultCode::InfrastructureFailure,
            "workspace capture unavailable",
        ))
    }
}

/// Kind of a resolved workspace object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// Regular file.
    Regular,
    /// Directory.
    Directory,
    /// No object at this path.
    Absent,
    /// Unsupported object (a real view must reject symlinks during resolution).
    Other,
}

/// Trusted descriptor/ancestor protection. More restrictive values win.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Protection {
    /// Ordinary workspace object.
    None,
    /// Reads only.
    ReadOnly,
    /// No access.
    Denied,
}

/// Facts from a trusted view, including inherited protection for absent targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFacts {
    /// Resolved kind.
    pub kind: FileKind,
    /// Byte size.
    pub size: u64,
    /// Link count; all file tools reject regular files with more than one link.
    pub nlink: u64,
    /// Required for regular files; absent for non-files.
    pub digest: Option<Digest>,
    /// Effective protection of the object and every ancestor.
    pub protection: Protection,
}

/// Protection facts bound to a plan. Cannot be manufactured or substituted by JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanFacts {
    target: Protection,
    cwd: Protection,
}

#[derive(Clone, Debug, Serialize)]
struct Mount {
    path: WorkspacePath,
    mode: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
enum PreImage {
    Digest(Digest),
    Absent(&'static str),
}

#[derive(Clone, Debug, Serialize)]
struct Target {
    path: WorkspacePath,
    pre_image: PreImage,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_sha256: Option<Digest>,
}

// Field declaration order is the version 2 canonical wire order.
#[derive(Clone, Debug, Serialize)]
struct WirePlan {
    version: u32,
    call: CallRef,
    name: String,
    arguments: Operation,
    workspace: WorkspaceIdentity,
    cwd: WorkspacePath,
    effects: EffectSet,
    backend: ExecutionBackend,
    mounts: Vec<Mount>,
    network: &'static str,
    timeout_ms: u64,
    policy_revision: String,
    target: Option<Target>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox: Option<super::sandbox::ToolchainSettings>,
}

/// An exact immutable plan. No public construction or deserialization is possible.
#[derive(Clone, Debug)]
pub struct Plan {
    wire: Arc<WirePlan>,
    bytes: Arc<[u8]>,
    digest: Digest,
    facts: PlanFacts,
    configuration: ArtifactRef,
}

impl Plan {
    pub(super) fn build(
        definition: &ToolDefinition,
        operation: &Operation,
        call: &CallRef,
        workspace: &dyn WorkspaceView,
        policy: &PolicySnapshot,
    ) -> Result<Self, Refusal> {
        let root = WorkspacePath::new(".")?;
        let cwd = match operation {
            Operation::Bash { cwd, .. } => cwd.clone(),
            _ => root.clone(),
        };
        let cwd_facts = workspace.inspect(&cwd)?;
        let cwd_protection = policy.protection(&cwd, cwd_facts.protection);
        refuse_protection(cwd_protection, false)?;
        require_directory(&cwd_facts)?;
        let mut facts = PlanFacts {
            target: Protection::None,
            cwd: cwd_protection,
        };
        let mut mounts = Vec::new();
        let target = if operation.search() {
            let path = operation.path().expect("search path");
            if policy.protected_paths().iter().any(|p| path.beneath(p))
                || path.as_str().split('/').any(|p| p == ".git")
            {
                return Err(Refusal::new(
                    ResultCode::ProtectedPath,
                    "target or directory is protected",
                ));
            }
            let current = workspace.inspect_metadata(path)?;
            facts.target = policy.protection(path, current.protection);
            if facts.target != Protection::None {
                return Err(Refusal::new(
                    ResultCode::ProtectedPath,
                    "target or directory is protected",
                ));
            }
            if matches!(operation, Operation::Grep { .. }) && current.kind == FileKind::Regular {
                // Grep accepts either a file or a directory, without content reads in planning.
            } else {
                require_directory(&current)?;
            }
            None
        } else if let Some(path) = operation.path() {
            let current = workspace.inspect(path)?;
            facts.target = policy.protection(path, current.protection);
            refuse_protection(facts.target, operation.mutation())?;
            let absent = current.kind == FileKind::Absent;
            if absent && !matches!(operation, Operation::Write { .. }) {
                return Err(Refusal::new(ResultCode::NotFound, "target does not exist"));
            }
            if !absent {
                if current.kind != FileKind::Regular {
                    return Err(Refusal::new(
                        ResultCode::NotRegularFile,
                        "target is not a regular file",
                    ));
                }
                if current.nlink > 1 {
                    return Err(Refusal::new(
                        ResultCode::HardLinkRejected,
                        "multiply linked files are not supported",
                    ));
                }
            }
            let mut content_sha256 = None;
            match operation {
                Operation::Write {
                    text, overwrite, ..
                } => {
                    if !absent && !overwrite {
                        return Err(Refusal::new(
                            ResultCode::AlreadyExists,
                            "target exists and overwrite is false",
                        ));
                    }
                    let parent_path = path.parent();
                    let parent = workspace.inspect(&parent_path)?;
                    facts.target = facts
                        .target
                        .max(policy.protection(&parent_path, parent.protection));
                    refuse_protection(facts.target, true)?;
                    require_directory(&parent)?;
                    content_sha256 = Some(Digest::of(text.as_bytes()));
                }
                Operation::Read { .. } => {}
                Operation::Edit {
                    old_text, new_text, ..
                } => {
                    if current.size > MIB {
                        return Err(Refusal::new(ResultCode::TooLarge, "file exceeds 1 MiB"));
                    }
                    let text = workspace.read_text(path, MIB)?;
                    if text.len() as u64 > MIB {
                        return Err(Refusal::new(ResultCode::TooLarge, "file exceeds 1 MiB"));
                    }
                    match text
                        .as_bytes()
                        .windows(old_text.len())
                        .filter(|window| *window == old_text.as_bytes())
                        .take(2)
                        .count()
                    {
                        0 => {
                            return Err(Refusal::new(
                                ResultCode::NoMatch,
                                "old_text has no literal match",
                            ));
                        }
                        1 => {}
                        _ => {
                            return Err(Refusal::new(
                                ResultCode::MultipleMatches,
                                "old_text has multiple literal matches",
                            ));
                        }
                    }
                    if (text.len() - old_text.len())
                        .checked_add(new_text.len())
                        .is_none_or(|size| size as u64 > MIB)
                    {
                        return Err(Refusal::new(
                            ResultCode::TooLarge,
                            "edited contents exceed 1 MiB",
                        ));
                    }
                }
                Operation::Bash { .. }
                | Operation::List { .. }
                | Operation::Glob { .. }
                | Operation::Grep { .. } => unreachable!(),
            }
            let pre_image = if absent {
                PreImage::Absent("absent")
            } else {
                PreImage::Digest(current.digest.ok_or_else(|| {
                    Refusal::new(
                        ResultCode::InfrastructureFailure,
                        "workspace view omitted a regular file digest",
                    )
                })?)
            };
            Some(Target {
                path: path.clone(),
                pre_image,
                content_sha256,
            })
        } else {
            mounts.push(Mount {
                path: root,
                mode: if policy.mutation_denied() { "ro" } else { "rw" },
            });
            mounts.extend(policy.protected_paths().iter().map(|path| Mount {
                path: path.clone(),
                mode: "masked",
            }));
            let git = WorkspacePath::new(".git")?;
            if !policy
                .protected_paths()
                .iter()
                .any(|path| git.beneath(path))
                && workspace.inspect(&git)?.kind != FileKind::Absent
            {
                mounts.push(Mount {
                    path: git,
                    mode: "ro",
                });
            }
            None
        };
        let wire = WirePlan {
            version: 2,
            call: call.clone(),
            name: definition.name.clone(),
            arguments: operation.clone(),
            workspace: workspace.identity().clone(),
            cwd,
            effects: definition.effects.clone(),
            backend: definition.backend,
            mounts,
            network: "deny",
            timeout_ms: match operation {
                Operation::Bash { timeout_ms, .. } => *timeout_ms,
                _ => 30000,
            },
            policy_revision: policy.revision().into(),
            target,
            sandbox: (definition.backend == ExecutionBackend::Bubblewrap
                && !policy.sandbox().is_empty())
            .then(|| policy.sandbox().clone()),
        };
        let bytes =
            serde_json::to_vec(&wire).expect("plan contains only infallibly serializable values");
        Ok(Self {
            digest: Digest::of(&bytes),
            bytes: bytes.into(),
            wire: Arc::new(wire),
            facts,
            configuration: policy.artifact().clone(),
        })
    }

    #[cfg(test)]
    pub(super) fn set_test_protection(&mut self, target: Protection, cwd: Protection) {
        self.facts = PlanFacts { target, cwd };
    }

    /// Canonical compact JSON, with version 2's fixed field order.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// SHA-256 of exactly [`Self::bytes`].
    pub fn digest(&self) -> &Digest {
        &self.digest
    }
    /// The sole source for the execution intent's effects.
    pub fn effects(&self) -> &EffectSet {
        &self.wire.effects
    }
    /// The compiled-in backend.
    pub fn backend(&self) -> ExecutionBackend {
        self.wire.backend
    }
    /// Deadline recorded on the execution intent.
    pub fn timeout_ms(&self) -> u64 {
        self.wire.timeout_ms
    }
    /// The trusted facts to pass to [`super::decide`].
    pub fn facts(&self) -> &PlanFacts {
        &self.facts
    }
    /// The proposal identity bound into these bytes.
    pub fn call(&self) -> &CallRef {
        &self.wire.call
    }
    /// The bound policy revision.
    pub fn policy_revision(&self) -> &str {
        &self.wire.policy_revision
    }
    /// Reserved name of the planned tool.
    pub fn name(&self) -> &str {
        &self.wire.name
    }
    pub(super) fn operation(&self) -> &Operation {
        &self.wire.arguments
    }
    pub(super) fn bound_to(&self, policy: &PolicySnapshot, facts: &PlanFacts) -> bool {
        self.configuration == *policy.artifact()
            && self.policy_revision() == policy.revision()
            && self.facts == *facts
    }
}

pub(super) fn refuse_protection(protection: Protection, mutation: bool) -> Result<(), Refusal> {
    if protection == Protection::Denied || (mutation && protection == Protection::ReadOnly) {
        Err(Refusal::new(
            ResultCode::ProtectedPath,
            "target or directory is protected",
        ))
    } else {
        Ok(())
    }
}

impl PlanFacts {
    pub(super) fn check(&self, mutation: bool) -> Result<(), Refusal> {
        refuse_protection(self.cwd, false)?;
        refuse_protection(self.target, mutation)
    }
}

fn require_directory(facts: &FileFacts) -> Result<(), Refusal> {
    match facts.kind {
        FileKind::Directory => Ok(()),
        FileKind::Absent => Err(Refusal::new(
            ResultCode::NotFound,
            "required directory does not exist",
        )),
        _ => Err(Refusal::new(
            ResultCode::NotRegularFile,
            "required parent or cwd is not a directory",
        )),
    }
}
