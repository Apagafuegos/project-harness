use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Plan, PlanFacts, Protection, Refusal, WorkspacePath, builtin::Operation};
use crate::session::{ArtifactRef, CallRef, Digest, ResultCode, codec};

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReadPolicy {
    Allow,
    Ask,
    Deny,
}
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RestrictedPolicy {
    Ask,
    Deny,
}

/// Session configuration failures, never model-facing tool refusals.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// The bytes do not match the configuration artifact reference.
    #[error("policy_mismatch")]
    Mismatch,
    /// Invalid JSON, duplicate keys, or a non-object root.
    #[error("policy_not_an_object")]
    NotAnObject,
    /// A required policy field is absent.
    #[error("policy_missing_field: {0}")]
    MissingField(&'static str),
    /// A field has an invalid type or value.
    #[error("policy_invalid_field: {0}")]
    InvalidField(&'static str),
    /// Only deny-network configuration is supported.
    #[error("network_unsupported")]
    NetworkUnsupported,
    /// An allowance is malformed; index is zero-based in the configuration.
    #[error("policy_invalid_allowance: index {index}")]
    InvalidAllowance {
        /// Index in the allowances array.
        index: usize,
    },
}

impl PolicyError {
    /// Stable session-level diagnostic code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Mismatch => "policy_mismatch",
            Self::NotAnObject => "policy_not_an_object",
            Self::MissingField(_) => "policy_missing_field",
            Self::InvalidField(_) => "policy_invalid_field",
            Self::NetworkUnsupported => "network_unsupported",
            Self::InvalidAllowance { .. } => "policy_invalid_allowance",
        }
    }
}

/// Validated session policy plus the untouched configuration values.
#[derive(Clone, Debug)]
pub struct PolicySnapshot {
    artifact: ArtifactRef,
    object: Map<String, Value>,
    revision: String,
    enabled_tools: Vec<String>,
    workspace_read: ReadPolicy,
    workspace_mutation: RestrictedPolicy,
    process: RestrictedPolicy,
    protected_paths: Vec<WorkspacePath>,
    allowances: Vec<Allowance>,
}

/// A validated, tool-scoped allowance. No shell prefix or glob variant exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allowance(AllowanceKind);
#[derive(Clone, Debug, PartialEq, Eq)]
enum AllowanceKind {
    File {
        tool: String,
        path: WorkspacePath,
        prefix: bool,
    },
    Bash {
        command: String,
        cwd: WorkspacePath,
    },
}

impl Allowance {
    /// Decode a strict allowance object. Paths are normalized component-wise.
    pub fn parse(value: &Value) -> Result<Self, Refusal> {
        #[derive(Deserialize)]
        #[serde(tag = "tool", rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            Write { path: String },
            Edit { path: String },
            Bash { command: String, cwd: String },
        }
        let wire: Wire =
            serde_json::from_value(value.clone()).map_err(|_| invalid("invalid allowance"))?;
        let kind = match wire {
            Wire::Write { path } => Self::file("write", &path)?,
            Wire::Edit { path } => Self::file("edit", &path)?,
            Wire::Bash { command, cwd } => {
                if command.is_empty() || command.len() > 65536 {
                    return Err(invalid("invalid bash allowance command"));
                }
                AllowanceKind::Bash {
                    command,
                    cwd: WorkspacePath::new(&cwd)?,
                }
            }
        };
        Ok(Self(kind))
    }

    fn file(tool: &str, raw: &str) -> Result<AllowanceKind, Refusal> {
        let path = WorkspacePath::new(raw)?;
        let prefix = raw.ends_with('/') || path.as_str() == ".";
        Ok(AllowanceKind::File {
            tool: tool.into(),
            path,
            prefix,
        })
    }

    /// Narrowest reusable allowance, or none for reads (which never use allowances).
    pub fn remember(plan: &Plan) -> Option<Self> {
        match plan.operation() {
            Operation::Write { path, .. } | Operation::Edit { path, .. } => {
                Some(Self(AllowanceKind::File {
                    tool: plan.name().into(),
                    path: path.clone(),
                    prefix: path.as_str() == ".",
                }))
            }
            Operation::Bash { command, cwd, .. } => Some(Self(AllowanceKind::Bash {
                command: command.clone(),
                cwd: cwd.clone(),
            })),
            Operation::Read { .. } => None,
        }
    }

    /// Match validated plan arguments only; never overrides denial or protection.
    pub fn matches(&self, plan: &Plan) -> bool {
        match (&self.0, plan.operation()) {
            (AllowanceKind::File { tool, path, prefix }, operation) => {
                tool == plan.name()
                    && operation.path().is_some_and(|target| {
                        if *prefix {
                            target.beneath(path) && (path.as_str() == "." || target != path)
                        } else {
                            target == path
                        }
                    })
            }
            (
                AllowanceKind::Bash { command, cwd },
                Operation::Bash {
                    command: actual,
                    cwd: actual_cwd,
                    ..
                },
            ) => command == actual && cwd == actual_cwd,
            _ => false,
        }
    }

    fn value(&self) -> Value {
        match &self.0 {
            AllowanceKind::File { tool, path, prefix } => {
                let pattern = if *prefix && path.as_str() != "." {
                    format!("{}/", path.as_str())
                } else {
                    path.as_str().into()
                };
                json!({"tool":tool,"path":pattern})
            }
            AllowanceKind::Bash { command, cwd } => {
                json!({"tool":"bash","command":command,"cwd":cwd})
            }
        }
    }
}

impl PolicySnapshot {
    /// Verify and decode the configuration artifact without dropping unknown fields.
    /// Configuration errors are session-level [`PolicyError`]s, not tool results.
    pub fn parse(artifact: ArtifactRef, bytes: &[u8]) -> Result<Self, PolicyError> {
        if bytes.len() as u64 != artifact.bytes || Digest::of(bytes) != artifact.sha256 {
            return Err(PolicyError::Mismatch);
        }
        let Value::Object(object) =
            codec::unique_json(bytes).map_err(|_| PolicyError::NotAnObject)?
        else {
            return Err(PolicyError::NotAnObject);
        };
        let field = |key: &'static str| {
            object
                .get(key)
                .cloned()
                .ok_or(PolicyError::MissingField(key))
        };
        let revision = serde_json::from_value(field("policy_revision")?)
            .map_err(|_| PolicyError::InvalidField("policy_revision"))?;
        let enabled_tools = serde_json::from_value(field("enabled_tools")?)
            .map_err(|_| PolicyError::InvalidField("enabled_tools"))?;
        let workspace_read = serde_json::from_value(field("workspace_read")?)
            .map_err(|_| PolicyError::InvalidField("workspace_read"))?;
        let workspace_mutation = serde_json::from_value(field("workspace_mutation")?)
            .map_err(|_| PolicyError::InvalidField("workspace_mutation"))?;
        let process = serde_json::from_value(field("process")?)
            .map_err(|_| PolicyError::InvalidField("process"))?;
        if field("network")? != "deny" {
            return Err(PolicyError::NetworkUnsupported);
        }
        let protected_paths: Vec<String> = serde_json::from_value(
            object
                .get("protected_paths")
                .cloned()
                .unwrap_or_else(|| json!([])),
        )
        .map_err(|_| PolicyError::InvalidField("protected_paths"))?;
        let mut protected_paths: Vec<_> = protected_paths
            .iter()
            .map(|p| {
                WorkspacePath::new(p).map_err(|_| PolicyError::InvalidField("protected_paths"))
            })
            .collect::<Result<_, _>>()?;
        protected_paths.sort();
        protected_paths.dedup();
        let allowances = match object.get("allowances") {
            None => Vec::new(),
            Some(Value::Array(values)) => values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    Allowance::parse(value).map_err(|_| PolicyError::InvalidAllowance { index })
                })
                .collect::<Result<_, _>>()?,
            _ => return Err(PolicyError::InvalidField("allowances")),
        };
        Ok(Self {
            artifact,
            object,
            revision,
            enabled_tools,
            workspace_read,
            workspace_mutation,
            process,
            protected_paths,
            allowances,
        })
    }

    /// Exact artifact this policy was read from.
    pub fn artifact(&self) -> &ArtifactRef {
        &self.artifact
    }
    /// Revision to bind into an approval or plan.
    pub fn revision(&self) -> &str {
        &self.revision
    }
    /// Whether the configuration enables this name.
    pub fn enabled(&self, name: &str) -> bool {
        self.enabled_tools.iter().any(|n| n == name)
    }
    /// Validated allowances, in configuration order.
    pub fn allowances(&self) -> &[Allowance] {
        &self.allowances
    }
    pub(super) fn protected_paths(&self) -> &[WorkspacePath] {
        &self.protected_paths
    }
    pub(super) fn mutation_denied(&self) -> bool {
        matches!(self.workspace_mutation, RestrictedPolicy::Deny)
    }
    pub(super) fn protection(&self, path: &WorkspacePath, facts: Protection) -> Protection {
        if self.protected_paths.iter().any(|p| path.beneath(p)) {
            return Protection::Denied;
        }
        let git = WorkspacePath::new(".git").expect("constant relative path");
        if path.beneath(&git) {
            facts.max(Protection::ReadOnly)
        } else {
            facts
        }
    }

    /// Append a nonduplicate allowance and replace only policy_revision/allowances.
    /// Existing allowances retain their original values and order, including spelling.
    pub fn grant(&self, allowance: Allowance, new_revision: &str) -> Vec<u8> {
        let mut object = self.object.clone();
        let entries = object
            .entry("allowances")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("validated allowances");
        if !self.allowances.contains(&allowance) {
            entries.push(allowance.value());
        }
        object.insert("policy_revision".into(), json!(new_revision));
        serde_json::to_vec(&object).expect("JSON values serialize")
    }

    /// Remove matching allowances, carrying all other values forward unchanged.
    pub fn revoke(&self, allowance: &Allowance, new_revision: &str) -> Vec<u8> {
        let mut object = self.object.clone();
        if let Some(Value::Array(values)) = object.get_mut("allowances") {
            let mut index = 0;
            values.retain(|_| {
                let keep = &self.allowances[index] != allowance;
                index += 1;
                keep
            });
        }
        object.insert("policy_revision".into(), json!(new_revision));
        serde_json::to_vec(&object).expect("JSON values serialize")
    }
}

/// An exact call/plan/revision binding. IDs, persistence and expiry are step 4.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalRequest {
    /// Proposal being authorized.
    pub call: CallRef,
    /// SHA-256 of plan_bytes.
    pub plan_digest: Digest,
    /// Exact canonical plan artifact bytes.
    pub plan_bytes: Vec<u8>,
    /// Revision at planning time.
    pub policy_revision: String,
}

/// Pure policy outcome; allowing always returns the exact immutable plan.
#[derive(Clone, Debug)]
pub enum Authorization {
    /// No approval needed under this configuration.
    Allow(Box<Plan>),
    /// Ask for this plan only.
    Ask(ApprovalRequest),
    /// Nonexecuted result.
    Deny(Refusal),
}

/// Evaluate protection first, then the tool's policy row, then allowances.
/// A stale policy or substituted facts cannot authorize a previously built plan.
pub fn decide(plan: &Plan, facts: &PlanFacts, policy: &PolicySnapshot) -> Authorization {
    if let Err(refusal) = facts.check(plan.operation().mutation()) {
        return Authorization::Deny(refusal);
    }
    if !plan.bound_to(policy, facts) || !policy.enabled(plan.name()) {
        return Authorization::Deny(Refusal::new(
            ResultCode::PolicyDenied,
            "plan is not bound to current enabled policy and facts",
        ));
    }
    let deny = || {
        Authorization::Deny(Refusal::new(
            ResultCode::PolicyDenied,
            "operation denied by session policy",
        ))
    };
    let ask = || {
        Authorization::Ask(ApprovalRequest {
            call: plan.call().clone(),
            plan_digest: plan.digest().clone(),
            plan_bytes: plan.bytes().to_vec(),
            policy_revision: plan.policy_revision().into(),
        })
    };
    match plan.operation() {
        Operation::Read { .. } => match policy.workspace_read {
            ReadPolicy::Allow => Authorization::Allow(Box::new(plan.clone())),
            ReadPolicy::Ask => ask(),
            ReadPolicy::Deny => deny(),
        },
        operation => {
            if (operation.mutation() && policy.mutation_denied())
                || (matches!(operation, Operation::Bash { .. })
                    && matches!(policy.process, RestrictedPolicy::Deny))
            {
                return deny();
            }
            if policy.allowances.iter().any(|a| a.matches(plan)) {
                Authorization::Allow(Box::new(plan.clone()))
            } else {
                ask()
            }
        }
    }
}

fn invalid(detail: &str) -> Refusal {
    Refusal::new(ResultCode::InvalidArguments, detail)
}
