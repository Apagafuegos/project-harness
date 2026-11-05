use std::{cell::Cell, collections::BTreeMap};

use serde_json::{Value, json};

use super::*;
use crate::session::{
    ArtifactRef, CallRef, Digest, EffectClass, ExecutionBackend, ResultCode, ToolStatus,
};

const CONFIG: &[u8] = include_bytes!(
    "../../../../docs/rfcs/001/examples/artifacts/7e66535a506e1c533025006e25e2fced162ef19d026d0db999154218ae7a30ea"
);

// All workspace tests use only these in-memory facts and bytes.
pub(crate) struct FakeWorkspace {
    pub(crate) identity: WorkspaceIdentity,
    entries: BTreeMap<String, (FileFacts, Vec<u8>)>,
    read_text_calls: Cell<u64>,
}
impl FakeWorkspace {
    pub(crate) fn new() -> Self {
        let mut view = Self {
            identity: WorkspaceIdentity("/project".into()),
            entries: BTreeMap::new(),
            read_text_calls: Cell::new(0),
        };
        view.directory(".");
        view.file("a", b"hello world");
        view
    }
    pub(crate) fn directory(&mut self, path: &str) {
        self.entries.insert(
            path.into(),
            (
                FileFacts {
                    kind: FileKind::Directory,
                    size: 0,
                    nlink: 1,
                    digest: None,
                    protection: Protection::None,
                },
                Vec::new(),
            ),
        );
    }
    pub(crate) fn file(&mut self, path: &str, bytes: &[u8]) {
        self.entries.insert(
            path.into(),
            (
                FileFacts {
                    kind: FileKind::Regular,
                    size: bytes.len() as u64,
                    nlink: 1,
                    digest: Some(Digest::of(bytes)),
                    protection: Protection::None,
                },
                bytes.into(),
            ),
        );
    }
    pub(crate) fn facts_mut(&mut self, path: &str) -> &mut FileFacts {
        &mut self.entries.get_mut(path).unwrap().0
    }
}
impl WorkspaceView for FakeWorkspace {
    fn identity(&self) -> &WorkspaceIdentity {
        &self.identity
    }
    fn inspect(&self, path: &WorkspacePath) -> Result<FileFacts, Refusal> {
        Ok(self
            .entries
            .get(path.as_str())
            .map(|(facts, _)| facts.clone())
            .unwrap_or(FileFacts {
                kind: FileKind::Absent,
                size: 0,
                nlink: 0,
                digest: None,
                protection: Protection::None,
            }))
    }
    fn read_text(&self, path: &WorkspacePath, max_bytes: u64) -> Result<String, Refusal> {
        self.read_text_calls.set(self.read_text_calls.get() + 1);
        let (_, bytes) = self
            .entries
            .get(path.as_str())
            .ok_or_else(|| Refusal::new(ResultCode::NotFound, "missing file"))?;
        if bytes.len() as u64 > max_bytes {
            return Err(Refusal::new(ResultCode::TooLarge, "file exceeds limit"));
        }
        String::from_utf8(bytes.clone())
            .map_err(|_| Refusal::new(ResultCode::NotUtf8, "file is not UTF-8"))
    }
}
fn artifact(bytes: &[u8]) -> ArtifactRef {
    ArtifactRef {
        sha256: Digest::of(bytes),
        bytes: bytes.len() as u64,
    }
}
fn policy(value: Value) -> PolicySnapshot {
    let bytes = serde_json::to_vec(&value).unwrap();
    PolicySnapshot::parse(artifact(&bytes), &bytes).unwrap()
}
fn config() -> Value {
    serde_json::from_slice(CONFIG).unwrap()
}
fn default_policy() -> PolicySnapshot {
    policy(config())
}
fn call() -> CallRef {
    CallRef {
        response_entry_id: "019a0000-0000-7000-8000-00000000019a".parse().unwrap(),
        call_id: "019a0000-0000-7000-8000-00000000019b".parse().unwrap(),
    }
}
fn args(name: &str) -> Value {
    match name {
        "read" => json!({"path":"a"}),
        "write" => json!({"path":"new","text":"new text"}),
        "edit" => json!({"path":"a","old_text":"world","new_text":"Rust"}),
        "bash" => json!({"command":"echo hello"}),
        _ => unreachable!(),
    }
}
fn validate(name: &str, value: &Value) -> Result<ValidatedOperation, Refusal> {
    Registry::builtin()
        .resolve(name, &default_policy())
        .unwrap()
        .validate(value)
}
fn plan(
    name: &str,
    value: Value,
    view: &FakeWorkspace,
    policy: &PolicySnapshot,
) -> Result<Plan, Refusal> {
    let registry = Registry::builtin();
    let tool = registry.resolve(name, policy)?;
    let before = view.read_text_calls.get();
    let result = tool.plan(&tool.validate(&value)?, &call(), view, policy);
    if name == "read" {
        assert_eq!(
            view.read_text_calls.get(),
            before,
            "read planning must inspect facts only"
        );
    }
    result
}
fn refusal<T: std::fmt::Debug>(result: Result<T, Refusal>, code: ResultCode, status: ToolStatus) {
    let error = result.unwrap_err();
    assert_eq!(error.code, code);
    assert_eq!(error.status, status);
    assert!(error.text().starts_with(&format!("{}: ", code.as_str())));
    assert!(error.detail.len() <= 200);
}
fn invalid<T: std::fmt::Debug>(result: Result<T, Refusal>) {
    refusal(
        result,
        ResultCode::InvalidArguments,
        ToolStatus::InvalidArguments,
    );
}
fn invalid_at(result: Result<ValidatedOperation, Refusal>, field: &str) -> Refusal {
    let error = result.unwrap_err();
    assert_eq!(error.code, ResultCode::InvalidArguments);
    assert_eq!(error.status, ToolStatus::InvalidArguments);
    assert!(error.detail.len() <= 200);
    assert!(error.detail.contains(field), "{field}: {}", error.detail);
    error
}

fn outcome(auth: Authorization) -> u8 {
    match auth {
        Authorization::Deny(_) => 0,
        Authorization::Ask(_) => 1,
        Authorization::Allow(_) => 2,
    }
}

#[test]
fn registry_unknown_tool_tool_disabled_and_reserved_names() {
    let registry = Registry::builtin();
    let mut c = config();
    c["enabled_tools"] = json!([]);
    let disabled = policy(c);
    for (name, code) in [
        ("unknown", ResultCode::UnknownTool),
        ("read", ResultCode::ToolDisabled),
    ] {
        let error = registry.resolve(name, &disabled).err().unwrap();
        assert_eq!(error.code, code);
        assert_eq!(error.status, ToolStatus::InvalidArguments);
    }
    assert_eq!(
        registry
            .definitions()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        ["read", "write", "edit", "bash", "list", "glob", "grep"]
    );
    for name in ["read", "write", "edit", "bash"] {
        let policy = default_policy();
        let tool = registry.resolve(name, &policy).unwrap();
        assert_eq!(tool.definition().name, name);
        assert_eq!(
            tool.definition().input_schema["additionalProperties"],
            false
        );
        assert!(!tool.definition().description.is_empty());
        // No replacement API exists; repeated construction keeps the reserved definitions.
        assert_eq!(
            Registry::builtin()
                .resolve(name, &policy)
                .unwrap()
                .definition()
                .input_schema,
            tool.definition().input_schema
        );
    }
}

#[test]
fn schemas_invalid_arguments_required_unknown_wrong_types() {
    for name in ["read", "write", "edit", "bash"] {
        let original = args(name);
        let required = Registry::builtin()
            .resolve(name, &default_policy())
            .unwrap()
            .definition()
            .input_schema["required"]
            .as_array()
            .unwrap()
            .clone();
        for field in required {
            let mut value = original.clone();
            value
                .as_object_mut()
                .unwrap()
                .remove(field.as_str().unwrap());
            assert!(
                invalid_at(validate(name, &value), field.as_str().unwrap())
                    .detail
                    .contains("required")
            );
        }
        let mut extra = original.clone();
        extra["elevated"] = json!(true);
        assert!(
            invalid_at(validate(name, &extra), "elevated")
                .detail
                .contains("unknown key")
        );
        for key in original.as_object().unwrap().keys() {
            let mut value = original.clone();
            value[key] = json!(42);
            assert!(
                invalid_at(validate(name, &value), key)
                    .detail
                    .contains("type")
            );
        }
        for bad in [Value::Null, json!([]), json!("text")] {
            assert!(
                invalid_at(validate(name, &bad), "\"\"")
                    .detail
                    .contains("type")
            );
        }
    }
    for (name, key, bad) in [
        ("read", "offset_bytes", json!(-1)),
        ("read", "max_bytes", json!("1")),
        ("read", "offset_bytes", json!(1.5)),
        ("write", "overwrite", json!(1)),
        ("bash", "cwd", json!(1)),
        ("bash", "timeout_ms", json!("1")),
    ] {
        let mut a = args(name);
        a[key] = bad;
        invalid_at(validate(name, &a), key);
    }
    invalid_at(
        validate("edit", &json!({"path":"a","old_text":"","new_text":"x"})),
        "old_text",
    );
    invalid_at(validate("bash", &json!({"command":""})), "command");
}

#[test]
fn schemas_invalid_arguments_byte_boundaries_and_defaults() {
    fn boundary(name: &str, value: Value, field: &str, valid: bool) {
        if valid {
            assert!(validate(name, &value).is_ok());
        } else {
            invalid_at(validate(name, &value), field);
        }
    }
    let mib = 1024 * 1024;
    for length in [mib, mib + 1] {
        let v = json!({"path":"new","text":"x".repeat(length)});
        boundary("write", v, "text", length == mib);
        let v = json!({"path":"a","max_bytes":length});
        boundary("read", v, "max_bytes", length == mib);
    }
    // JSON Schema length counts characters; semantic validation counts UTF-8 bytes.
    assert!(validate("write", &json!({"path":"a","text":"é".repeat(mib / 2)})).is_ok());
    invalid_at(
        validate("write", &json!({"path":"a","text":"é".repeat(mib / 2 + 1)})),
        "text",
    );
    for length in [65536, 65537] {
        boundary(
            "bash",
            json!({"command":"x".repeat(length)}),
            "command",
            length == 65536,
        );
    }
    invalid_at(
        validate("bash", &json!({"command":"é".repeat(32769)})),
        "command",
    );
    for timeout in [0, 1, 600000, 600001] {
        boundary(
            "bash",
            json!({"command":"x","timeout_ms":timeout}),
            "timeout_ms",
            (1..=600000).contains(&timeout),
        );
    }
    let view = FakeWorkspace::new();
    let p = default_policy();
    for name in ["read", "write", "edit", "bash"] {
        let built = plan(name, args(name), &view, &p).unwrap();
        let wire: Value = serde_json::from_slice(built.bytes()).unwrap();
        match name {
            "read" => {
                assert_eq!(wire["arguments"]["offset_bytes"], 0);
                assert_eq!(wire["arguments"]["max_bytes"], mib);
            }
            "write" => assert_eq!(wire["arguments"]["overwrite"], false),
            "bash" => {
                assert_eq!(wire["arguments"]["cwd"], ".");
                assert_eq!(wire["arguments"]["timeout_ms"], 120000);
            }
            _ => {}
        }
        assert_eq!(
            wire["timeout_ms"],
            if name == "bash" { 120000 } else { 30000 }
        );
    }
    assert!(
        validate(
            "read",
            &json!({"path":"a","offset_bytes":u64::MAX,"max_bytes":0})
        )
        .is_ok()
    );
}

#[test]
fn schema_refusals_name_fields_rules_and_never_echo_values() {
    for (name, value, field, rule) in [
        (
            "read",
            json!({"path":"a","offset_bytes":-1}),
            "/offset_bytes",
            "minimum",
        ),
        (
            "bash",
            json!({"command":"x","timeout_ms":600001}),
            "/timeout_ms",
            "maximum",
        ),
        (
            "edit",
            json!({"path":"a","old_text":"","new_text":"x"}),
            "/old_text",
            "length",
        ),
        (
            "write",
            json!({"path":"a","text":"x".repeat(1048577)}),
            "/text",
            "1048576 UTF-8 bytes",
        ),
        (
            "write",
            json!({"path":"a","text":"é".repeat(524289)}),
            "/text",
            "1048576 UTF-8 bytes",
        ),
        (
            "bash",
            json!({"command":"é".repeat(32769)}),
            "/command",
            "65536 UTF-8 bytes",
        ),
        (
            "read",
            json!({"path":"../TOP_SECRET_VALUE"}),
            "/path",
            "workspace-relative",
        ),
        (
            "bash",
            json!({"command":"x","cwd":"../TOP_SECRET_VALUE"}),
            "/cwd",
            "workspace-relative",
        ),
        (
            "bash",
            json!({"command":"x","timeout_ms":"TOP_SECRET_VALUE"}),
            "/timeout_ms",
            "type",
        ),
    ] {
        let error = invalid_at(validate(name, &value), field);
        assert!(error.detail.contains(rule), "{}", error.detail);
        assert!(!error.detail.contains("TOP_SECRET_VALUE"));
    }
    let error = invalid_at(
        validate("read", &json!({"path":"a","a/b~c":true})),
        "/a~1b~0c",
    );
    assert!(error.detail.contains("unknown key"));
    let mut value = args("read");
    value["é".repeat(1000)] = json!("TOP_SECRET_VALUE");
    let error = validate("read", &value).unwrap_err();
    assert!(error.detail.len() <= 200);
    assert!(error.detail.ends_with("unknown key"));
    assert!(!error.detail.contains("TOP_SECRET_VALUE"));
}

#[test]
fn workspace_path_invalid_arguments_and_normalization() {
    for input in [
        "/etc/passwd",
        "a/../b",
        "..",
        "a\0b",
        "",
        "//root",
        "./../",
        "/",
    ] {
        invalid(WorkspacePath::new(input));
    }
    invalid(WorkspacePath::new(&"x".repeat(4097)));
    invalid(WorkspacePath::new(&"é".repeat(2049)));
    for (input, expected) in [
        (".", "."),
        ("./a//b/", "a/b"),
        ("./a//b", "a/b"),
        ("a", "a"),
        ("./", "."),
        ("...", "..."),
        ("a/..b", "a/..b"),
    ] {
        assert_eq!(WorkspacePath::new(input).unwrap().as_str(), expected);
    }
    assert!(WorkspacePath::new(&"x".repeat(4096)).is_ok());
    for name in ["read", "write", "edit", "bash"] {
        let mut a = args(name);
        let field = if name == "bash" { "cwd" } else { "path" };
        a[field] = json!("a/../b");
        invalid_at(validate(name, &a), field);
    }
}

#[test]
fn omitted_or_empty_sandbox_keeps_the_v01_plan_bytes() {
    let view = FakeWorkspace::new();
    let omitted = plan("bash", args("bash"), &view, &default_policy()).unwrap();
    assert_eq!(
        omitted.bytes(),
        include_str!("golden/bash.json").trim_end().as_bytes()
    );
    let mut empty = config();
    empty["sandbox"] = json!({});
    let empty = plan("bash", args("bash"), &view, &policy(empty)).unwrap();
    assert_eq!(empty.bytes(), omitted.bytes());
    assert_eq!(empty.digest(), omitted.digest());
    let mut tools = config();
    tools["sandbox"] = json!({"toolchain_paths":["/opt/rust"],"path_prepend":["/opt/rust/bin"],"env":{"RUSTUP_TOOLCHAIN":"1.99.1"}});
    let configured = plan("bash", args("bash"), &view, &policy(tools)).unwrap();
    assert_ne!(configured.bytes(), omitted.bytes());
    assert!(
        serde_json::from_slice::<Value>(configured.bytes()).unwrap()["sandbox"]["toolchain_paths"]
            == json!(["/opt/rust"])
    );
    // Only bash carries sandbox scope; trusted file plans stay byte-identical.
    for name in ["read", "write", "edit"] {
        let mut configured = config();
        configured["sandbox"] = json!({"toolchain_paths":["/opt/rust"]});
        assert_eq!(
            plan(name, args(name), &view, &policy(configured))
                .unwrap()
                .bytes(),
            plan(name, args(name), &view, &default_policy())
                .unwrap()
                .bytes()
        );
    }
}

#[test]
fn plans_golden_canonical_bytes_determinism_and_digest() {
    let view = FakeWorkspace::new();
    let p = default_policy();
    for (name, golden) in [
        ("read", include_str!("golden/read.json")),
        ("write", include_str!("golden/write.json")),
        ("edit", include_str!("golden/edit.json")),
        ("bash", include_str!("golden/bash.json")),
    ] {
        let a = plan(name, args(name), &view, &p).unwrap();
        let b = plan(name, args(name), &view, &p).unwrap();
        assert_eq!(a.bytes(), golden.trim_end().as_bytes(), "{name}");
        assert_eq!(a.bytes(), b.bytes());
        assert_eq!(a.digest(), b.digest());
        assert_eq!(*a.digest(), Digest::of(a.bytes()));
    }
    let original = plan("read", args("read"), &view, &p).unwrap();
    let normalized = plan(
        "read",
        json!({"path":"./a/","offset_bytes":0,"max_bytes":1048576}),
        &view,
        &p,
    )
    .unwrap();
    assert_eq!(original.bytes(), normalized.bytes());
}

#[test]
fn planning_not_found_not_regular_file_hard_link_rejected_already_exists() {
    let mut view = FakeWorkspace::new();
    let p = default_policy();
    for name in ["read", "edit"] {
        let mut a = args(name);
        a["path"] = json!("missing");
        refusal(
            plan(name, a, &view, &p),
            ResultCode::NotFound,
            ToolStatus::InvalidArguments,
        );
    }
    refusal(
        plan("write", json!({"path":"missing/new","text":"x"}), &view, &p),
        ResultCode::NotFound,
        ToolStatus::InvalidArguments,
    );
    view.directory("dir");
    for kind in [FileKind::Directory, FileKind::Other] {
        view.facts_mut("a").kind = kind;
        for name in ["read", "write", "edit"] {
            let mut a = args(name);
            a["path"] = json!("a");
            refusal(
                plan(name, a, &view, &p),
                ResultCode::NotRegularFile,
                ToolStatus::InvalidArguments,
            );
        }
    }
    view.facts_mut("a").kind = FileKind::Regular;
    view.facts_mut("a").nlink = 2;
    for name in ["read", "write", "edit"] {
        let mut a = args(name);
        a["path"] = json!("a");
        refusal(
            plan(name, a, &view, &p),
            ResultCode::HardLinkRejected,
            ToolStatus::InvalidArguments,
        );
    }
    view.facts_mut("a").nlink = 1;
    refusal(
        plan("write", json!({"path":"a","text":"x"}), &view, &p),
        ResultCode::AlreadyExists,
        ToolStatus::InvalidArguments,
    );
    assert!(
        plan(
            "write",
            json!({"path":"a","text":"x","overwrite":true}),
            &view,
            &p
        )
        .is_ok()
    );
    refusal(
        plan("bash", json!({"command":"x","cwd":"missing"}), &view, &p),
        ResultCode::NotFound,
        ToolStatus::InvalidArguments,
    );
}

#[test]
fn planning_no_match_multiple_matches_not_utf8_too_large() {
    let mut view = FakeWorkspace::new();
    let p = default_policy();
    refusal(
        plan(
            "edit",
            json!({"path":"a","old_text":"missing","new_text":"x"}),
            &view,
            &p,
        ),
        ResultCode::NoMatch,
        ToolStatus::InvalidArguments,
    );
    refusal(
        plan(
            "edit",
            json!({"path":"a","old_text":"l","new_text":"x"}),
            &view,
            &p,
        ),
        ResultCode::MultipleMatches,
        ToolStatus::InvalidArguments,
    );
    view.file("a", &[0xff]);
    refusal(
        plan("edit", args("edit"), &view, &p),
        ResultCode::NotUtf8,
        ToolStatus::InvalidArguments,
    );
    view.file("a", &vec![b'x'; 1048577]);
    refusal(
        plan("edit", args("edit"), &view, &p),
        ResultCode::TooLarge,
        ToolStatus::InvalidArguments,
    );
    view.file("a", &vec![b'x'; 1048576]);
    assert!(plan("read", args("read"), &view, &p).is_ok());
    refusal(
        plan(
            "edit",
            json!({"path":"a","old_text":"x".repeat(1048576),"new_text":"y".repeat(1048577)}),
            &view,
            &p,
        ),
        ResultCode::TooLarge,
        ToolStatus::InvalidArguments,
    );
}

#[test]
fn read_large_ranges_plan_from_inspection_without_reading_text() {
    let mut view = FakeWorkspace::new();
    let p = default_policy();
    view.file("large", &vec![b'x'; 5 * 1048576]);
    assert!(
        plan(
            "read",
            json!({"path":"large","offset_bytes":4 * 1048576,"max_bytes":1048576}),
            &view,
            &p
        )
        .is_ok()
    );
    assert!(plan("read", args("read"), &view, &p).is_ok());
    view.file("binary", &[0xff]);
    assert!(plan("read", json!({"path":"binary"}), &view, &p).is_ok());
    assert_eq!(view.read_text_calls.get(), 0);
}

#[test]
fn edit_overlapping_occurrences_return_multiple_matches() {
    let mut view = FakeWorkspace::new();
    let p = default_policy();
    for (text, old) in [("aaa", "aa"), ("abab", "ab"), ("ééé", "éé")] {
        view.file("a", text.as_bytes());
        refusal(
            plan(
                "edit",
                json!({"path":"a","old_text":old,"new_text":"x"}),
                &view,
                &p,
            ),
            ResultCode::MultipleMatches,
            ToolStatus::InvalidArguments,
        );
    }
    view.file("a", b"abc");
    assert!(
        plan(
            "edit",
            json!({"path":"a","old_text":"b","new_text":"x"}),
            &view,
            &p
        )
        .is_ok()
    );
}

#[test]
fn missing_workspace_digest_is_infrastructure_failure_not_invalid_arguments() {
    let mut view = FakeWorkspace::new();
    view.facts_mut("a").digest = None;
    for name in ["read", "write", "edit"] {
        let mut a = args(name);
        a["path"] = json!("a");
        if name == "write" {
            a["overwrite"] = json!(true);
        }
        refusal(
            plan(name, a, &view, &default_policy()),
            ResultCode::InfrastructureFailure,
            ToolStatus::InfrastructureFailure,
        );
    }
}

#[test]
fn bash_protected_mounts_sorted_deduplicated_and_bound_to_golden_bytes() {
    let mut view = FakeWorkspace::new();
    view.directory(".git");
    let mut c = config();
    c["protected_paths"] = json!(["z/absent", "./.git/", "private", "a/b", ".git", "a/b"]);
    let built = plan("bash", args("bash"), &view, &policy(c.clone())).unwrap();
    assert_eq!(
        built.bytes(),
        include_str!("golden/bash-protected.json")
            .trim_end()
            .as_bytes()
    );
    let wire: Value = serde_json::from_slice(built.bytes()).unwrap();
    assert_eq!(
        wire["mounts"],
        json!([
            {"path":".","mode":"rw"}, {"path":".git","mode":"masked"},
            {"path":"a/b","mode":"masked"}, {"path":"private","mode":"masked"}, {"path":"z/absent","mode":"masked"}
        ])
    );
    assert!(
        !wire["mounts"]
            .as_array()
            .unwrap()
            .contains(&json!({"path":".git","mode":"ro"}))
    );
    c["protected_paths"] = json!(["private", ".git", "a/b", "z/absent"]);
    assert_eq!(
        built.bytes(),
        plan("bash", args("bash"), &view, &policy(c.clone()))
            .unwrap()
            .bytes()
    );
    c["protected_paths"] = json!([]);
    let unmasked = plan("bash", args("bash"), &view, &policy(c.clone())).unwrap();
    let wire: Value = serde_json::from_slice(unmasked.bytes()).unwrap();
    assert_eq!(
        wire["mounts"],
        json!([{"path":".","mode":"rw"},{"path":".git","mode":"ro"}])
    );
    let absent = plan("bash", args("bash"), &FakeWorkspace::new(), &policy(c)).unwrap();
    let wire: Value = serde_json::from_slice(absent.bytes()).unwrap();
    assert_eq!(wire["mounts"], json!([{"path":".","mode":"rw"}]));
    assert_eq!(*built.digest(), Digest::of(built.bytes()));
}

#[test]
fn plans_bash_mounts_network_deny_and_file_preimages() {
    let mut view = FakeWorkspace::new();
    view.directory(".git");
    for setting in ["ask", "deny"] {
        let mut c = config();
        c["workspace_mutation"] = json!(setting);
        let p = policy(c);
        let built = plan("bash", args("bash"), &view, &p).unwrap();
        let wire: Value = serde_json::from_slice(built.bytes()).unwrap();
        assert_eq!(
            wire["mounts"],
            json!([{"path":".","mode":if setting=="ask" {"rw"} else {"ro"}},{"path":".git","mode":"masked"}])
        );
        assert_eq!(wire["target"], Value::Null);
        assert_eq!(wire["network"], "deny");
    }
    let p = default_policy();
    let built = plan("write", args("write"), &view, &p).unwrap();
    let wire: Value = serde_json::from_slice(built.bytes()).unwrap();
    assert_eq!(wire["target"]["pre_image"], "absent");
    assert_eq!(
        wire["target"]["content_sha256"],
        Digest::of(b"new text").as_str()
    );
    let built = plan(
        "write",
        json!({"path":"a","text":"x","overwrite":true}),
        &view,
        &p,
    )
    .unwrap();
    let wire: Value = serde_json::from_slice(built.bytes()).unwrap();
    assert_eq!(
        wire["target"]["pre_image"],
        Digest::of(b"hello world").as_str()
    );
}

#[test]
fn policy_decide_every_row_and_approval_binding() {
    let view = FakeWorkspace::new();
    for (name, field, settings) in [
        ("read", "workspace_read", vec!["allow", "ask", "deny"]),
        ("write", "workspace_mutation", vec!["ask", "deny"]),
        ("edit", "workspace_mutation", vec!["ask", "deny"]),
        ("bash", "process", vec!["ask", "deny"]),
    ] {
        for setting in settings {
            for remembered in [false, true] {
                let mut c = config();
                c[field] = json!(setting);
                if remembered {
                    c["allowances"] = json!([match name {
                        "read" => json!({"tool":"write","path":"."}),
                        "write" => json!({"tool":"write","path":"new"}),
                        "edit" => json!({"tool":"edit","path":"a"}),
                        _ => json!({"tool":"bash","command":"echo hello","cwd":"."}),
                    }]);
                }
                let p = policy(c);
                let built = plan(name, args(name), &view, &p).unwrap();
                let auth = decide(&built, built.facts(), &p);
                let expected = match setting {
                    "deny" => 0,
                    "allow" => 2,
                    _ if remembered && name != "read" => 2,
                    _ => 1,
                };
                match &auth {
                    Authorization::Deny(r) => assert_eq!(r.code, ResultCode::PolicyDenied),
                    Authorization::Ask(request) => {
                        assert_eq!(request.call, call());
                        assert_eq!(request.plan_digest, *built.digest());
                        assert_eq!(request.plan_bytes, built.bytes());
                        assert_eq!(request.policy_revision, p.revision());
                        assert_eq!(Digest::of(&request.plan_bytes), request.plan_digest);
                    }
                    Authorization::Allow(allowed) => assert_eq!(allowed.bytes(), built.bytes()),
                }
                assert_eq!(outcome(auth), expected, "{name} {setting} {remembered}");
            }
        }
    }
}

#[test]
fn protected_path_denied_and_read_only_win_over_all_policy_and_allowances() {
    let mut view = FakeWorkspace::new();
    let p = default_policy();
    for name in ["read", "write", "edit", "bash"] {
        let a = args(name);
        view.facts_mut(if name == "bash" { "." } else { "a" })
            .protection = Protection::Denied;
        let mut a = a;
        if name != "bash" {
            a["path"] = json!("a");
        }
        refusal(
            plan(name, a, &view, &p),
            ResultCode::ProtectedPath,
            ToolStatus::Denied,
        );
        view.facts_mut(if name == "bash" { "." } else { "a" })
            .protection = Protection::None;
    }
    for protection in [Protection::Denied, Protection::ReadOnly] {
        view.facts_mut("a").protection = protection;
        for name in ["write", "edit"] {
            let mut c = config();
            c["allowances"] = json!([{"tool":name,"path":"."}]);
            for setting in ["ask", "deny"] {
                c["workspace_mutation"] = json!(setting);
                let p = policy(c.clone());
                let mut a = args(name);
                a["path"] = json!("a");
                a.as_object_mut().unwrap().remove("overwrite");
                if name == "write" {
                    a["overwrite"] = json!(true);
                }
                refusal(
                    plan(name, a, &view, &p),
                    ResultCode::ProtectedPath,
                    ToolStatus::Denied,
                );
            }
        }
    }
    // Exercise decide's protection rows independently with bound private fixtures.
    let view = FakeWorkspace::new();
    for name in ["read", "write", "edit", "bash"] {
        let mut built = plan(name, args(name), &view, &p).unwrap();
        built.set_test_protection(Protection::Denied, Protection::None);
        assert!(matches!(
            decide(&built, built.facts(), &p),
            Authorization::Deny(Refusal {
                code: ResultCode::ProtectedPath,
                ..
            })
        ));
        built.set_test_protection(Protection::None, Protection::Denied);
        assert_eq!(outcome(decide(&built, built.facts(), &p)), 0);
        if name == "write" || name == "edit" {
            built.set_test_protection(Protection::ReadOnly, Protection::None);
            assert!(matches!(
                decide(&built, built.facts(), &p),
                Authorization::Deny(Refusal {
                    code: ResultCode::ProtectedPath,
                    ..
                })
            ));
        }
    }
}

#[test]
fn git_read_only_versus_configured_protected_path() {
    let mut view = FakeWorkspace::new();
    view.directory(".git");
    view.file(".git/config", b"world");
    let mut c = config();
    c["protected_paths"] = json!([]);
    c["allowances"] = json!([{"tool":"write","path":"."},{"tool":"edit","path":"."}]);
    let p = policy(c.clone());
    assert!(plan("read", json!({"path":".git/config"}), &view, &p).is_ok());
    for name in ["write", "edit"] {
        let mut a = args(name);
        a["path"] = json!(".git/config");
        refusal(
            plan(name, a, &view, &p),
            ResultCode::ProtectedPath,
            ToolStatus::Denied,
        );
    }
    c["protected_paths"] = json!([".git"]);
    let p = policy(c);
    refusal(
        plan("read", json!({"path":".git/config"}), &view, &p),
        ResultCode::ProtectedPath,
        ToolStatus::Denied,
    );
    refusal(
        plan("bash", json!({"command":"x","cwd":".git"}), &view, &p),
        ResultCode::ProtectedPath,
        ToolStatus::Denied,
    );
}

#[test]
fn allowances_component_patterns_and_bash_exact_not_prefix() {
    let mut view = FakeWorkspace::new();
    for dir in ["src", "src/a", "src2", "other"] {
        view.directory(dir);
    }
    for (pattern, expected) in [
        ("src/", [true, true, false, false]),
        (".", [true, true, true, true]),
        ("src/a", [true, false, false, false]),
    ] {
        let allowance = Allowance::parse(&json!({"tool":"write","path":pattern})).unwrap();
        for (path, expected) in ["src/a", "src/a/b", "src", "src2/a"]
            .into_iter()
            .zip(expected)
        {
            let mut target_view = FakeWorkspace::new();
            target_view.directory("src");
            target_view.directory("src/a");
            target_view.directory("src2");
            target_view.file(path, b"x");
            let built = plan(
                "write",
                json!({"path":path,"text":"x","overwrite":true}),
                &target_view,
                &default_policy(),
            )
            .unwrap();
            assert_eq!(allowance.matches(&built), expected, "{pattern} {path}");
        }
    }
    let allowance =
        Allowance::parse(&json!({"tool":"bash","command":"echo hello","cwd":"."})).unwrap();
    let p = default_policy();
    for (command, cwd, expected) in [
        ("echo hello", ".", true),
        ("echo hello; evil", ".", false),
        ("echo", ".", false),
        ("echo hello ", ".", false),
        ("echo hello", "other", false),
    ] {
        let built = plan("bash", json!({"command":command,"cwd":cwd}), &view, &p).unwrap();
        assert_eq!(allowance.matches(&built), expected);
    }
    for bad in [
        json!({"tool":"read","path":"."}),
        json!({"tool":"write","path":"../"}),
        json!({"tool":"bash","command":"x"}),
        json!({"tool":"bash","command":"x","cwd":".","prefix":true}),
    ] {
        invalid(Allowance::parse(&bad));
    }
}

#[test]
fn allowances_remember_grant_revoke_preserve_unknown_fields_and_existing_values() {
    let view = FakeWorkspace::new();
    let mut original = config();
    original["unknown"] = json!({"nested":[1,null,{"secret_policy":true}],"number":12.75});
    original["allowances"] =
        json!([{"tool":"write","path":"./old//"},{"tool":"edit","path":"existing"}]);
    let p = policy(original.clone());
    for name in ["write", "edit", "bash"] {
        let built = plan(name, args(name), &view, &p).unwrap();
        let allowance = Allowance::remember(&built).unwrap();
        assert!(allowance.matches(&built));
        let bytes = p.grant(allowance.clone(), "next");
        let next = PolicySnapshot::parse(artifact(&bytes), &bytes).unwrap();
        assert_eq!(bytes, next.grant(allowance.clone(), "next"), "deduplicate");
        let mut next_value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            next_value["allowances"].as_array().unwrap()[..2],
            original["allowances"].as_array().unwrap()[..]
        );
        assert_eq!(next_value["policy_revision"], "next");
        next_value["allowances"] = original["allowances"].clone();
        next_value["policy_revision"] = original["policy_revision"].clone();
        assert_eq!(
            next_value, original,
            "all other known and unknown values must survive"
        );
        let restored: Value = serde_json::from_slice(&next.revoke(&allowance, "initial")).unwrap();
        assert_eq!(restored, original);
        let authorized = plan(name, args(name), &view, &next).unwrap();
        assert_eq!(outcome(decide(&authorized, authorized.facts(), &next)), 2);
        assert_eq!(
            outcome(decide(&built, built.facts(), &next)),
            0,
            "new revision invalidates old plans"
        );
    }
    assert!(Allowance::remember(&plan("read", args("read"), &view, &p).unwrap()).is_none());
    let same = Allowance::parse(&json!({"tool":"write","path":"old/"})).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&p.grant(same, "next")).unwrap()["allowances"],
        original["allowances"]
    );
}

#[test]
fn configuration_rfc_example_errors_and_network_unsupported() {
    fn check(result: Result<PolicySnapshot, PolicyError>, expected: PolicyError, code: &str) {
        let error = result.unwrap_err();
        assert_eq!(error, expected);
        assert_eq!(error.code(), code);
        assert!(error.to_string().starts_with(code));
        match error {
            PolicyError::MissingField(field) | PolicyError::InvalidField(field) => {
                assert!(error.to_string().contains(field))
            }
            PolicyError::InvalidAllowance { index } => {
                assert!(error.to_string().contains(&index.to_string()))
            }
            _ => {}
        }
    }
    assert!(PolicySnapshot::parse(artifact(CONFIG), CONFIG).is_ok());
    for field in [
        "policy_revision",
        "enabled_tools",
        "workspace_read",
        "workspace_mutation",
        "process",
        "network",
    ] {
        let mut c = config();
        c.as_object_mut().unwrap().remove(field);
        let bytes = serde_json::to_vec(&c).unwrap();
        check(
            PolicySnapshot::parse(artifact(&bytes), &bytes),
            PolicyError::MissingField(field),
            "policy_missing_field",
        );
        let mut c = config();
        c[field] = json!(27);
        let bytes = serde_json::to_vec(&c).unwrap();
        let (expected, code) = if field == "network" {
            (PolicyError::NetworkUnsupported, "network_unsupported")
        } else {
            (PolicyError::InvalidField(field), "policy_invalid_field")
        };
        check(
            PolicySnapshot::parse(artifact(&bytes), &bytes),
            expected,
            code,
        );
    }
    for (field, bad) in [
        ("workspace_read", json!("always")),
        ("workspace_mutation", json!("allow")),
        ("process", json!("allow")),
        ("enabled_tools", json!([1])),
        ("protected_paths", json!(["../secret"])),
        ("protected_paths", json!(null)),
        ("protected_paths", json!([1])),
        ("allowances", json!({})),
    ] {
        let mut c = config();
        c[field] = bad;
        let bytes = serde_json::to_vec(&c).unwrap();
        check(
            PolicySnapshot::parse(artifact(&bytes), &bytes),
            PolicyError::InvalidField(field),
            "policy_invalid_field",
        );
    }
    for bad in [
        json!({"tool":"bad"}),
        json!({"tool":"write","path":"../"}),
        json!({"tool":"bash","command":"","cwd":"."}),
    ] {
        let mut c = config();
        c["allowances"] = json!([{"tool":"write","path":"a"}, bad]);
        let bytes = serde_json::to_vec(&c).unwrap();
        check(
            PolicySnapshot::parse(artifact(&bytes), &bytes),
            PolicyError::InvalidAllowance { index: 1 },
            "policy_invalid_allowance",
        );
    }
    for network in [json!("allow"), json!("ask"), json!(null), json!(false)] {
        let mut c = config();
        c["network"] = network;
        let bytes = serde_json::to_vec(&c).unwrap();
        check(
            PolicySnapshot::parse(artifact(&bytes), &bytes),
            PolicyError::NetworkUnsupported,
            "network_unsupported",
        );
    }
    for bytes in [
        b"[]".as_slice(),
        b"{\"network\":\"deny\",\"network\":\"deny\"}",
        b"invalid",
    ] {
        check(
            PolicySnapshot::parse(artifact(bytes), bytes),
            PolicyError::NotAnObject,
            "policy_not_an_object",
        );
    }
    let mut mismatched = artifact(CONFIG);
    mismatched.sha256 = Digest::of(b"other");
    check(
        PolicySnapshot::parse(mismatched, CONFIG),
        PolicyError::Mismatch,
        "policy_mismatch",
    );
    let mut mismatched = artifact(CONFIG);
    mismatched.bytes += 1;
    check(
        PolicySnapshot::parse(mismatched, CONFIG),
        PolicyError::Mismatch,
        "policy_mismatch",
    );
    let mut c = config();
    c.as_object_mut().unwrap().remove("protected_paths");
    let p = policy(c);
    assert!(p.allowances().is_empty());
}

struct SplitMix64(u64);
impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut x = self.0;
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
        x ^ (x >> 31)
    }
}
#[test]
fn seeded_no_widening_arguments_paths_commands_and_allowances() {
    let mut random = SplitMix64(0x4d335f706f6c6963);
    let mut view = FakeWorkspace::new();
    view.directory("src");
    view.directory("src2");
    view.directory(".git");
    view.file(".git/a", b"world");
    for iteration in 0..512 {
        let name = ["read", "write", "edit", "bash"][(random.next() % 4) as usize];
        let path = ["a", "src/a", "src2/a", "./src//a", ".git/a", "../escape"]
            [(random.next() % 6) as usize];
        let command = format!(
            "echo {}{}",
            random.next() % 7,
            if random.next().is_multiple_of(2) {
                "; extra"
            } else {
                ""
            }
        );
        let cwd = [".", "src", "src2", "./src/", ".git", "../escape"][(random.next() % 6) as usize];
        let mut a = args(name);
        if name == "bash" {
            a["command"] = json!(command);
            a["cwd"] = json!(cwd);
            a["timeout_ms"] = json!(1 + random.next() % 600000);
        } else {
            a["path"] = json!(path);
        }
        if name == "read" {
            a["max_bytes"] = json!(random.next() % 1048577);
        }
        if name == "write" {
            a["overwrite"] = json!(true);
            a["text"] = json!(format!("{}", random.next()));
        }
        if name == "edit" {
            a["old_text"] = json!("world");
            a["new_text"] = json!(format!("{}", random.next()));
        }
        if let Ok(path) = WorkspacePath::new(path)
            && path.as_str() != ".git/a"
        {
            view.file(path.as_str(), b"world");
        }
        for denied in [true, false] {
            let mut c = config();
            c["workspace_read"] = json!(if denied { "deny" } else { "ask" });
            c["workspace_mutation"] = json!(if denied { "deny" } else { "ask" });
            c["process"] = json!(if denied { "deny" } else { "ask" });
            let matching = random.next().is_multiple_of(2);
            let allowance_cwd = if random.next().is_multiple_of(2) {
                "."
            } else {
                "src"
            };
            c["allowances"] = json!([
                {"tool":"write","path":if matching {"src/"} else {"other/"}},
                {"tool":"edit","path":if matching {"a"} else {"other"}},
                {"tool":"bash","command":if matching {command.clone()} else {"echo".into()},"cwd":allowance_cwd}
            ]);
            let p = policy(c);
            match plan(name, a.clone(), &view, &p) {
                Err(r) => assert!(
                    [ResultCode::InvalidArguments, ResultCode::ProtectedPath].contains(&r.code),
                    "iteration {iteration}: {r:?}"
                ),
                Ok(built) => {
                    let has_match = p
                        .allowances()
                        .iter()
                        .any(|allowance| allowance.matches(&built));
                    // Independent oracle for these generated patterns, not the
                    // implementation's matching result feeding its own expectation.
                    let expected_match = matching
                        && match name {
                            "write" => matches!(path, "src/a" | "./src//a"),
                            "edit" => path == "a",
                            "bash" => {
                                if allowance_cwd == "." {
                                    cwd == "."
                                } else {
                                    matches!(cwd, "src" | "./src/")
                                }
                            }
                            _ => false,
                        };
                    assert_eq!(has_match, expected_match, "iteration {iteration} {name}");
                    let expected = if denied {
                        0
                    } else if expected_match {
                        2
                    } else {
                        1
                    };
                    assert_eq!(
                        outcome(decide(&built, built.facts(), &p)),
                        expected,
                        "iteration {iteration} {name}"
                    );
                }
            }
        }
    }
}

#[test]
fn schema_integral_numbers_normalize_without_selecting_defaults() {
    let view = FakeWorkspace::new();
    let p = default_policy();
    for (name, a, key, expected) in [
        (
            "read",
            json!({"path":"a","offset_bytes":1.0,"max_bytes":2.0}),
            "offset_bytes",
            1,
        ),
        ("read", json!({"path":"a","max_bytes":2.0}), "max_bytes", 2),
        (
            "bash",
            json!({"command":"x","timeout_ms":1.0}),
            "timeout_ms",
            1,
        ),
    ] {
        let built = plan(name, a, &view, &p).unwrap();
        let wire: Value = serde_json::from_slice(built.bytes()).unwrap();
        assert_eq!(wire["arguments"][key], expected);
    }
}

#[test]
fn plans_reject_cross_tool_operations_changed_policy_and_substituted_facts() {
    let registry = Registry::builtin();
    let p = default_policy();
    let view = FakeWorkspace::new();
    let operation = registry
        .resolve("read", &p)
        .unwrap()
        .validate(&args("read"))
        .unwrap();
    invalid(
        registry
            .resolve("write", &p)
            .unwrap()
            .plan(&operation, &call(), &view, &p),
    );
    let built = plan("read", args("read"), &view, &p).unwrap();
    let mut c = config();
    c["workspace_read"] = json!("deny"); // Even reusing the revision cannot change the artifact binding.
    assert_eq!(outcome(decide(&built, built.facts(), &policy(c))), 0);
    let mut other = plan("write", args("write"), &view, &p).unwrap();
    other.set_test_protection(Protection::ReadOnly, Protection::None);
    assert_eq!(outcome(decide(&built, other.facts(), &p)), 0);
    let mut c = config();
    c["enabled_tools"] = json!([]);
    assert_eq!(outcome(decide(&built, built.facts(), &policy(c))), 0);
}

#[test]
fn effects_pass_real_v2_storage_state_validator_without_io() {
    use crate::session::{JournalEvent, Record, RecordPayload, state::State};
    let fixture = include_str!("../../../../docs/rfcs/001/examples/interrupted-execution.jsonl");
    let records: Vec<Record> = fixture
        .lines()
        .map(|line| {
            serde_json::from_value::<Record>(
                serde_json::from_str::<Value>(line).unwrap()["record"].clone(),
            )
            .unwrap()
        })
        .collect();
    for (name, expected) in [
        ("read", EffectClass::Read),
        ("write", EffectClass::FilesystemMutation),
        ("edit", EffectClass::FilesystemMutation),
        ("bash", EffectClass::Process),
    ] {
        let built = plan(name, args(name), &FakeWorkspace::new(), &default_policy()).unwrap();
        assert_eq!(built.effects().as_slice(), [expected]);
        assert_eq!(
            built.backend(),
            if name == "bash" {
                ExecutionBackend::Bubblewrap
            } else {
                ExecutionBackend::TrustedFs
            }
        );
        let mut records = records.clone();
        let mut saw_intent = false;
        for record in &mut records {
            record.version = 2;
            if let RecordPayload::Commit(commit) = &mut record.payload {
                for event in &mut commit.events {
                    if let JournalEvent::ExecutionIntent(intent) = event {
                        intent.effects = Some(built.effects().clone());
                        intent.backend = built.backend();
                        saw_intent = true;
                    }
                }
            }
        }
        let mut checksum = Digest::of(b"header");
        let mut state = State::start(&records[0], checksum.clone()).unwrap();
        for record in records.iter_mut().skip(1) {
            record.previous_checksum = Some(checksum);
            checksum = Digest::of(&serde_json::to_vec(record).unwrap());
            state.apply(record, checksum.clone()).unwrap();
        }
        assert!(saw_intent);
    }
}

#[test]
fn all_result_codes_match_real_v2_schema_pattern() {
    let schema: Value = serde_json::from_str(include_str!(
        "../../../../docs/rfcs/001/schema/session-v2.schema.json"
    ))
    .unwrap();
    fn find_pattern(value: &Value) -> Option<Value> {
        if value.get("pattern").and_then(Value::as_str) == Some("^[a-z][a-z0-9_]{0,63}$") {
            return Some(value.clone());
        }
        match value {
            Value::Object(m) => m.values().find_map(find_pattern),
            Value::Array(a) => a.iter().find_map(find_pattern),
            _ => None,
        }
    }
    let validator = jsonschema::validator_for(&find_pattern(&schema).unwrap()).unwrap();
    for &code in ResultCode::ALL {
        assert_eq!(ResultCode::parse(code.as_str()), Some(code));
        assert_eq!(
            ResultCode::ALL
                .iter()
                .filter(|&&other| other == code)
                .count(),
            1,
            "duplicate result code"
        );
        assert!(
            validator.is_valid(&json!(code.as_str())),
            "{}",
            code.as_str()
        );
    }
}

#[test]
fn result_code_parse_rejects_unknown_and_inexact_spellings() {
    for code in [
        "",
        "future_result_code",
        "Cancelled",
        " cancelled",
        "cancelled ",
    ] {
        assert_eq!(ResultCode::parse(code), None);
    }
}

#[test]
fn tools_source_has_no_runtime_io() {
    // Explicit I/O boundaries: trusted fs/ and sandbox/ (args, output,
    // process, probe). Every other tools module, including exec, stays pure.
    let io_exceptions = ["fs", "sandbox"];
    let pure_files = [
        ("mod", include_str!("mod.rs")),
        ("path", include_str!("path.rs")),
        ("builtin", include_str!("builtin.rs")),
        ("exec", include_str!("exec.rs")),
        ("plan", include_str!("plan.rs")),
        ("policy", include_str!("policy.rs")),
        ("recovery", include_str!("recovery.rs")),
        ("search", include_str!("search.rs")),
        ("tests", include_str!("tests.rs")),
    ];
    for line in include_str!("mod.rs").lines() {
        if let Some(module) = line
            .strip_prefix("mod ")
            .or_else(|| line.strip_prefix("pub mod "))
            .and_then(|line| line.strip_suffix(';'))
        {
            assert!(
                pure_files.iter().any(|(name, _)| *name == module)
                    || io_exceptions.contains(&module),
                "unclassified tools module: {module}"
            );
        }
    }
    for (name, source) in pure_files {
        for forbidden in [
            ["std", "fs"].join("::"),
            ["std", "process"].join("::"),
            ["rustix", "fs"].join("::"),
            ["to", "kio"].concat(),
        ] {
            assert!(!source.contains(&forbidden), "{name}: {forbidden}");
        }
    }
}

#[test]
fn search_tools_follow_workspace_read_ignore_allowances_and_bind_every_argument() {
    let mut view = FakeWorkspace::new();
    view.file("dir/a.rs", b"needle");
    view.directory("dir");
    let cases = [
        ("list", json!({"path":"."})),
        ("glob", json!({"pattern":"**/*.rs","path":"."})),
        ("grep", json!({"pattern":"needle","path":"."})),
    ];
    for (name, a) in &cases {
        let name = *name;
        for setting in ["allow", "ask", "deny"] {
            let mut c = config();
            c["enabled_tools"] = json!(["read", "write", "edit", "bash", "list", "glob", "grep"]);
            c["workspace_read"] = json!(setting);
            // Reads never consume allowances; an unrelated grant cannot widen them.
            c["allowances"] = json!([{"tool":"write","path":"."}]);
            let p = policy(c);
            let built = plan(name, a.clone(), &view, &p).unwrap();
            let expected = match setting {
                "allow" => 2,
                "deny" => 0,
                _ => 1,
            };
            let auth = decide(&built, built.facts(), &p);
            assert_eq!(outcome(auth), expected, "{name} {setting}");
            if setting == "ask" {
                let Authorization::Ask(request) = decide(&built, built.facts(), &p) else {
                    panic!("ask row must produce a ticket");
                };
                assert_eq!(Digest::of(&request.plan_bytes), request.plan_digest);
                assert_eq!(request.policy_revision, p.revision());
            }
            // Defaulted arguments are part of the approval digest.
            let mut changed = a.clone();
            match name {
                "list" => changed["path"] = json!("dir"),
                "glob" => changed["include_ignored"] = json!(true),
                _ => changed["case_insensitive"] = json!(true),
            }
            assert_ne!(
                plan(name, changed, &view, &p).unwrap().digest(),
                built.digest(),
                "{name}: an argument change must change the approval digest"
            );
        }
    }
    // A protected path is refused with the same code `read` uses, before any walk.
    view.facts_mut("dir").protection = Protection::Denied;
    let p = policy({
        let mut c = config();
        c["enabled_tools"] = json!(["read", "write", "edit", "bash", "list", "glob", "grep"]);
        c
    });
    for (name, a) in [
        ("list", json!({"path":"dir"})),
        ("glob", json!({"pattern":"*","path":"dir"})),
        ("grep", json!({"pattern":"needle","path":"dir"})),
    ] {
        refusal(
            plan(name, a, &view, &p),
            ResultCode::ProtectedPath,
            ToolStatus::Denied,
        );
    }
    view.facts_mut("dir").protection = Protection::None;
    let p = policy({
        let mut c = config();
        c["enabled_tools"] = json!(["read", "write", "edit", "bash", "list", "glob", "grep"]);
        c["protected_paths"] = json!(["dir"]);
        c
    });
    for (name, a) in [
        ("list", json!({"path":"dir"})),
        ("glob", json!({"pattern":"*","path":"dir"})),
        ("grep", json!({"pattern":"needle","path":"dir"})),
    ] {
        refusal(
            plan(name, a, &view, &p),
            ResultCode::ProtectedPath,
            ToolStatus::Denied,
        );
    }
    for (name, a) in [
        ("list", json!({"path":".git"})),
        ("glob", json!({"pattern":"*","path":".git/objects"})),
        ("grep", json!({"pattern":"needle","path":".git"})),
    ] {
        refusal(
            plan(name, a, &view, &p),
            ResultCode::ProtectedPath,
            ToolStatus::Denied,
        );
    }
}
