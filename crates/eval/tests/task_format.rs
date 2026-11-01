//! The shape of the checked-in task suite, without running anything.
//!
//! These tests are part of the ordinary workspace test run: they never spawn a
//! sandbox. The pipeline itself is exercised by `offline_gate.rs`, which is
//! ignored unless a Bubblewrap backend is available.

use std::path::{Path, PathBuf};

use harness_eval::{Shape, Suite, Task, normalize};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/tasks")
}

#[test]
fn the_suite_has_four_tasks_covering_every_shape() {
    let suite = Suite::load(root()).unwrap();
    assert!(suite.tasks().len() >= 4);
    let shapes: Vec<Shape> = suite.tasks().iter().map(Task::shape).collect();
    for shape in [
        Shape::Refactor,
        Shape::BugFix,
        Shape::Feature,
        Shape::MultiFile,
    ] {
        assert!(shapes.contains(&shape), "{}", shape.as_str());
    }
    let mut names: Vec<&str> = suite.tasks().iter().map(Task::name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), suite.tasks().len());
}

#[test]
fn every_task_declares_its_history_requirements_and_files() {
    let suite = Suite::load(root()).unwrap();
    for task in suite.tasks() {
        assert!(
            (4..=8).contains(&task.turns().len()),
            "{}: {} turns",
            task.name(),
            task.turns().len()
        );
        let calls: usize = task.turns().iter().map(|turn| turn.tool_calls()).sum();
        assert!(
            calls >= 2,
            "{}: {calls} tool calls, at least 2 are required",
            task.name()
        );
        assert!(
            task.requirements().len() >= 2,
            "{}: {} requirements",
            task.name(),
            task.requirements().len()
        );
        assert!(!task.modified_files().is_empty(), "{}", task.name());
        assert!(!task.continue_prompt().trim().is_empty(), "{}", task.name());
        // At least two requirements are stated in the first turn, so an early
        // statement is what a compaction has to preserve.
        let opening = normalize(&task.turns()[0].prompt);
        let early = task
            .requirements()
            .iter()
            .filter(|requirement| requirement.matcher.matches(&opening))
            .count();
        assert!(
            early >= 2,
            "{}: only {early} requirements are stated in the first turn",
            task.name()
        );
        // Each requirement's own phrasing must satisfy its matcher, so a typo in
        // `all_of` cannot silently make a requirement unscoreable.
        for requirement in task.requirements() {
            assert!(
                requirement.matcher.matches(&normalize(&requirement.text)),
                "{}: requirement {} does not match its own text {:?}",
                task.name(),
                requirement.id,
                requirement.text
            );
        }
        // Every modified path must really exist in the task's workspace.
        for path in task.modified_files() {
            assert!(
                task.workspace().join(path).exists(),
                "{}: modified path {path:?} is not in the workspace",
                task.name()
            );
        }
        // The scripts are present and non-empty.
        assert!(
            !task.scripted().summary.trim().is_empty(),
            "{}",
            task.name()
        );
        assert!(!task.scripted().probe.trim().is_empty(), "{}", task.name());
        assert!(!task.scripted().continuation.is_empty(), "{}", task.name());
        // A model-based summary must satisfy the product's fixed heading schema,
        // or the offline run would fail with `malformed_summary`.
        let headings: Vec<&str> = task
            .scripted()
            .summary
            .lines()
            .filter(|line| line.starts_with("## "))
            .collect();
        assert_eq!(
            headings,
            [
                "## Task and constraints",
                "## Decisions",
                "## Files and actions",
                "## Outcomes",
                "## Unresolved work",
            ],
            "{}",
            task.name()
        );
    }
}

#[test]
fn a_fingerprint_changes_when_any_task_file_changes() {
    let suite = Suite::load(root()).unwrap();
    let task = &suite.tasks()[0];
    let temp = tempfile::tempdir().unwrap();
    let copy = temp.path().join(task.name());
    harness_eval::task::copy_task(task.dir(), &copy).unwrap();
    let original = Task::load(&copy).unwrap();
    assert_eq!(original.fingerprint(), task.fingerprint());
    std::fs::write(copy.join("workspace/.touch"), "changed\n").unwrap();
    let changed = Task::load(&copy).unwrap();
    assert_ne!(changed.fingerprint(), original.fingerprint());
}

#[test]
fn the_probe_prompt_asks_for_requirements_and_files_and_nothing_else() {
    let prompt = normalize(harness_eval::pipeline::PROBE_PROMPT);
    assert!(prompt.contains("requirement"));
    assert!(prompt.contains("file"));
    // The probe must not name a task, a strategy or a check.
    assert!(!prompt.contains("check"));
    assert!(!prompt.contains("hidden"));
}

#[test]
fn matchers_are_literal_substrings_over_normalized_text() {
    let matcher = harness_eval::Matcher::new(
        ["one copy of the row format".to_owned()],
        ["byte-identical".to_owned(), "unchanged".to_owned()],
    )
    .unwrap();
    assert!(matcher.matches(&normalize(
        "only ONE copy  of the row\nformat remains, output unchanged"
    )));
    assert!(matcher.matches(&normalize(
        "... one copy of the row format ... byte-identical"
    )));
    // The all-of term without any alternative still fails.
    assert!(!matcher.matches(&normalize("one copy of the row format")));
    // A pattern-looking term is matched literally, never interpreted.
    let literal = harness_eval::Matcher::new(["a.*b".to_owned()], Vec::new()).unwrap();
    assert!(literal.matches(&normalize("a.*b")));
    assert!(!literal.matches(&normalize("axxb")));
}
