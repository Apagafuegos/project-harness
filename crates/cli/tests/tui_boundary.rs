//! Architectural ratchet for the client/engine boundary (ADR 0023).
//!
//! This is a conservative source scan, not a Rust security sandbox: it makes
//! drift loud, while the engine's own types and the crate graph remain the real
//! boundary. `tui/` is the pure model (`mod.rs`, `render.rs`, `transcript.rs`,
//! `navigation.rs`, `inspector.rs`) plus the terminal shell (`runtime*.rs`,
//! `inspector_shell.rs`); the process shell is `src/tui_cli.rs`.
//!
//! Allowed in `tui/`: typed engine commands/events, the read-only inspection
//! API and wiring-free rendering. Not allowed: provider adapters or requests,
//! tool authorization/execution/planning types, a non-key approval decision, or
//! a synthesized key that would let a timer approve or send.
use std::path::Path;

/// Engine names that would mean the TUI had taken over an engine job.
const FORBIDDEN: &[&str] = &[
    "harness_providers",
    "ModelRequest",
    "PreparedContext",
    "FsWorkspace",
    "Sandbox",
    "SandboxConfig",
    "ToolHost",
    "ToolHostConfig",
    "Executor",
    "AuthorizedOperation",
    "ExecutionOutcome",
    "ToolSet",
    "PolicySnapshot",
    "Registry",
    "include",
    "include_str",
    "include_bytes",
];

/// True when the line calls the reducer itself, not e.g. `navigation_key(`.
fn calls_key_reducer(line: &str) -> bool {
    line.match_indices("on_key(").any(|(index, _)| {
        line[..index]
            .chars()
            .next_back()
            .is_none_or(|before| !before.is_alphanumeric() && before != '_')
    })
}

fn violations(path: &Path, source: &str) -> Vec<String> {
    let mut failures = Vec::new();
    let file = path.file_name().unwrap().to_str().unwrap();
    // Identifier boundaries only: `InvalidRequest` is displayable text, not a
    // request builder. Comments count too, so no alias or macro is exempt.
    let identifiers: Vec<&str> = source
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .collect();
    for forbidden in FORBIDDEN {
        if identifiers.contains(forbidden) {
            failures.push(format!(
                "{}: forbidden engine identifier {forbidden}",
                path.display()
            ));
        }
    }

    // Approval answers exist only in the modal key handler of `mod.rs`.
    let mut function = "";
    let mut key_calls = 0usize;
    for (line, text) in source.lines().enumerate() {
        let trimmed = text.trim_start();
        if let Some(after) = trimmed
            .strip_prefix("pub(super) fn ")
            .or_else(|| trimmed.strip_prefix("pub fn "))
            .or_else(|| trimmed.strip_prefix("fn "))
        {
            function = after.split('(').next().unwrap_or_default();
        }
        if text.contains("SubmitApproval") && !(file == "mod.rs" && function == "approval_key") {
            failures.push(format!(
                "{}:{}: approval outside the modal key handler",
                path.display(),
                line + 1
            ));
        }
        if text.contains("approval_key(")
            && !(file == "mod.rs" && function == "on_key")
            && !(file == "mod.rs" && trimmed.starts_with("fn approval_key("))
        {
            failures.push(format!(
                "{}:{}: approval decided without a user key",
                path.display(),
                line + 1
            ));
        }
        if calls_key_reducer(text) && !trimmed.contains("fn on_key(") {
            key_calls += 1;
        }
    }

    // Keys enter the reducer only from the terminal event stream and from the
    // Ctrl-C signal translation. A third call is a timer or tick synthesizing
    // input, which could approve or send without the user.
    let expected = if file == "runtime.rs" { 2 } else { 0 };
    if key_calls != expected {
        failures.push(format!(
            "{}: {key_calls} key-reducer calls, expected {expected}",
            path.display()
        ));
    }
    if file == "runtime.rs" {
        for anchor in ["Event::Key(key)", "Signal::Interrupt"] {
            if !source.contains(anchor) {
                failures.push(format!("{}: missing input arm {anchor}", path.display()));
            }
        }
    }
    failures
}

fn scan(directory: &Path, failures: &mut Vec<String>) -> usize {
    let mut files = 0;
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files += scan(&path, failures);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files += 1;
            failures.extend(violations(&path, &std::fs::read_to_string(&path).unwrap()));
        }
    }
    files
}

#[test]
fn tui_never_builds_requests_runs_tools_or_approves_without_a_key() {
    let mut failures = Vec::new();
    let files = scan(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui"),
        &mut failures,
    );
    assert!(
        files >= 8,
        "scanned only {files} TUI files; the guard is misdirected"
    );
    assert!(
        failures.is_empty(),
        "TUI boundary violations:\n{}",
        failures.join("\n")
    );
}

#[test]
fn boundary_guard_rejects_provider_execution_and_timer_approval_mutations() {
    for source in [
        "use harness_providers::openai::OpenAi;",
        "fn request() { let request = ModelRequest { }; }",
        "use harness_core::model::ModelRequest as Request;",
        "fn run(e: &dyn harness_core::tools::Executor) { e.execute(op, cancel); }",
        "fn workspace() { FsWorkspace::open(path); }",
        "fn sandbox() { Sandbox::new(config); }",
        "fn tick() { Command::SubmitApproval { decision: Decision::Allow }; }",
        "fn tick() { self.approval_key(key); }",
        "fn tick() { app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)); }",
    ] {
        assert!(
            !violations(Path::new("mod.rs"), source).is_empty(),
            "guard missed {source}"
        );
    }
    // A timer in the loop is a third key-reducer call.
    let loop_with_timer = "fn drive() {\n app.on_key(a);\n let actions = app.on_key(c);\n _ = tick.tick() => { app.on_key(approve); }\n}\n";
    assert!(!violations(Path::new("runtime.rs"), loop_with_timer).is_empty());

    // The shapes the design does allow stay clean.
    assert!(
        violations(
            Path::new("mod.rs"),
            "pub fn on_key(&mut self, key: KeyEvent) -> Vec<Action> { self.approval_key(key) }\nfn approval_key(&mut self, key: KeyEvent) -> Vec<Action> { Command::SubmitApproval { }; }"
        )
        .is_empty()
    );
    assert!(
        violations(
            Path::new("runtime.rs"),
            "fn drive() {\n Some(Ok(Event::Key(key))) => { app.on_key(key); }\n Some(Signal::Interrupt) => { app.on_key(ctrl_c); }\n}\n"
        )
        .is_empty()
    );
}
