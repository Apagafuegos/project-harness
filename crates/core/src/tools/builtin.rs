use std::sync::OnceLock;

use serde::Serialize;
use serde_json::{Value, json};

use super::{Plan, PolicySnapshot, Refusal, WorkspacePath, WorkspaceView};
use crate::session::{CallRef, EffectClass, EffectSet, ExecutionBackend, ResultCode};

pub(super) const MIB: u64 = 1024 * 1024;

/// Provider-facing description of a compiled-in capability.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolDefinition {
    /// Reserved tool name.
    pub name: String,
    /// Model-facing usage and limits.
    pub description: String,
    /// Strict JSON Schema argument object.
    pub input_schema: Value,
    /// Declared effects; plan effects are the intent's source of truth.
    pub effects: EffectSet,
    /// Trusted implementation backend, never selected by arguments.
    pub backend: ExecutionBackend,
}

/// Validation is pure in arguments; planning reads only the supplied view.
pub trait Tool: Send + Sync {
    /// The immutable provider-facing definition.
    fn definition(&self) -> &ToolDefinition;
    /// Validate schema, semantics, and defaults without workspace access.
    fn validate(&self, arguments: &Value) -> Result<ValidatedOperation, Refusal>;
    /// Bind current workspace facts, policy revision, and call into a plan.
    fn plan(
        &self,
        operation: &ValidatedOperation,
        call: &CallRef,
        workspace: &dyn WorkspaceView,
        policy: &PolicySnapshot,
    ) -> Result<Plan, Refusal>;
}

/// An opaque, validated/defaulted operation. Only a built-in tool constructs it.
#[derive(Clone, Debug)]
pub struct ValidatedOperation(pub(super) Operation);

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub(super) enum Operation {
    Read {
        path: WorkspacePath,
        offset_bytes: u64,
        max_bytes: u64,
    },
    Write {
        path: WorkspacePath,
        text: String,
        overwrite: bool,
    },
    Edit {
        path: WorkspacePath,
        old_text: String,
        new_text: String,
    },
    Bash {
        command: String,
        cwd: WorkspacePath,
        timeout_ms: u64,
    },
}

impl Operation {
    pub(super) fn index(&self) -> usize {
        match self {
            Self::Read { .. } => 0,
            Self::Write { .. } => 1,
            Self::Edit { .. } => 2,
            Self::Bash { .. } => 3,
        }
    }
    pub(super) fn path(&self) -> Option<&WorkspacePath> {
        match self {
            Self::Read { path, .. } | Self::Write { path, .. } | Self::Edit { path, .. } => {
                Some(path)
            }
            Self::Bash { .. } => None,
        }
    }
    pub(super) fn mutation(&self) -> bool {
        matches!(self, Self::Write { .. } | Self::Edit { .. })
    }
}

#[derive(Clone, Debug)]
struct Builtin {
    index: usize,
    definition: ToolDefinition,
}

/// A fixed registry. It has no registration or replacement API.
#[derive(Clone, Debug)]
pub struct Registry {
    tools: [Builtin; 4],
}

impl Registry {
    /// The four definitions, regardless of executor availability (step 4 gates dispatch).
    pub fn builtin() -> Self {
        Self {
            tools: std::array::from_fn(|index| Builtin {
                index,
                definition: definition(index),
            }),
        }
    }

    /// Resolve before validating: unknown names and disabled tools never see arguments.
    pub fn resolve(&self, name: &str, policy: &PolicySnapshot) -> Result<&dyn Tool, Refusal> {
        let tool = self
            .tools
            .iter()
            .find(|t| t.definition.name == name)
            .ok_or_else(|| {
                Refusal::new(ResultCode::UnknownTool, "no built-in tool has this name")
            })?;
        if !policy.enabled(name) {
            return Err(Refusal::new(
                ResultCode::ToolDisabled,
                "tool is disabled by session configuration",
            ));
        }
        Ok(tool)
    }

    /// All definitions in fixed read/write/edit/bash order.
    pub fn definitions(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.tools.iter().map(|t| &t.definition)
    }
}

fn definition(index: usize) -> ToolDefinition {
    let path = json!({"type":"string", "minLength":1, "description":"Workspace-relative Linux path; no absolute paths, traversal, NUL, or symlinks. At most 4096 UTF-8 bytes."});
    let (name, description, properties, required, effect) = match index {
        0 => (
            "read",
            "Read a regular UTF-8 workspace file. Offsets and limits are bytes; max_bytes defaults to 1 MiB. No symlinks or hard links.",
            json!({"path":path, "offset_bytes":{"type":"integer","minimum":0,"maximum":u64::MAX}, "max_bytes":{"type":"integer","minimum":0,"maximum":MIB}}),
            json!(["path"]),
            EffectClass::Read,
        ),
        1 => (
            "write",
            "Write at most 1 MiB of UTF-8 text to a workspace file. Parent must exist. overwrite defaults to false. Requires mutation authorization.",
            json!({"path":path,"text":{"type":"string","maxLength":MIB},"overwrite":{"type":"boolean"}}),
            json!(["path", "text"]),
            EffectClass::FilesystemMutation,
        ),
        2 => (
            "edit",
            "Replace exactly one literal occurrence of nonempty old_text with new_text (overlapping occurrences also count) in a regular UTF-8 workspace file. No regular expressions. Requires mutation authorization.",
            json!({"path":path,"old_text":{"type":"string","minLength":1},"new_text":{"type":"string"}}),
            json!(["path", "old_text", "new_text"]),
            EffectClass::FilesystemMutation,
        ),
        _ => (
            "bash",
            "Plan a nonempty shell command (at most 64 KiB UTF-8) inside Bubblewrap, without network. cwd defaults to workspace root; timeout_ms defaults to 120000 (1..600000). No elevation or host-environment inheritance.",
            json!({"command":{"type":"string","minLength":1,"maxLength":65536},"cwd":path,"timeout_ms":{"type":"integer","minimum":1,"maximum":600000}}),
            json!(["command"]),
            EffectClass::Process,
        ),
    };
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        effects: EffectSet::new([effect]).expect("one effect"),
        backend: if index == 3 {
            ExecutionBackend::Bubblewrap
        } else {
            ExecutionBackend::TrustedFs
        },
    }
}

fn schema(index: usize) -> &'static jsonschema::Validator {
    static SCHEMAS: [OnceLock<jsonschema::Validator>; 4] = [const { OnceLock::new() }; 4];
    SCHEMAS[index].get_or_init(|| {
        jsonschema::validator_for(&definition(index).input_schema).expect("built-in schema")
    })
}

fn argument_refusal(location: &str, rule: &str) -> Refusal {
    // Preserve the rule even if a model supplies an enormous property name.
    let budget = 200usize.saturating_sub(rule.len() + 2);
    let end = location
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(location.len()))
        .take_while(|&i| i <= budget)
        .last()
        .unwrap_or(0);
    Refusal::new(
        ResultCode::InvalidArguments,
        &format!("{}: {rule}", &location[..end]),
    )
}

fn schema_refusal(error: &jsonschema::ValidationError<'_>) -> Refusal {
    use jsonschema::error::ValidationErrorKind;
    let mut location = error.instance_path().to_string();
    let rule = match error.kind() {
        ValidationErrorKind::Required { property } => {
            location.push('/');
            location.push_str(
                &property
                    .as_str()
                    .expect("schema required name")
                    .replace('~', "~0")
                    .replace('/', "~1"),
            );
            "required".into()
        }
        ValidationErrorKind::AdditionalProperties { unexpected } => {
            if let Some(key) = unexpected.first() {
                location.push('/');
                location.push_str(&key.replace('~', "~0").replace('/', "~1"));
            }
            "unknown key".into()
        }
        ValidationErrorKind::Type { .. } => "type mismatch".into(),
        ValidationErrorKind::Minimum { limit } => format!("minimum is {limit}"),
        ValidationErrorKind::Maximum { limit } => format!("maximum is {limit}"),
        ValidationErrorKind::MinLength { limit } => format!("length must be at least {limit}"),
        ValidationErrorKind::MaxLength { limit } => format!("length exceeds {limit} UTF-8 bytes"),
        kind => format!("{} rule failed", kind.keyword()),
    };
    argument_refusal(
        if location.is_empty() {
            "\"\""
        } else {
            &location
        },
        &rule,
    )
}

impl Tool for Builtin {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn validate(&self, arguments: &Value) -> Result<ValidatedOperation, Refusal> {
        if let Err(error) = schema(self.index).validate(arguments) {
            return Err(schema_refusal(&error));
        }
        let string = |key: &str| arguments[key].as_str().expect("schema string").to_owned();
        let path = |key: &str| {
            WorkspacePath::new(arguments[key].as_str().unwrap_or("."))
                .map_err(|error| argument_refusal(&format!("/{key}"), &error.detail))
        };
        // JSON Schema integers include 1.0. Normalize them, but never treat a
        // present, unrepresentable number as a missing argument with a default.
        let integer = |key: &str, default: u64| -> Result<u64, Refusal> {
            let Some(value) = arguments.get(key) else {
                return Ok(default);
            };
            value
                .as_u64()
                .or_else(|| {
                    value
                        .as_f64()
                        .filter(|n| *n >= 0.0 && n.fract() == 0.0 && *n < u64::MAX as f64)
                        .map(|n| n as u64)
                })
                .ok_or_else(|| argument_refusal(&format!("/{key}"), "integer is out of range"))
        };
        let operation = match self.index {
            0 => Operation::Read {
                path: path("path")?,
                offset_bytes: integer("offset_bytes", 0)?,
                max_bytes: integer("max_bytes", MIB)?,
            },
            1 => {
                let text = string("text");
                if text.len() as u64 > MIB {
                    return Err(argument_refusal(
                        "/text",
                        "length exceeds 1048576 UTF-8 bytes",
                    ));
                }
                Operation::Write {
                    path: path("path")?,
                    text,
                    overwrite: arguments["overwrite"].as_bool().unwrap_or(false),
                }
            }
            2 => Operation::Edit {
                path: path("path")?,
                old_text: string("old_text"),
                new_text: string("new_text"),
            },
            _ => {
                let command = string("command");
                if command.len() > 65536 {
                    return Err(argument_refusal(
                        "/command",
                        "length exceeds 65536 UTF-8 bytes",
                    ));
                }
                Operation::Bash {
                    command,
                    cwd: path("cwd")?,
                    timeout_ms: integer("timeout_ms", 120000)?,
                }
            }
        };
        Ok(ValidatedOperation(operation))
    }

    fn plan(
        &self,
        operation: &ValidatedOperation,
        call: &CallRef,
        workspace: &dyn WorkspaceView,
        policy: &PolicySnapshot,
    ) -> Result<Plan, Refusal> {
        if operation.0.index() != self.index {
            return Err(Refusal::new(
                ResultCode::InvalidArguments,
                "operation belongs to another tool",
            ));
        }
        Plan::build(&self.definition, &operation.0, call, workspace, policy)
    }
}
