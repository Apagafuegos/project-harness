use std::{collections::BTreeMap, sync::OnceLock};

use serde::Serialize;
use serde_json::{Value, json};

use super::{Plan, PolicySnapshot, Refusal, WorkspacePath, WorkspaceView};
use crate::session::{CallRef, EffectClass, EffectSet, ExecutionBackend, ResultCode};

pub(super) const MIB: u64 = 1024 * 1024;
const MULTI_EDIT_LIMIT: u64 = 64;

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
    ReadLines {
        path: WorkspacePath,
        start_line: u64,
        line_count: u64,
        max_bytes: u64,
    },
    ReadMany {
        paths: Vec<super::read_many::ReadSpec>,
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
    MultiEdit {
        files: Vec<MultiEditFile>,
    },
    List {
        path: WorkspacePath,
    },
    Glob {
        pattern: String,
        path: WorkspacePath,
        include_ignored: bool,
    },
    Grep {
        pattern: String,
        path: WorkspacePath,
        glob: Option<String>,
        case_insensitive: bool,
        output: String,
        context_lines: u64,
        max_results: u64,
        include_ignored: bool,
    },
    Bash {
        command: String,
        cwd: WorkspacePath,
        timeout_ms: u64,
    },
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct MultiEditFile {
    pub(super) path: WorkspacePath,
    pub(super) edits: Vec<MultiEditReplacement>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct MultiEditReplacement {
    pub(super) old_text: String,
    pub(super) new_text: String,
}

impl Operation {
    pub(super) fn index(&self) -> usize {
        match self {
            Self::Read { .. } | Self::ReadLines { .. } | Self::ReadMany { .. } => 0,
            Self::Write { .. } => 1,
            Self::Edit { .. } => 2,
            Self::Bash { .. } => 3,
            Self::List { .. } => 4,
            Self::Glob { .. } => 5,
            Self::Grep { .. } => 6,
            Self::MultiEdit { .. } => 7,
        }
    }
    pub(super) fn path(&self) -> Option<&WorkspacePath> {
        match self {
            Self::Read { path, .. }
            | Self::ReadLines { path, .. }
            | Self::Write { path, .. }
            | Self::Edit { path, .. }
            | Self::List { path }
            | Self::Glob { path, .. }
            | Self::Grep { path, .. } => Some(path),
            Self::Bash { .. } | Self::MultiEdit { .. } | Self::ReadMany { .. } => None,
        }
    }
    pub(super) fn search(&self) -> bool {
        matches!(
            self,
            Self::List { .. } | Self::Glob { .. } | Self::Grep { .. }
        )
    }
    pub(super) fn mutation(&self) -> bool {
        matches!(
            self,
            Self::Write { .. } | Self::Edit { .. } | Self::MultiEdit { .. }
        )
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
    tools: [Builtin; 8],
}

impl Registry {
    /// All definitions, regardless of executor availability (step 4 gates dispatch).
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

    /// All definitions in fixed read/write/edit/bash/list/glob/grep/multi_edit order.
    pub fn definitions(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.tools.iter().map(|t| &t.definition)
    }
}

fn definition(index: usize) -> ToolDefinition {
    if (4..=6).contains(&index) {
        return super::search::definition(index);
    }
    let path = json!({"type":"string", "minLength":1, "description":"Workspace-relative Linux path; no absolute paths, traversal, NUL, or symlinks. At most 4096 UTF-8 bytes."});
    if index == 0 {
        return ToolDefinition {
            name: "read".into(),
            description: "Read UTF-8 workspace files by byte range or one-based LF-delimited line range; read up to eight distinct files in one bounded result. Byte mode defaults to offset 0 and 1 MiB. No symlinks or hard links.".into(),
            input_schema: super::read_many::schema(path),
            effects: EffectSet::new([EffectClass::Read]).expect("one effect"),
            backend: ExecutionBackend::TrustedFs,
        };
    }
    let (name, description, properties, required, effect) = match index {
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
        3 => (
            "bash",
            "Plan a nonempty shell command (at most 64 KiB UTF-8) inside Bubblewrap, without network and with pipefail (rightmost failing pipeline status). cwd defaults to workspace root; timeout_ms defaults to 120000 (1..600000). No elevation or host-environment inheritance.",
            json!({"command":{"type":"string","minLength":1,"maxLength":65536},"cwd":path,"timeout_ms":{"type":"integer","minimum":1,"maximum":600000}}),
            json!(["command"]),
            EffectClass::Process,
        ),
        7 => (
            "multi_edit",
            "Apply up to 64 exact literal replacements across existing workspace files in one approval. Repeated paths are grouped in input order; files are previewed and committed in sorted path order. Every replacement must match exactly once. No symlinks or hard links. A commit failure can leave a reported partial result; use undo to reverse committed files.",
            json!({"edits":{"type":"array","minItems":1,"maxItems":MULTI_EDIT_LIMIT,"items":{"type":"object","properties":{"path":path,"old_text":{"type":"string","minLength":1,"maxLength":MIB},"new_text":{"type":"string","maxLength":MIB}},"required":["path","old_text","new_text"],"additionalProperties":false}}}),
            json!(["edits"]),
            EffectClass::FilesystemMutation,
        ),
        _ => unreachable!("unknown built-in tool index"),
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
    static SCHEMAS: [OnceLock<jsonschema::Validator>; 8] = [const { OnceLock::new() }; 8];
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
            if self.index == 0 && arguments.is_object()
                && arguments.get("path").is_none() && arguments.get("paths").is_none()
            {
                return Err(argument_refusal("\"\"", "required path or paths"));
            }
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
            0 => match super::read_many::parse(arguments)? {
                super::read_many::ReadRequest::Single(spec) => match spec.range {
                    super::read_many::ReadRange::Bytes {
                        offset_bytes,
                        max_bytes,
                    } => Operation::Read {
                        path: spec.path,
                        offset_bytes,
                        max_bytes,
                    },
                    super::read_many::ReadRange::Lines {
                        start_line,
                        line_count,
                        max_bytes,
                    } => Operation::ReadLines {
                        path: spec.path,
                        start_line,
                        line_count,
                        max_bytes,
                    },
                },
                super::read_many::ReadRequest::Many(paths) => Operation::ReadMany { paths },
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
            7 => {
                if serde_json::to_vec(arguments).expect("JSON arguments").len() > MIB as usize {
                    return Err(argument_refusal("/edits", "aggregate arguments exceed 1048576 bytes"));
                }
                let mut grouped: BTreeMap<WorkspacePath, Vec<MultiEditReplacement>> =
                    BTreeMap::new();
                for (index, edit) in arguments["edits"]
                    .as_array()
                    .expect("schema array")
                    .iter()
                    .enumerate()
                {
                    let path = WorkspacePath::new(edit["path"].as_str().expect("schema path"))
                        .map_err(|error| {
                            argument_refusal(&format!("/edits/{index}/path"), &error.detail)
                        })?;
                    let old_text = edit["old_text"].as_str().expect("schema old_text");
                    let new_text = edit["new_text"].as_str().expect("schema new_text");
                    if old_text.len() > MIB as usize || new_text.len() > MIB as usize {
                        return Err(argument_refusal(
                            &format!("/edits/{index}"),
                            "replacement text exceeds 1048576 UTF-8 bytes",
                        ));
                    }
                    grouped.entry(path).or_default().push(MultiEditReplacement {
                        old_text: old_text.to_owned(),
                        new_text: new_text.to_owned(),
                    });
                }
                Operation::MultiEdit {
                    files: grouped
                        .into_iter()
                        .map(|(path, edits)| MultiEditFile { path, edits })
                        .collect(),
                }
            }
            4 => Operation::List {
                path: path("path")?,
            },
            5 => {
                let pattern = string("pattern");
                super::search::glob_matcher(&pattern)?;
                Operation::Glob {
                    pattern,
                    path: path("path")?,
                    include_ignored: arguments["include_ignored"].as_bool().unwrap_or(false),
                }
            }
            6 => {
                let pattern = string("pattern");
                let case_insensitive = arguments["case_insensitive"].as_bool().unwrap_or(false);
                super::search::regex(&pattern, case_insensitive)?;
                let glob = arguments["glob"].as_str().map(str::to_owned);
                if let Some(pattern) = &glob {
                    super::search::glob_matcher(pattern)?;
                }
                Operation::Grep {
                    pattern,
                    path: path("path")?,
                    glob,
                    case_insensitive,
                    output: arguments["output"].as_str().unwrap_or("matches").into(),
                    context_lines: integer("context_lines", 0)?,
                    max_results: integer("max_results", 200)?,
                    include_ignored: arguments["include_ignored"].as_bool().unwrap_or(false),
                }
            }
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
