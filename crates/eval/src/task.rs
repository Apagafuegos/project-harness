//! The task format: a workspace, a replayable history script, requirements,
//! modified files, a continuation prompt, a hidden check and offline scripts.
//!
//! Every task is a directory under `eval/tasks/`:
//!
//! ```text
//! <name>/task.json                 {"name","shape","description"}
//! <name>/workspace/                copied into a fresh scratch directory per run
//! <name>/history.jsonl             the pre-compaction conversation, replayed
//! <name>/requirements.json         requirements stated in the history
//! <name>/modified_files.json       workspace paths the history changed
//! <name>/continue.md               the continuation prompt sent after compaction
//! <name>/check.sh                  the hidden completion check (never sent to a model)
//! <name>/controls.json             optional {"pin":[n],"exclude":[m]} turn ordinals
//! <name>/scripted/summary.md       offline summary text, for model strategies
//! <name>/scripted/probe.md         offline probe answer
//! <name>/scripted/continue.jsonl   offline continuation steps
//! ```
//!
//! The format is documented in `eval/README.md` and ADR 0027.

use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use harness_core::session::Digest;
use serde::Deserialize;
use serde_json::Value;

/// Minimum history length of a task, in turns.
pub const MIN_TURNS: usize = 4;
/// Maximum history length of a task, in turns.
pub const MAX_TURNS: usize = 8;
/// Minimum number of tool round trips a history must produce.
pub const MIN_TOOL_CALLS: usize = 2;
/// Minimum number of requirements a task must state.
pub const MIN_REQUIREMENTS: usize = 2;
/// Marker every hidden check must carry, so the harness can prove it knows
/// where the check's contents are.
pub const CHECK_SENTINEL: &str = "# harness-eval hidden check:";

/// A task directory that failed validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskError(pub String);

impl std::fmt::Display for TaskError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for TaskError {}

fn fail<T>(message: impl Into<String>) -> Result<T, TaskError> {
    Err(TaskError(message.into()))
}

/// The shape of work a task represents. The starter suite covers every shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Shape {
    /// A refactor under a constraint.
    Refactor,
    /// A bug fix with a test.
    BugFix,
    /// A feature with a style rule.
    Feature,
    /// A change spanning several files.
    MultiFile,
}

impl Shape {
    /// Stable spelling used in `task.json` and result records.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Refactor => "refactor",
            Self::BugFix => "bug_fix",
            Self::Feature => "feature",
            Self::MultiFile => "multi_file",
        }
    }
}

/// One step of a replayable history or continuation script.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// A user message. It opens a turn.
    User {
        /// Prompt text.
        text: String,
    },
    /// A scripted assistant reply. It ends a turn unless a tool call follows.
    Assistant {
        /// Reply text.
        text: String,
    },
    /// A scripted tool proposal. The tool really runs against the workspace.
    ToolCall {
        /// Built-in tool name.
        tool: String,
        /// Tool arguments.
        arguments: Value,
        /// Optional narration emitted before the call.
        say: String,
    },
}

/// One turn: the user prompt and the scripted steps that follow it.
#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    /// The user message that opened the turn.
    pub prompt: String,
    /// Scripted assistant and tool steps, in order.
    pub steps: Vec<Step>,
}

impl Turn {
    /// Number of real tool calls this turn produces.
    pub fn tool_calls(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step, Step::ToolCall { .. }))
            .count()
    }
}

/// Optional pre-compaction context controls, by 1-based turn ordinal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Controls {
    /// Turns to pin before compacting.
    pub pin: Vec<usize>,
    /// Turns to exclude before compacting.
    pub exclude: Vec<usize>,
}

/// Offline scripts for the stages after the replayed history.
#[derive(Clone, Debug, PartialEq)]
pub struct Scripted {
    /// Summary text handed to the model-based strategies.
    pub summary: String,
    /// The probe turn's scripted answer.
    pub probe: String,
    /// The continuation's scripted steps.
    pub continuation: Vec<Step>,
}

/// A loaded, validated task.
#[derive(Clone, Debug)]
pub struct Task {
    name: String,
    shape: Shape,
    description: String,
    dir: PathBuf,
    workspace: PathBuf,
    check: PathBuf,
    turns: Vec<Turn>,
    requirements: Vec<crate::matcher::Requirement>,
    modified_files: Vec<String>,
    continue_prompt: String,
    controls: Controls,
    scripted: Scripted,
    fingerprint: Digest,
}

impl Task {
    /// Load and validate the task in `dir`.
    pub fn load(dir: impl AsRef<Path>) -> Result<Self, TaskError> {
        let dir = dir.as_ref();
        let name = directory_name(dir)?;
        let meta: TaskFile = read_json(&dir.join("task.json"))?;
        if meta.name != name {
            return fail(format!(
                "{}: task.json name {:?} does not match the directory",
                dir.display(),
                meta.name
            ));
        }
        let shape = match meta.shape.as_str() {
            "refactor" => Shape::Refactor,
            "bug_fix" => Shape::BugFix,
            "feature" => Shape::Feature,
            "multi_file" => Shape::MultiFile,
            other => return fail(format!("{name}: unknown shape {other:?}")),
        };
        let steps = read_jsonl(&dir.join("history.jsonl"))?;
        let turns = group_turns(&name, steps)?;
        validate_turn_count(&name, turns.len())?;
        let tool_calls: usize = turns.iter().map(Turn::tool_calls).sum();
        if tool_calls < MIN_TOOL_CALLS {
            return fail(format!(
                "{name}: history produces {tool_calls} tool calls; at least {MIN_TOOL_CALLS} are required"
            ));
        }
        let requirements = read_requirements(&dir.join("requirements.json"), &name)?;
        let modified: ModifiedFiles = read_json(&dir.join("modified_files.json"))?;
        validate_paths(&name, &modified.paths)?;
        let continue_prompt = read_text(&dir.join("continue.md"))?;
        let check_bytes = read_text(&dir.join("check.sh"))?;
        if !check_bytes.contains(CHECK_SENTINEL) {
            return fail(format!(
                "{name}: check.sh must contain the marker {CHECK_SENTINEL:?}"
            ));
        }
        let controls_path = dir.join("controls.json");
        let controls = if controls_path.is_file() {
            let wire: ControlsFile = read_json(&controls_path)?;
            let controls = Controls {
                pin: wire.pin,
                exclude: wire.exclude,
            };
            validate_controls(&name, &controls, turns.len())?;
            controls
        } else {
            Controls::default()
        };
        let scripted = Scripted {
            summary: read_text(&dir.join("scripted/summary.md"))?,
            probe: read_text(&dir.join("scripted/probe.md"))?,
            continuation: read_jsonl(&dir.join("scripted/continue.jsonl"))?,
        };
        validate_continuation(&name, &scripted.continuation)?;
        let workspace = dir.join("workspace");
        if !workspace.is_dir() {
            return fail(format!("{name}: workspace/ is missing"));
        }
        if read_tree(&workspace)?.is_empty() {
            return fail(format!("{name}: workspace/ is empty"));
        }
        let fingerprint = fingerprint(dir)?;
        Ok(Self {
            name,
            shape,
            description: meta.description,
            dir: dir.to_path_buf(),
            workspace,
            check: dir.join("check.sh"),
            turns,
            requirements,
            modified_files: modified.paths,
            continue_prompt,
            controls,
            scripted,
            fingerprint,
        })
    }

    /// Task name; equal to its directory name.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Work shape, recorded in the result.
    pub fn shape(&self) -> Shape {
        self.shape
    }
    /// One-line description from `task.json`.
    pub fn description(&self) -> &str {
        &self.description
    }
    /// The task directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    /// The pristine workspace tree, copied per run.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
    /// The hidden check script. Never sent to a model.
    pub fn check(&self) -> &Path {
        &self.check
    }
    /// The pre-compaction conversation, as replayed turns.
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }
    /// Requirements stated in the history.
    pub fn requirements(&self) -> &[crate::matcher::Requirement] {
        &self.requirements
    }
    /// Workspace paths the history modified, as scored for recall.
    pub fn modified_files(&self) -> &[String] {
        &self.modified_files
    }
    /// The continuation prompt sent after compaction.
    pub fn continue_prompt(&self) -> &str {
        &self.continue_prompt
    }
    /// Optional pre-compaction pins and exclusions.
    pub fn controls(&self) -> &Controls {
        &self.controls
    }
    /// Offline scripts for the post-history stages.
    pub fn scripted(&self) -> &Scripted {
        &self.scripted
    }
    /// Digest of every file in the task directory, path-bound and order-stable.
    pub fn fingerprint(&self) -> &Digest {
        &self.fingerprint
    }
}

/// Every task under one root, in name order.
#[derive(Clone, Debug)]
pub struct Suite {
    tasks: Vec<Task>,
}

impl Suite {
    /// Load and validate every task directory under `root`.
    pub fn load(root: impl AsRef<Path>) -> Result<Self, TaskError> {
        let root = root.as_ref();
        let mut directories = Vec::new();
        let entries = fs::read_dir(root)
            .map_err(|error| TaskError(format!("{}: {error}", root.display())))?;
        for entry in entries {
            let entry = entry.map_err(|error| TaskError(format!("{}: {error}", root.display())))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !entry.path().is_dir() || name.starts_with('.') {
                continue;
            }
            directories.push(entry.path());
        }
        directories.sort();
        if directories.is_empty() {
            return fail(format!("{}: no task directories", root.display()));
        }
        let tasks: Vec<Task> = directories
            .iter()
            .map(Task::load)
            .collect::<Result<_, _>>()?;
        let suite = Self { tasks };
        suite.validate()?;
        Ok(suite)
    }

    /// Tasks in name order.
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// The task named `name`, if present.
    pub fn task(&self, name: &str) -> Option<&Task> {
        self.tasks.iter().find(|task| task.name == name)
    }

    /// Every declared shape must be represented, so the suite spans the kinds of
    /// work the metric is supposed to compare.
    fn validate(&self) -> Result<(), TaskError> {
        if self.tasks.len() < 4 {
            return fail(format!(
                "suite has {} tasks; the starter suite needs at least 4",
                self.tasks.len()
            ));
        }
        let shapes: BTreeSet<Shape> = self.tasks.iter().map(Task::shape).collect();
        for shape in [
            Shape::Refactor,
            Shape::BugFix,
            Shape::Feature,
            Shape::MultiFile,
        ] {
            if !shapes.contains(&shape) {
                return fail(format!("suite has no task with shape {}", shape.as_str()));
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFile {
    name: String,
    shape: String,
    description: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequirement {
    id: String,
    text: String,
    #[serde(default)]
    all_of: Vec<String>,
    #[serde(default)]
    any_of: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequirementsFile {
    requirements: Vec<WireRequirement>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModifiedFiles {
    paths: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ControlsFile {
    pin: Vec<usize>,
    exclude: Vec<usize>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WireStep {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    ToolCall {
        tool: String,
        arguments: Value,
        #[serde(default)]
        say: String,
    },
}

impl From<WireStep> for Step {
    fn from(wire: WireStep) -> Self {
        match wire {
            WireStep::User { text } => Self::User { text },
            WireStep::Assistant { text } => Self::Assistant { text },
            WireStep::ToolCall {
                tool,
                arguments,
                say,
            } => Self::ToolCall {
                tool,
                arguments,
                say,
            },
        }
    }
}

fn directory_name(dir: &Path) -> Result<String, TaskError> {
    dir.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            TaskError(format!(
                "{}: task directory needs a UTF-8 name",
                dir.display()
            ))
        })
}

fn read_text(path: &Path) -> Result<String, TaskError> {
    let text = fs::read_to_string(path)
        .map_err(|error| TaskError(format!("{}: {error}", path.display())))?;
    if text.trim().is_empty() {
        return fail(format!("{}: file is empty", path.display()));
    }
    Ok(text)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, TaskError> {
    let text = read_text(path)?;
    serde_json::from_str(&text).map_err(|error| TaskError(format!("{}: {error}", path.display())))
}

fn read_jsonl(path: &Path) -> Result<Vec<Step>, TaskError> {
    let text = read_text(path)?;
    let mut steps = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let wire: WireStep = serde_json::from_str(line)
            .map_err(|error| TaskError(format!("{}:{}: {error}", path.display(), index + 1)))?;
        steps.push(wire.into());
    }
    if steps.is_empty() {
        return fail(format!("{}: no steps", path.display()));
    }
    Ok(steps)
}

fn group_turns(name: &str, steps: Vec<Step>) -> Result<Vec<Turn>, TaskError> {
    let mut turns: Vec<Turn> = Vec::new();
    for step in steps {
        match step {
            Step::User { text } => {
                if let Some(previous) = turns.last()
                    && previous
                        .steps
                        .last()
                        .is_some_and(|last| !matches!(last, Step::Assistant { .. }))
                {
                    return fail(format!(
                        "{name}: turn {} ends with a tool call, which leaves it unanswered",
                        turns.len()
                    ));
                }
                turns.push(Turn {
                    prompt: text,
                    steps: Vec::new(),
                });
            }
            other => match turns.last_mut() {
                Some(turn) => turn.steps.push(other),
                None => return fail(format!("{name}: history begins with a non-user step")),
            },
        }
    }
    for (index, turn) in turns.iter().enumerate() {
        match turn.steps.last() {
            Some(Step::Assistant { .. }) => {}
            _ => {
                return fail(format!(
                    "{name}: turn {} (1-based) must end with an assistant reply",
                    index + 1
                ));
            }
        }
        // A reply ends its turn, so an assistant step before the last one would
        // silently push the remaining scripts into the next stage.
        if turn.steps[..turn.steps.len() - 1]
            .iter()
            .any(|step| matches!(step, Step::Assistant { .. }))
        {
            return fail(format!(
                "{name}: turn {} (1-based) has an assistant reply before its last step; \
                 a reply ends the turn, so tool calls must come first",
                index + 1
            ));
        }
    }
    Ok(turns)
}

fn validate_turn_count(name: &str, turns: usize) -> Result<(), TaskError> {
    if !(MIN_TURNS..=MAX_TURNS).contains(&turns) {
        return fail(format!(
            "{name}: history has {turns} turns; {MIN_TURNS} to {MAX_TURNS} are required"
        ));
    }
    Ok(())
}

fn read_requirements(
    path: &Path,
    name: &str,
) -> Result<Vec<crate::matcher::Requirement>, TaskError> {
    let wire: RequirementsFile = read_json(path)?;
    if wire.requirements.len() < MIN_REQUIREMENTS {
        return fail(format!(
            "{name}: {} requirements; at least {MIN_REQUIREMENTS} are required",
            wire.requirements.len()
        ));
    }
    let mut seen = BTreeSet::new();
    let mut requirements = Vec::new();
    for requirement in wire.requirements {
        if requirement.id.is_empty() || !seen.insert(requirement.id.clone()) {
            return fail(format!("{name}: empty or duplicate requirement id"));
        }
        if requirement.text.trim().is_empty() {
            return fail(format!(
                "{name}: requirement {} has no text",
                requirement.id
            ));
        }
        let matcher = crate::matcher::Matcher::new(requirement.all_of, requirement.any_of)
            .map_err(|error| {
                TaskError(format!("{name}: requirement {}: {error}", requirement.id))
            })?;
        requirements.push(crate::matcher::Requirement {
            id: requirement.id,
            text: requirement.text,
            matcher,
        });
    }
    Ok(requirements)
}

fn validate_paths(name: &str, paths: &[String]) -> Result<(), TaskError> {
    if paths.is_empty() {
        return fail(format!("{name}: modified_files.json lists no paths"));
    }
    let mut seen = BTreeSet::new();
    for path in paths {
        if path.is_empty()
            || path.starts_with('/')
            || path.split('/').any(|part| part == ".." || part.is_empty())
            || !seen.insert(path.clone())
        {
            return fail(format!(
                "{name}: modified path {path:?} must be unique and workspace-relative"
            ));
        }
    }
    Ok(())
}

fn validate_controls(name: &str, controls: &Controls, turns: usize) -> Result<(), TaskError> {
    // Only turns a compaction would otherwise absorb are worth controlling: the
    // newest two finished turns are retained verbatim regardless.
    let eligible = turns.saturating_sub(2);
    let mut seen = BTreeSet::new();
    for ordinal in controls.pin.iter().chain(controls.exclude.iter()) {
        if *ordinal == 0 || *ordinal > eligible {
            return fail(format!(
                "{name}: control ordinal {ordinal} is outside 1..={eligible}"
            ));
        }
        if !seen.insert(*ordinal) {
            return fail(format!(
                "{name}: turn {ordinal} is controlled more than once"
            ));
        }
    }
    Ok(())
}

fn validate_continuation(name: &str, steps: &[Step]) -> Result<(), TaskError> {
    if steps.is_empty() {
        return fail(format!("{name}: scripted continuation has no steps"));
    }
    if steps.iter().any(|step| matches!(step, Step::User { .. })) {
        return fail(format!(
            "{name}: scripted continuation cannot contain a user step"
        ));
    }
    if !matches!(steps.last(), Some(Step::Assistant { .. })) {
        return fail(format!(
            "{name}: scripted continuation must end with an assistant reply"
        ));
    }
    Ok(())
}

/// Every file below `root`, relative and sorted, with its bytes.
fn read_tree(root: &Path) -> Result<Vec<(String, Vec<u8>)>, TaskError> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<(), TaskError> {
        let entries =
            fs::read_dir(dir).map_err(|error| TaskError(format!("{}: {error}", dir.display())))?;
        for entry in entries {
            let entry = entry.map_err(|error| TaskError(format!("{}: {error}", dir.display())))?;
            let path = entry.path();
            let name = entry
                .file_name()
                .to_str()
                .ok_or_else(|| TaskError(format!("{}: non-UTF-8 entry", path.display())))?
                .to_owned();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| TaskError(format!("{}: outside the task", path.display())))?;
            let relative = relative
                .to_str()
                .ok_or_else(|| TaskError(format!("{}: non-UTF-8 relative path", path.display())))?;
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| TaskError(format!("{}: {error}", path.display())))?;
            if metadata.file_type().is_symlink() {
                return fail(format!(
                    "{}: symlinks are not allowed in a task",
                    path.display()
                ));
            }
            if metadata.is_dir() {
                if name != ".git" {
                    walk(root, &path, out)?;
                }
                continue;
            }
            let bytes = fs::read(&path)
                .map_err(|error| TaskError(format!("{}: {error}", path.display())))?;
            out.push((relative.to_owned(), bytes));
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

/// A digest over the sorted `(path, length, sha256)` manifest of the directory.
fn fingerprint(dir: &Path) -> Result<Digest, TaskError> {
    let files = read_tree(dir)?;
    let mut manifest = String::new();
    for (path, bytes) in &files {
        let digest = Digest::of(bytes);
        writeln!(
            manifest,
            "{path}\t{}\t{}",
            bytes.len(),
            serde_json::to_value(&digest)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default()
        )
        .map_err(|_| TaskError("manifest is not writable".into()))?;
    }
    Ok(Digest::of(manifest.as_bytes()))
}

/// Copy a task workspace into `destination`, which must not exist yet.
pub fn copy_workspace(source: &Path, destination: &Path) -> Result<(), TaskError> {
    copy_tree(source, destination)
}

/// Copy a whole task directory, so a test can change one script without
/// touching the checked-in suite. `destination` must not exist yet.
pub fn copy_task(source: &Path, destination: &Path) -> Result<(), TaskError> {
    copy_tree(source, destination)
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), TaskError> {
    fs::create_dir_all(destination)
        .map_err(|error| TaskError(format!("{}: {error}", destination.display())))?;
    let files = read_tree(source)?;
    for (relative, bytes) in files {
        let target = destination.join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| TaskError(format!("{}: {error}", parent.display())))?;
        }
        fs::write(&target, &bytes)
            .map_err(|error| TaskError(format!("{}: {error}", target.display())))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, relative: &str, text: &str) {
        let path = dir.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn task_dir(root: &Path, name: &str) {
        let dir = root.join(name);
        write(
            &dir,
            "task.json",
            &format!(r#"{{"name":"{name}","shape":"refactor","description":"d"}}"#),
        );
        write(
            &dir,
            "history.jsonl",
            concat!(
                r#"{"type":"user","text":"one"}"#,
                "\n",
                r#"{"type":"assistant","text":"one done"}"#,
                "\n",
                r#"{"type":"user","text":"two"}"#,
                "\n",
                r#"{"type":"tool_call","tool":"read","arguments":{"path":"a"}}"#,
                "\n",
                r#"{"type":"assistant","text":"two done"}"#,
                "\n",
                r#"{"type":"user","text":"three"}"#,
                "\n",
                r#"{"type":"tool_call","tool":"read","arguments":{"path":"a"}}"#,
                "\n",
                r#"{"type":"assistant","text":"three done"}"#,
                "\n",
                r#"{"type":"user","text":"four"}"#,
                "\n",
                r#"{"type":"assistant","text":"four done"}"#,
                "\n",
            ),
        );
        write(
            &dir,
            "requirements.json",
            r#"{"requirements":[{"id":"R1","text":"t","all_of":["one"]},{"id":"R2","text":"t","any_of":["two","three"]}]}"#,
        );
        write(&dir, "modified_files.json", r#"{"paths":["a"]}"#);
        write(&dir, "continue.md", "continue\n");
        write(
            &dir,
            "check.sh",
            &format!("{CHECK_SENTINEL} {name}\nexit 0\n"),
        );
        write(&dir, "workspace/a", "x");
        write(&dir, "scripted/summary.md", "## Task and constraints\ns\n");
        write(&dir, "scripted/probe.md", "probe\n");
        write(
            &dir,
            "scripted/continue.jsonl",
            "{\"type\":\"assistant\",\"text\":\"fixed\"}\n",
        );
    }

    #[test]
    fn a_task_directory_loads_with_its_fingerprint() {
        let temp = tempfile::tempdir().unwrap();
        task_dir(temp.path(), "one");
        let task = Task::load(temp.path().join("one")).unwrap();
        assert_eq!(task.name(), "one");
        assert_eq!(task.shape(), Shape::Refactor);
        assert_eq!(task.turns().len(), 4);
        assert_eq!(task.turns()[1].tool_calls(), 1);
        assert_eq!(task.requirements().len(), 2);
        assert_eq!(task.modified_files(), ["a"]);
        let again = Task::load(temp.path().join("one")).unwrap();
        assert_eq!(task.fingerprint(), again.fingerprint());
        write(&temp.path().join("one"), "workspace/a", "y");
        let changed = Task::load(temp.path().join("one")).unwrap();
        assert_ne!(task.fingerprint(), changed.fingerprint());
    }

    #[test]
    fn an_unanswered_tool_call_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        task_dir(temp.path(), "one");
        write(
            &temp.path().join("one"),
            "history.jsonl",
            concat!(
                r#"{"type":"user","text":"one"}"#,
                "\n",
                r#"{"type":"tool_call","tool":"read","arguments":{"path":"a"}}"#,
                "\n",
                r#"{"type":"user","text":"two"}"#,
                "\n",
                r#"{"type":"assistant","text":"two done"}"#,
                "\n",
            ),
        );
        let error = Task::load(temp.path().join("one")).unwrap_err();
        assert!(error.0.contains("unanswered"), "{error}");
    }

    #[test]
    fn an_assistant_reply_before_the_last_step_is_rejected() {
        // A reply ends its turn, so a second reply in the same turn would push
        // the remaining scripts into the next stage.
        let temp = tempfile::tempdir().unwrap();
        task_dir(temp.path(), "one");
        write(
            &temp.path().join("one"),
            "history.jsonl",
            concat!(
                r#"{"type":"user","text":"one"}"#,
                "\n",
                r#"{"type":"tool_call","tool":"read","arguments":{"path":"a"}}"#,
                "\n",
                r#"{"type":"assistant","text":"done"}"#,
                "\n",
                r#"{"type":"tool_call","tool":"read","arguments":{"path":"a"}}"#,
                "\n",
                r#"{"type":"assistant","text":"done"}"#,
                "\n",
                r#"{"type":"user","text":"two"}"#,
                "\n",
                r#"{"type":"assistant","text":"done"}"#,
                "\n",
                r#"{"type":"user","text":"three"}"#,
                "\n",
                r#"{"type":"assistant","text":"done"}"#,
                "\n",
                r#"{"type":"user","text":"four"}"#,
                "\n",
                r#"{"type":"assistant","text":"done"}"#,
                "\n",
            ),
        );
        let error = Task::load(temp.path().join("one")).unwrap_err();
        assert!(error.0.contains("before its last step"), "{error}");
    }

    #[test]
    fn too_short_histories_and_missing_terms_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        task_dir(temp.path(), "one");
        write(
            &temp.path().join("one"),
            "requirements.json",
            r#"{"requirements":[{"id":"R1","text":"t","all_of":[]}]}"#,
        );
        assert!(Task::load(temp.path().join("one")).is_err());
        task_dir(temp.path(), "two");
        write(
            &temp.path().join("two"),
            "history.jsonl",
            concat!(
                r#"{"type":"user","text":"one"}"#,
                "\n",
                r#"{"type":"assistant","text":"done"}"#,
            ),
        );
        let error = Task::load(temp.path().join("two")).unwrap_err();
        assert!(error.0.contains("turns"), "{error}");
    }

    #[test]
    fn a_suite_needs_four_tasks_and_every_shape() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c", "d"] {
            task_dir(temp.path(), name);
        }
        let error = Suite::load(temp.path()).unwrap_err();
        assert!(error.0.contains("shape"), "{error}");
    }

    #[test]
    fn the_check_marker_is_required_and_controls_stay_eligible() {
        let temp = tempfile::tempdir().unwrap();
        task_dir(temp.path(), "one");
        write(&temp.path().join("one"), "check.sh", "exit 0\n");
        let error = Task::load(temp.path().join("one")).unwrap_err();
        assert!(error.0.contains("marker"), "{error}");
        task_dir(temp.path(), "two");
        write(&temp.path().join("two"), "controls.json", r#"{"pin":[4]}"#);
        let error = Task::load(temp.path().join("two")).unwrap_err();
        assert!(error.0.contains("outside"), "{error}");
    }
}
