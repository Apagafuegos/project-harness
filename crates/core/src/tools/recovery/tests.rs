use super::*;
use crate::tools::tests::FakeWorkspace;
use serde_json::json;

fn plan(tool: &str, pre: &str, post: &str, old: &str, new: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"name":tool,"workspace":"/project","target":{"path":"target","pre_image":pre,"content_sha256":Digest::of(post.as_bytes())},"arguments":{"old_text":old,"new_text":new}})).unwrap()
}
fn run(
    tool: &str,
    pre: &str,
    post: &str,
    current: Option<&str>,
    old: &str,
    new: &str,
) -> RecoveryHint {
    let mut ws = FakeWorkspace::new();
    if let Some(text) = current {
        ws.file("target", text.as_bytes());
    }
    recovery_hint(&plan(tool, pre, post, old, new), &ws)
}
#[test]
fn write_images() {
    let pre = Digest::of(b"before").to_string();
    for (current, expected) in [
        (Some("after"), RecoveryHint::MatchesPostImage),
        (Some("before"), RecoveryHint::MatchesPreImage),
        (Some("else"), RecoveryHint::ChangedSince),
        (None, RecoveryHint::Missing),
    ] {
        assert_eq!(run("write", &pre, "after", current, "", ""), expected);
    }
    assert_eq!(
        run("write", "absent", "after", None, "", ""),
        RecoveryHint::MatchesPreImage
    );
    assert_eq!(
        run("write", "absent", "after", Some("else"), "", ""),
        RecoveryHint::ChangedSince
    );
}
#[test]
fn no_op_write_is_post() {
    assert_eq!(
        run(
            "write",
            &Digest::of(b"same").to_string(),
            "same",
            Some("same"),
            "",
            ""
        ),
        RecoveryHint::MatchesPostImage
    );
}
#[test]
fn edit_images() {
    let pre = Digest::of(b"hello world").to_string();
    for (current, expected) in [
        (Some("hello world"), RecoveryHint::MatchesPreImage),
        (Some("hello Rust"), RecoveryHint::MatchesPostImage),
        (Some("else"), RecoveryHint::ChangedSince),
        (None, RecoveryHint::Missing),
        (Some("Rust Rust"), RecoveryHint::ChangedSince),
        (Some("wrong Rust"), RecoveryHint::ChangedSince),
    ] {
        assert_eq!(run("edit", &pre, "", current, "world", "Rust"), expected);
    }
    assert_eq!(
        run("edit", &Digest::of(b"x").to_string(), "", Some(""), "x", ""),
        RecoveryHint::MatchesPostImage
    );
    assert_eq!(
        run("edit", &pre, "", Some("something"), "world", ""),
        RecoveryHint::ChangedSince
    );
}
#[test]
fn overlapping_edit_is_not_post() {
    // Reversing only the first nonoverlapping `aa` would produce `xa`,
    // but `aaa` has two byte offsets, so it is ambiguous.
    assert_eq!(
        run(
            "edit",
            &Digest::of(b"xa").to_string(),
            "",
            Some("aaa"),
            "x",
            "aa"
        ),
        RecoveryHint::ChangedSince
    );
}
#[test]
fn unavailable_and_refusals() {
    let mut ws = FakeWorkspace::new();
    let bytes = plan("edit", &Digest::of(b"pre").to_string(), "", "pre", "post");
    for (data, reason) in [
        (vec![255], "not_utf8"),
        (vec![b'x'; 1024 * 1024 + 1], "too_large"),
    ] {
        ws.file("target", &data);
        assert_eq!(
            recovery_hint(&bytes, &ws),
            RecoveryHint::unavailable(reason)
        );
    }
    ws.file("target", b"post");
    ws.facts_mut("target").protection = Protection::Denied;
    assert_eq!(
        recovery_hint(&bytes, &ws),
        RecoveryHint::unavailable("protected_path")
    );
    assert_eq!(
        recovery_hint(&plan("bash", "absent", "", "", ""), &ws),
        RecoveryHint::unavailable("no_hint_for_tool")
    );
    assert_eq!(
        recovery_hint(b"invalid", &ws),
        RecoveryHint::unavailable("invalid_plan")
    );
    struct Refusing(crate::session::ResultCode, super::super::WorkspaceIdentity);
    impl WorkspaceView for Refusing {
        fn identity(&self) -> &super::super::WorkspaceIdentity {
            &self.1
        }
        fn inspect(
            &self,
            _: &WorkspacePath,
        ) -> Result<super::super::FileFacts, super::super::Refusal> {
            Err(super::super::Refusal::new(self.0, "refused"))
        }
        fn read_text(&self, _: &WorkspacePath, _: u64) -> Result<String, super::super::Refusal> {
            unreachable!()
        }
    }
    for tool in ["write", "edit"] {
        for code in [
            crate::session::ResultCode::SymlinkRejected,
            crate::session::ResultCode::ProtectedPath,
            crate::session::ResultCode::InfrastructureFailure,
            crate::session::ResultCode::NotUtf8,
            crate::session::ResultCode::TooLarge,
        ] {
            let refusing = Refusing(code, super::super::WorkspaceIdentity("/project".into()));
            assert_eq!(
                recovery_hint(
                    &plan(tool, &Digest::of(b"pre").to_string(), "post", "pre", "post"),
                    &refusing
                ),
                RecoveryHint::unavailable(code.as_str())
            );
        }
    }
}

#[test]
fn serialized_hint_names_and_golden_plans() {
    for (hint, name) in [
        (RecoveryHint::MatchesPostImage, "matches_post_image"),
        (RecoveryHint::MatchesPreImage, "matches_pre_image"),
        (RecoveryHint::ChangedSince, "changed_since"),
        (RecoveryHint::Missing, "missing"),
    ] {
        assert_eq!(serde_json::to_value(hint).unwrap(), json!(name));
    }
    assert_eq!(
        serde_json::to_value(RecoveryHint::unavailable("refusal")).unwrap(),
        json!({"unavailable":{"reason":"refusal"}})
    );
    let mut ws = FakeWorkspace::new();
    ws.file("new", b"new text");
    assert_eq!(
        recovery_hint(include_bytes!("../golden/write.json"), &ws),
        RecoveryHint::MatchesPostImage
    );
    ws.file("a", b"hello Rust");
    assert_eq!(
        recovery_hint(include_bytes!("../golden/edit.json"), &ws),
        RecoveryHint::MatchesPostImage
    );
    for bytes in [
        &include_bytes!("../golden/read.json")[..],
        &include_bytes!("../golden/bash.json")[..],
    ] {
        assert_eq!(
            recovery_hint(bytes, &ws),
            RecoveryHint::unavailable("no_hint_for_tool")
        );
    }
}
