use std::{
    cell::Cell,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    time::Duration,
};

use crate::tools::{AuthorizedOperation, CancelToken, ExecutionOutcome, Executor};
use rustix::io::Errno;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;
use crate::{
    session::{ArtifactRef, CallRef, ContentBlock, Digest, ResultCode, ToolStatus},
    tools::{FileKind, Plan, PolicySnapshot, Refusal, Registry, WorkspaceView},
};

thread_local! { static OPENS: Cell<usize> = const { Cell::new(0) }; }
pub(super) fn record_open() {
    OPENS.with(|n| n.set(n.get() + 1));
}
fn path(value: &str) -> WorkspacePath {
    WorkspacePath::new(value).unwrap()
}
fn workspace(root: &TempDir) -> FsWorkspace {
    FsWorkspace::open(root.path(), &FsProtectedPaths::default()).unwrap()
}
fn policy() -> PolicySnapshot {
    let mut config: Value = serde_json::from_slice(include_bytes!("../../../../../docs/rfcs/001/examples/artifacts/7e66535a506e1c533025006e25e2fced162ef19d026d0db999154218ae7a30ea")).unwrap();
    config["protected_paths"] = json!([]);
    config["enabled_tools"].as_array_mut().unwrap().push(json!("multi_edit"));
    let bytes = serde_json::to_vec(&config).unwrap();
    PolicySnapshot::parse(
        ArtifactRef {
            sha256: Digest::of(&bytes),
            bytes: bytes.len() as u64,
        },
        &bytes,
    )
    .unwrap()
}
fn plan(view: &FsWorkspace, name: &str, arguments: Value) -> Result<Plan, Refusal> {
    let registry = Registry::builtin();
    let policy = policy();
    let tool = registry.resolve(name, &policy).unwrap();
    let call = CallRef {
        response_entry_id: "019a0000-0000-7000-8000-00000000019a".parse().unwrap(),
        call_id: "019a0000-0000-7000-8000-00000000019b".parse().unwrap(),
    };
    tool.plan(&tool.validate(&arguments)?, &call, view, &policy)
}
fn execute(view: &FsWorkspace, plan: &Plan) -> ExecutionOutcome {
    view.execute(plan, &CancelToken::new())
}
fn text(outcome: &ExecutionOutcome) -> &str {
    match &outcome.content[0] {
        ContentBlock::Text { text } => text,
        _ => panic!("file result is not text"),
    }
}
fn assert_code(outcome: ExecutionOutcome, code: ResultCode) {
    assert_eq!(outcome.code, Some(code), "{}", text(&outcome));
    let expected = match code {
        ResultCode::InfrastructureFailure => ToolStatus::InfrastructureFailure,
        ResultCode::Cancelled => ToolStatus::Cancelled,
        ResultCode::Timeout => ToolStatus::TimedOut,
        _ => ToolStatus::Failed,
    };
    assert_eq!(outcome.status, expected);
    assert!(text(&outcome).len() <= 32768);
}
fn assert_no_temp(root: &TempDir) {
    assert!(
        !fs::read_dir(root.path()).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".harness-")),
        "temporary file leaked"
    );
}

#[test]
fn fs_directory_listing_is_bounded_no_symlinks_and_pinned_to_root() {
    let container = TempDir::new().unwrap();
    let root = container.path().join("root");
    fs::create_dir(&root).unwrap();
    for name in ["a", "b", "c", ".hidden"] {
        fs::create_dir(root.join(name)).unwrap();
    }
    fs::create_dir(root.join("a/nested")).unwrap();
    fs::write(root.join("file"), "not a directory").unwrap();
    symlink("a", root.join("alias")).unwrap();
    let view = FsWorkspace::open(&root, &FsProtectedPaths::default()).unwrap();
    let (directories, limited) = view
        .list_directories(None, 2, |n| !n.starts_with('.'))
        .unwrap();
    assert_eq!(directories, [path("a"), path("b")]);
    assert!(limited);
    let (directories, limited) = view
        .list_directories(None, 3, |n| !n.starts_with('.'))
        .unwrap();
    assert_eq!(directories, [path("a"), path("b"), path("c")]);
    assert!(!limited);
    assert_eq!(
        view.list_directories(Some(&path("alias")), 10, |_| true)
            .unwrap_err()
            .code,
        ResultCode::SymlinkRejected
    );
    fs::rename(&root, container.path().join("saved")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("unrelated")).unwrap();
    assert_eq!(
        view.list_directories(Some(&path("a")), 10, |_| true)
            .unwrap()
            .0,
        [path("a/nested")]
    );
    assert_eq!(
        view.list_directories(None, 10, |n| !n.starts_with('.'))
            .unwrap()
            .0,
        [path("a"), path("b"), path("c")]
    );
    fs::rename(
        container.path().join("saved/a"),
        container.path().join("outside"),
    )
    .unwrap();
    assert_eq!(
        view.list_directories(Some(&path("a")), 10, |_| true)
            .unwrap_err()
            .code,
        ResultCode::NotFound
    );
}

#[test]
fn s01_invalid_paths_never_reach_openat2() {
    let registry = Registry::builtin();
    let policy = policy();
    OPENS.with(|n| n.set(0));
    for name in ["read", "write", "edit"] {
        for bad in ["/etc/passwd", "../outside", "x/../outside", "x\0y"] {
            let args = match name {
                "read" => json!({"path":bad}),
                "write" => json!({"path":bad,"text":"x"}),
                _ => json!({"path":bad,"old_text":"a","new_text":"b"}),
            };
            assert_eq!(
                registry
                    .resolve(name, &policy)
                    .unwrap()
                    .validate(&args)
                    .unwrap_err()
                    .code,
                ResultCode::InvalidArguments
            );
        }
    }
    OPENS.with(|n| assert_eq!(n.get(), 0));
}

#[test]
fn s01_symlink_final_intermediate_and_outside() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    fs::create_dir(root.path().join("dir")).unwrap();
    fs::write(root.path().join("dir/file"), "inside").unwrap();
    fs::write(outside.path().join("file"), "secret").unwrap();
    symlink("dir/file", root.path().join("final")).unwrap();
    symlink("dir", root.path().join("middle")).unwrap();
    symlink(outside.path(), root.path().join("outside")).unwrap();
    let view = workspace(&root);
    for candidate in ["final", "middle/file", "outside/file"] {
        assert_eq!(
            view.inspect(&path(candidate)).unwrap_err().code,
            ResultCode::SymlinkRejected,
            "{candidate}"
        );
        for tool in ["read", "write", "edit"] {
            let args = match tool {
                "read" => json!({"path":candidate}),
                "write" => json!({"path":candidate,"text":"x","overwrite":true}),
                _ => json!({"path":candidate,"old_text":"inside","new_text":"x"}),
            };
            assert_eq!(
                plan(&view, tool, args).unwrap_err().code,
                ResultCode::SymlinkRejected
            );
        }
    }
    assert_eq!(
        fs::read_to_string(outside.path().join("file")).unwrap(),
        "secret"
    );
}

#[test]
fn s01_proc_root_and_fd_magic_links() {
    use std::os::fd::AsRawFd;
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let file = fs::File::create(outside.path().join("secret")).unwrap();
    symlink("/proc/self/root", root.path().join("proc-root")).unwrap();
    symlink(
        format!("/proc/self/fd/{}", file.as_raw_fd()),
        root.path().join("proc-fd"),
    )
    .unwrap();
    let view = workspace(&root);
    for candidate in ["proc-root/etc/passwd", "proc-fd"] {
        assert_eq!(
            view.inspect(&path(candidate)).unwrap_err().code,
            ResultCode::SymlinkRejected
        );
    }
}

#[test]
fn s01_mount_crossing_dev_shm_is_required_not_skipped() {
    let dev = fs::metadata("/dev").expect("S01 requires /dev");
    let shm =
        fs::metadata("/dev/shm").expect("S01 requires a separate /dev/shm mount (tmpfs on CI)");
    assert_ne!(
        dev.dev(),
        shm.dev(),
        "S01 precondition: /dev/shm must be a separate mount from /dev; provision tmpfs on this runner"
    );
    let view = FsWorkspace::open("/dev", &FsProtectedPaths::default()).unwrap();
    let error = view.inspect(&path("shm")).unwrap_err();
    assert_eq!(error.code, ResultCode::ProtectedPath);
    assert!(error.detail.contains("crosses a mount"));
}

#[test]
fn s01_hard_links_to_outside_refused_during_planning() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("secret"), "outside").unwrap();
    fs::hard_link(outside.path().join("secret"), root.path().join("linked")).unwrap();
    let view = workspace(&root);
    let facts = view.inspect(&path("linked")).unwrap();
    assert_eq!(facts.nlink, 2);
    for (tool, args) in [
        ("read", json!({"path":"linked"})),
        (
            "write",
            json!({"path":"linked","text":"oops","overwrite":true}),
        ),
        (
            "edit",
            json!({"path":"linked","old_text":"outside","new_text":"oops"}),
        ),
    ] {
        assert_eq!(
            plan(&view, tool, args).unwrap_err().code,
            ResultCode::HardLinkRejected
        );
    }
    assert_eq!(
        fs::read_to_string(outside.path().join("secret")).unwrap(),
        "outside"
    );
}

#[test]
fn s01_hard_links_introduced_after_planning_refused_for_all_tools() {
    for tool in ["read", "write", "edit"] {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(root.path().join("file"), "original").unwrap();
        let view = workspace(&root);
        let args = match tool {
            "read" => json!({"path":"file"}),
            "write" => json!({"path":"file","text":"replacement","overwrite":true}),
            _ => json!({"path":"file","old_text":"original","new_text":"replacement"}),
        };
        let plan = plan(&view, tool, args).unwrap();
        fs::hard_link(root.path().join("file"), outside.path().join("linked")).unwrap();
        assert_code(execute(&view, &plan), ResultCode::HardLinkRejected);
        assert_eq!(
            fs::read_to_string(outside.path().join("linked")).unwrap(),
            "original"
        );
        assert_no_temp(&root);
    }
}

#[test]
fn s01_directory_symlink_swap_between_plan_and_execution() {
    for tool in ["read", "write", "edit"] {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir(root.path().join("dir")).unwrap();
        fs::write(root.path().join("dir/file"), "original").unwrap();
        fs::write(outside.path().join("file"), "outside").unwrap();
        let view = workspace(&root);
        let args = match tool {
            "read" => json!({"path":"dir/file"}),
            "write" => json!({"path":"dir/file","text":"replacement","overwrite":true}),
            _ => json!({"path":"dir/file","old_text":"original","new_text":"replacement"}),
        };
        let plan = plan(&view, tool, args).unwrap();
        fs::rename(root.path().join("dir"), root.path().join("saved")).unwrap();
        symlink(outside.path(), root.path().join("dir")).unwrap();
        assert_code(execute(&view, &plan), ResultCode::SymlinkRejected);
        assert_eq!(
            fs::read_to_string(outside.path().join("file")).unwrap(),
            "outside"
        );
    }
}

#[test]
fn s01_changed_content_is_stale_for_read_write_and_edit() {
    for tool in ["read", "write", "edit"] {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("file"), "original").unwrap();
        let view = workspace(&root);
        let args = match tool {
            "read" => json!({"path":"file"}),
            "write" => json!({"path":"file","text":"replacement","overwrite":true}),
            _ => json!({"path":"file","old_text":"original","new_text":"replacement"}),
        };
        let plan = plan(&view, tool, args).unwrap();
        fs::write(root.path().join("file"), "modified").unwrap();
        assert_code(execute(&view, &plan), ResultCode::StalePlan);
        assert_eq!(
            fs::read_to_string(root.path().join("file")).unwrap(),
            "modified"
        );
        assert_no_temp(&root);
    }
}

#[test]
fn s01_created_target_between_plan_and_execution_is_stale() {
    let root = TempDir::new().unwrap();
    let view = workspace(&root);
    let plan = plan(&view, "write", json!({"path":"new","text":"replacement"})).unwrap();
    fs::write(root.path().join("new"), "concurrent").unwrap();
    let mut created_temp = false;
    let outcome = view.execute_hooked(
        &plan,
        &CancelToken::new(),
        &mut |step| {
            created_temp |= step == Step::CreateTemp;
            Ok(())
        },
        None,
        None,
    );
    assert_code(outcome, ResultCode::StalePlan);
    assert!(
        !created_temp,
        "absent pre-image must be checked before staging a replacement"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("new")).unwrap(),
        "concurrent"
    );
    assert_no_temp(&root);
}

#[test]
fn s01_created_target_at_rename_requires_noreplace() {
    let root = TempDir::new().unwrap();
    let view = workspace(&root);
    let plan = plan(&view, "write", json!({"path":"new","text":"replacement"})).unwrap();
    let outcome = view.execute_hooked(
        &plan,
        &CancelToken::new(),
        &mut |step| {
            if step == Step::Rename {
                fs::write(root.path().join("new"), "concurrent").unwrap();
            }
            Ok(())
        },
        None,
        None,
    );
    // NOREPLACE EEXIST proves no replacement happened, so the effect is not
    // merely "unknown": it is provably not applied.
    assert_eq!(
        outcome.file_effect,
        Some(crate::session::FileEffect::NotApplied)
    );
    assert_code(outcome, ResultCode::StalePlan);
    assert_eq!(
        fs::read_to_string(root.path().join("new")).unwrap(),
        "concurrent"
    );
    assert_no_temp(&root);
}

#[test]
fn s01_changed_content_while_preparing_replacement_is_stale() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("file"), "original").unwrap();
    let view = workspace(&root);
    let plan = plan(
        &view,
        "write",
        json!({"path":"file","text":"replacement","overwrite":true}),
    )
    .unwrap();
    let outcome = view.execute_hooked(
        &plan,
        &CancelToken::new(),
        &mut |step| {
            if step == Step::Recheck {
                fs::write(root.path().join("file"), "concurrent").unwrap();
            }
            Ok(())
        },
        None,
        None,
    );
    assert_code(outcome, ResultCode::StalePlan);
    assert_eq!(
        fs::read_to_string(root.path().join("file")).unwrap(),
        "concurrent"
    );
    assert_no_temp(&root);
}

#[test]
fn s01_protected_paths_direct_and_different_spelling_by_inode() {
    let root = TempDir::new().unwrap();
    for dir in ["secrets", "policy", "state"] {
        fs::create_dir(root.path().join(dir)).unwrap();
        fs::write(root.path().join(dir).join("file"), "secret").unwrap();
    }
    fs::write(root.path().join("credential"), "secret").unwrap();
    let view = FsWorkspace::open(
        root.path(),
        &FsProtectedPaths {
            configured: ["secrets", "policy", "credential", "missing"]
                .map(path)
                .into(),
            harness_data_root: Some(path("state")),
        },
    )
    .unwrap();
    for original in ["secrets", "policy", "state", "credential"] {
        let suffix = if original == "credential" {
            ""
        } else {
            "/file"
        };
        let direct = format!("{original}{suffix}");
        assert_eq!(
            view.inspect(&path(&direct)).unwrap().protection,
            Protection::Denied
        );
        assert_eq!(
            plan(&view, "read", json!({"path":direct}))
                .unwrap_err()
                .code,
            ResultCode::ProtectedPath
        );
        let alias = format!("renamed-{original}");
        fs::rename(root.path().join(original), root.path().join(&alias)).unwrap();
        let different = format!("{alias}{suffix}");
        assert_eq!(
            view.inspect(&path(&different)).unwrap().protection,
            Protection::Denied,
            "protection must follow inode, not original path spelling"
        );
        assert_eq!(
            plan(&view, "read", json!({"path":different}))
                .unwrap_err()
                .code,
            ResultCode::ProtectedPath
        );
        if !suffix.is_empty() {
            assert_eq!(
                view.inspect(&path(&format!("{alias}/absent")))
                    .unwrap()
                    .protection,
                Protection::Denied
            );
            assert_eq!(
                plan(
                    &view,
                    "write",
                    json!({"path":format!("{alias}/new"),"text":"oops"})
                )
                .unwrap_err()
                .code,
                ResultCode::ProtectedPath
            );
        }
    }
}

#[test]
fn s01_protection_rechecked_at_execution_when_denied_directory_moved() {
    let root = TempDir::new().unwrap();
    for dir in ["protected", "ordinary"] {
        fs::create_dir(root.path().join(dir)).unwrap();
        fs::write(root.path().join(dir).join("file"), "same").unwrap();
    }
    let view = FsWorkspace::open(
        root.path(),
        &FsProtectedPaths {
            configured: vec![path("protected")],
            harness_data_root: None,
        },
    )
    .unwrap();
    let plan = plan(
        &view,
        "write",
        json!({"path":"ordinary/file","text":"replacement","overwrite":true}),
    )
    .unwrap();
    fs::rename(root.path().join("ordinary"), root.path().join("saved")).unwrap();
    fs::rename(root.path().join("protected"), root.path().join("ordinary")).unwrap();
    assert_code(execute(&view, &plan), ResultCode::ProtectedPath);
    assert_eq!(
        fs::read_to_string(root.path().join("ordinary/file")).unwrap(),
        "same"
    );
}

#[test]
fn s01_git_is_read_only_including_renamed_directory_and_absent_children() {
    let root = TempDir::new().unwrap();
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::write(root.path().join(".git/config"), "git data").unwrap();
    let view = workspace(&root);
    assert_eq!(
        view.inspect(&path(".git/config")).unwrap().protection,
        Protection::ReadOnly
    );
    assert_eq!(
        execute(
            &view,
            &plan(&view, "read", json!({"path":".git/config"})).unwrap()
        )
        .status,
        ToolStatus::Succeeded
    );
    for prefix in [".git", "renamed-git"] {
        if prefix != ".git" {
            fs::rename(root.path().join(".git"), root.path().join(prefix)).unwrap();
        }
        for target in [format!("{prefix}/config"), format!("{prefix}/new")] {
            assert_eq!(
                plan(
                    &view,
                    "write",
                    json!({"path":target,"text":"oops","overwrite":true})
                )
                .unwrap_err()
                .code,
                ResultCode::ProtectedPath
            );
        }
    }
}

#[test]
fn s01_configured_git_denial_overrides_builtin_read_only() {
    let root = TempDir::new().unwrap();
    fs::create_dir(root.path().join(".git")).unwrap();
    let view = FsWorkspace::open(
        root.path(),
        &FsProtectedPaths {
            configured: vec![path(".git")],
            harness_data_root: None,
        },
    )
    .unwrap();
    assert_eq!(
        view.inspect(&path(".git")).unwrap().protection,
        Protection::Denied
    );
}

#[test]
fn s01_cancel_at_every_precommit_step_leaves_original_and_no_temp() {
    for absent in [false, true] {
        let root = TempDir::new().unwrap();
        let view = workspace(&root);
        if !absent {
            fs::write(root.path().join("file"), "original").unwrap();
        }
        let plan = plan(
            &view,
            "write",
            json!({"path":"file","text":"replacement","overwrite":true}),
        )
        .unwrap();
        let mut trace = Vec::new();
        let token = CancelToken::new();
        let outcome = view.execute_hooked(
            &plan,
            &token,
            &mut |step| {
                trace.push(step);
                if step == Step::Rename {
                    token.cancel();
                }
                Ok(())
            },
            None,
            None,
        );
        assert_code(outcome, ResultCode::Cancelled);
        assert_eq!(trace.last(), Some(&Step::Rename));
        for cancel_at in 0..trace.len() {
            let token = CancelToken::new();
            let mut index = 0;
            let outcome = view.execute_hooked(
                &plan,
                &token,
                &mut |_| {
                    if index == cancel_at {
                        token.cancel();
                    }
                    index += 1;
                    Ok(())
                },
                None,
                None,
            );
            assert_code(outcome, ResultCode::Cancelled);
            if absent {
                assert!(!root.path().join("file").exists());
            } else {
                assert_eq!(
                    fs::read_to_string(root.path().join("file")).unwrap(),
                    "original"
                );
            }
            assert_no_temp(&root);
        }
    }
}

#[test]
fn s01_cancel_after_rename_reports_success() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("file"), "original").unwrap();
    let view = workspace(&root);
    let plan = plan(
        &view,
        "write",
        json!({"path":"file","text":"replacement","overwrite":true}),
    )
    .unwrap();
    let token = CancelToken::new();
    let outcome = view.execute_hooked(
        &plan,
        &token,
        &mut |step| {
            if step == Step::SyncParent {
                token.cancel();
            }
            Ok(())
        },
        None,
        None,
    );
    assert_eq!(outcome.status, ToolStatus::Succeeded);
    assert_eq!(outcome.code, None);
    assert!(token.is_cancelled());
    assert_eq!(
        fs::read_to_string(root.path().join("file")).unwrap(),
        "replacement"
    );
    assert_no_temp(&root);
}

#[test]
fn s01_directory_relocated_outside_root_during_resolution_or_reopen() {
    for before_reopen in [false, true] {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir(root.path().join("dir")).unwrap();
        fs::write(root.path().join("dir/file"), "original").unwrap();
        let view = workspace(&root);
        let plan = plan(&view, "read", json!({"path":"dir/file"})).unwrap();
        let mut moved = false;
        let mut resolves = 0;
        let out = view.execute_hooked(
            &plan,
            &CancelToken::new(),
            &mut |step| {
                if step == Step::Resolve {
                    resolves += 1;
                }
                if !moved
                    && ((before_reopen && step == Step::OpenFile)
                        || (!before_reopen && step == Step::Resolve && resolves == 3))
                {
                    fs::rename(root.path().join("dir"), outside.path().join("moved")).unwrap();
                    moved = true;
                }
                Ok(())
            },
            None,
            None,
        );
        assert!(moved);
        assert_code(out, ResultCode::NotFound);
        assert_eq!(
            fs::read_to_string(outside.path().join("moved/file")).unwrap(),
            "original"
        );
    }
}

#[test]
fn fs_read_malformed_boundary_bytes_are_not_silently_discarded() {
    let root = TempDir::new().unwrap();
    let view = workspace(&root);
    for (bytes, offset, max) in [
        (vec![b'a', 0xe2], 0, 2),
        (vec![b'a', 0xe2], 1, 1),
        (vec![0x80, b'a'], 0, 2),
        (vec![b'a', 0x80, b'z'], 1, 1),
        (vec![b'a', 0xc0, 0x80, b'z'], 2, 1),
        (vec![b'a', 0xe2, 0xff], 0, 2),
    ] {
        fs::write(root.path().join("file"), &bytes).unwrap();
        let plan = plan(
            &view,
            "read",
            json!({"path":"file","offset_bytes":offset,"max_bytes":max}),
        )
        .unwrap();
        assert_code(execute(&view, &plan), ResultCode::NotUtf8);
    }
}

#[test]
fn fs_exact_resolution_flags_and_pinned_root_flags() {
    use rustix::fs::{OFlags, ResolveFlags, fcntl_getfl};
    assert_eq!(
        RESOLVE,
        ResolveFlags::BENEATH
            | ResolveFlags::NO_SYMLINKS
            | ResolveFlags::NO_MAGICLINKS
            | ResolveFlags::NO_XDEV
    );
    let root = TempDir::new().unwrap();
    let view = workspace(&root);
    let flags = fcntl_getfl(&view.root).unwrap();
    assert!(flags.contains(OFlags::PATH | OFlags::DIRECTORY));
}

#[test]
fn fs_unavailable_enosys_and_seccomp_eperm_have_no_fallback() {
    let root = TempDir::new().unwrap();
    for (error, name) in [(Errno::NOSYS, "ENOSYS"), (Errno::PERM, "EPERM")] {
        let error =
            FsWorkspace::open_probed(root.path(), &FsProtectedPaths::default(), |_| Err(error))
                .unwrap_err();
        assert!(error.reason.contains(name), "{}", error.reason);
    }
}

#[test]
fn fs_pinned_root_survives_rename_and_replacement_of_root_spelling() {
    let container = TempDir::new().unwrap();
    fs::create_dir(container.path().join("root")).unwrap();
    fs::write(container.path().join("root/file"), "original").unwrap();
    let view =
        FsWorkspace::open(container.path().join("root"), &FsProtectedPaths::default()).unwrap();
    let plan = plan(
        &view,
        "write",
        json!({"path":"file","text":"replacement","overwrite":true}),
    )
    .unwrap();
    fs::rename(
        container.path().join("root"),
        container.path().join("saved"),
    )
    .unwrap();
    fs::create_dir(container.path().join("root")).unwrap();
    fs::write(container.path().join("root/file"), "unrelated").unwrap();
    assert_eq!(execute(&view, &plan).status, ToolStatus::Succeeded);
    assert_eq!(
        fs::read_to_string(container.path().join("saved/file")).unwrap(),
        "replacement"
    );
    assert_eq!(
        fs::read_to_string(container.path().join("root/file")).unwrap(),
        "unrelated"
    );
}

#[test]
fn fs_inspect_regular_directory_absent_and_inherited_protection() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("file"), "hello").unwrap();
    let view = workspace(&root);
    let facts = view.inspect(&path("file")).unwrap();
    assert_eq!(facts.kind, FileKind::Regular);
    assert_eq!(facts.size, 5);
    assert_eq!(facts.digest, Some(Digest::of(b"hello")));
    assert_eq!(view.inspect(&path(".")).unwrap().kind, FileKind::Directory);
    assert_eq!(
        view.inspect(&path("missing/sub/file")).unwrap().kind,
        FileKind::Absent
    );
}

#[test]
fn fs_inspect_above_old_cap_streams_and_detects_concurrent_truncate() {
    let root = TempDir::new().unwrap();
    let file = fs::File::create(root.path().join("large")).unwrap();
    file.set_len(256 * 1024 * 1024 + 1).unwrap();
    let view = workspace(&root);
    // Reach streaming on a genuinely oversized sparse file, then shorten it
    // after the first real chunk. This gates both the absence of a whole-file
    // cap and concurrent-change detection without hashing 256 MiB on every
    // debug CI run (software SHA-256 can take over a minute on ARM).
    let mut reads = 0;
    let mut hook = |step| {
        if step == Step::HashRead {
            reads += 1;
            if reads == 2 {
                file.set_len(0).unwrap();
            }
        }
        Ok(())
    };
    let error = view
        .inspect_controlled(&path("large"), &mut Control::planning(&mut hook))
        .unwrap_err();
    assert_eq!(error.code, ResultCode::StalePlan);
    assert_eq!(
        reads, 2,
        "large inspect must reach streamed hashing, not a size cap"
    );
    file.set_len(3 * 65536 + 7).unwrap();
    let facts = view.inspect(&path("large")).unwrap();
    assert_eq!(facts.digest, Some(Digest::of(&vec![0; 3 * 65536 + 7])));
    let plan = plan(&view, "read", json!({"path":"large","max_bytes":1})).unwrap();
    assert_eq!(execute(&view, &plan).status, ToolStatus::Succeeded);
}

#[test]
fn fs_read_utf8_inward_ranges_not_utf8_and_empty_ranges() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("file"), "aé🦀z").unwrap();
    let view = workspace(&root);
    for (offset, max, expected, payload) in [
        (2, 6, "bytes 3..8; more=false", "🦀z"),
        (0, 5, "bytes 0..3; more=true", "aé"),
        (99, 9, "bytes 8..8; more=false", ""),
        (3, 0, "bytes 3..3; more=true", ""),
    ] {
        let plan = plan(
            &view,
            "read",
            json!({"path":"file","offset_bytes":offset,"max_bytes":max}),
        )
        .unwrap();
        let out = execute(&view, &plan);
        assert_eq!(out.status, ToolStatus::Succeeded, "{}", text(&out));
        assert!(text(&out).starts_with(expected), "{}", text(&out));
        assert_eq!(text(&out).split_once('\n').unwrap().1, payload);
    }
    fs::write(root.path().join("binary"), [b'a', 0xff, b'z']).unwrap();
    assert_code(
        execute(
            &view,
            &plan(&view, "read", json!({"path":"binary"})).unwrap(),
        ),
        ResultCode::NotUtf8,
    );
}

#[test]
fn fs_read_output_bounded_with_continuable_actual_byte_range() {
    let root = TempDir::new().unwrap();
    let contents = "é".repeat(40000);
    fs::write(root.path().join("file"), &contents).unwrap();
    let view = workspace(&root);
    let out = execute(&view, &plan(&view, "read", json!({"path":"file"})).unwrap());
    assert_eq!(out.status, ToolStatus::Succeeded);
    assert!(out.output_truncated);
    assert!(text(&out).len() <= 32768);
    let (header, payload) = text(&out).split_once('\n').unwrap();
    let end: usize = header
        .split_once("..")
        .unwrap()
        .1
        .split(';')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(end, payload.len());
    let next = execute(
        &view,
        &plan(&view, "read", json!({"path":"file","offset_bytes":end})).unwrap(),
    );
    assert!(text(&next).starts_with(&format!("bytes {end}..")));
    assert!(out.artifacts.is_empty());
}

#[test]
fn fs_executor_trait_runs_authorized_plans_and_shared_cancellation() {
    use crate::session::ExecutionBackend;
    let root = TempDir::new().unwrap();
    let view = workspace(&root);
    let executor: &dyn Executor = &view;
    assert_eq!(executor.backend(), ExecutionBackend::TrustedFs);
    for (name, arguments) in [
        ("write", json!({"path":"file","text":"hello"})),
        ("read", json!({"path":"file"})),
        (
            "edit",
            json!({"path":"file","old_text":"hello","new_text":"done"}),
        ),
    ] {
        let operation = AuthorizedOperation::new(
            plan(&view, name, arguments).unwrap(),
            "019a0000-0000-7000-8000-00000000019c".parse().unwrap(),
            "019a0000-0000-7000-8000-00000000019d".parse().unwrap(),
            None,
        );
        let outcome = executor.execute(&operation, &CancelToken::new());
        assert_eq!(outcome.status, ToolStatus::Succeeded, "{}", text(&outcome));
        assert_eq!(outcome.code, None);
        assert!(outcome.duration_ms.is_some());
        assert_eq!(outcome.exit_code, None);
        assert!(outcome.artifacts.is_empty());
    }
    assert_eq!(
        fs::read_to_string(root.path().join("file")).unwrap(),
        "done"
    );
    let operation = AuthorizedOperation::new(
        plan(&view, "read", json!({"path":"file"})).unwrap(),
        "019a0000-0000-7000-8000-00000000019c".parse().unwrap(),
        "019a0000-0000-7000-8000-00000000019d".parse().unwrap(),
        None,
    );
    let cancel = CancelToken::new();
    cancel.clone().cancel();
    assert_code(executor.execute(&operation, &cancel), ResultCode::Cancelled);
    let bash = AuthorizedOperation::new(
        plan(&view, "bash", json!({"command":"true"})).unwrap(),
        "019a0000-0000-7000-8000-00000000019c".parse().unwrap(),
        "019a0000-0000-7000-8000-00000000019d".parse().unwrap(),
        None,
    );
    assert_ne!(bash.plan().backend(), ExecutionBackend::TrustedFs);
    assert_code(
        executor.execute(&bash, &CancelToken::new()),
        ResultCode::InfrastructureFailure,
    );
    assert_no_temp(&root);
}

#[test]
fn fs_write_and_edit_strip_special_mode_bits() {
    for tool in ["write", "edit"] {
        for mode in [0o4755, 0o2755, 0o1755, 0o7755] {
            let root = TempDir::new().unwrap();
            let view = workspace(&root);
            let file = root.path().join("file");
            fs::write(&file, "original").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(fs::metadata(&file).unwrap().mode() & 0o7777, mode);
            let arguments = if tool == "write" {
                json!({"path":"file","text":"replacement","overwrite":true})
            } else {
                json!({"path":"file","old_text":"original","new_text":"replacement"})
            };
            let outcome = execute(&view, &plan(&view, tool, arguments).unwrap());
            assert_eq!(outcome.status, ToolStatus::Succeeded, "{}", text(&outcome));
            assert_eq!(fs::metadata(&file).unwrap().mode() & 0o7777, 0o755);
            assert_eq!(fs::read_to_string(&file).unwrap(), "replacement");
            assert_no_temp(&root);
        }
    }
}

#[test]
fn fs_write_and_edit_are_atomic_preserve_mode_and_report_post_image() {
    let root = TempDir::new().unwrap();
    let view = workspace(&root);
    let out = execute(
        &view,
        &plan(&view, "write", json!({"path":"file","text":"hello world"})).unwrap(),
    );
    assert_eq!(out.status, ToolStatus::Succeeded);
    assert!(text(&out).contains(Digest::of(b"hello world").as_str()));
    assert_eq!(
        fs::metadata(root.path().join("file")).unwrap().mode() & 0o777,
        0o644
    );
    fs::set_permissions(root.path().join("file"), fs::Permissions::from_mode(0o751)).unwrap();
    let old = fs::File::open(root.path().join("file")).unwrap();
    let old_inode = old.metadata().unwrap().ino();
    let out = execute(
        &view,
        &plan(
            &view,
            "edit",
            json!({"path":"file","old_text":"world","new_text":"Rust"}),
        )
        .unwrap(),
    );
    assert_eq!(out.status, ToolStatus::Succeeded);
    assert!(text(&out).contains(Digest::of(b"hello Rust").as_str()));
    let metadata = fs::metadata(root.path().join("file")).unwrap();
    assert_ne!(old_inode, metadata.ino());
    assert_eq!(metadata.mode() & 0o777, 0o751);
    assert_eq!(
        fs::read_to_string(root.path().join("file")).unwrap(),
        "hello Rust"
    );
    assert_no_temp(&root);
}

#[test]
fn fs_edit_limit_utf8_and_overlapping_matches() {
    let root = TempDir::new().unwrap();
    let view = workspace(&root);
    fs::write(root.path().join("file"), "aaa").unwrap();
    assert_eq!(
        plan(
            &view,
            "edit",
            json!({"path":"file","old_text":"aa","new_text":"b"})
        )
        .unwrap_err()
        .code,
        ResultCode::MultipleMatches
    );
    fs::write(root.path().join("file"), [0xff]).unwrap();
    assert_eq!(
        view.read_text(&path("file"), 1).unwrap_err().code,
        ResultCode::NotUtf8
    );
    fs::File::create(root.path().join("file"))
        .unwrap()
        .set_len(1024 * 1024 + 1)
        .unwrap();
    assert_eq!(
        plan(
            &view,
            "edit",
            json!({"path":"file","old_text":"a","new_text":"b"})
        )
        .unwrap_err()
        .code,
        ResultCode::TooLarge
    );
    assert_eq!(
        view.read_text(&path("file"), 1024).unwrap_err().code,
        ResultCode::TooLarge
    );
}

#[test]
fn fs_execution_rejects_directory_socket_and_deleted_target() {
    let root = TempDir::new().unwrap();
    let view = workspace(&root);
    for kind in ["directory", "socket", "absent"] {
        fs::write(root.path().join("file"), "original").unwrap();
        let plan = plan(&view, "read", json!({"path":"file"})).unwrap();
        fs::remove_file(root.path().join("file")).unwrap();
        let socket;
        let code = match kind {
            "directory" => {
                fs::create_dir(root.path().join("file")).unwrap();
                ResultCode::NotRegularFile
            }
            "socket" => {
                socket = std::os::unix::net::UnixListener::bind(root.path().join("file")).unwrap();
                let _keep = &socket;
                ResultCode::NotRegularFile
            }
            _ => ResultCode::NotFound,
        };
        assert_code(execute(&view, &plan), code);
        if kind == "directory" {
            fs::remove_dir(root.path().join("file")).unwrap();
        } else if kind == "socket" {
            fs::remove_file(root.path().join("file")).unwrap();
        }
    }
}

#[test]
fn fs_missing_parent_at_execution_never_creates_an_ancestor() {
    let root = TempDir::new().unwrap();
    fs::create_dir(root.path().join("dir")).unwrap();
    let view = workspace(&root);
    let plan = plan(&view, "write", json!({"path":"dir/file","text":"new"})).unwrap();
    fs::remove_dir(root.path().join("dir")).unwrap();
    assert_code(execute(&view, &plan), ResultCode::NotFound);
    assert!(!root.path().join("dir").exists());
    assert_no_temp(&root);
}

#[test]
fn fs_unexpected_errno_and_deadline_use_executed_statuses() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("file"), "original").unwrap();
    let view = workspace(&root);
    let plan = plan(
        &view,
        "write",
        json!({"path":"file","text":"replacement","overwrite":true}),
    )
    .unwrap();
    for step in [
        Step::CreateTemp,
        Step::WriteTemp,
        Step::ChmodTemp,
        Step::SyncTemp,
        Step::Recheck,
        Step::Rename,
    ] {
        let out = view.execute_hooked(
            &plan,
            &CancelToken::new(),
            &mut |s| {
                if s == step {
                    Err(errno(Errno::IO))
                } else {
                    Ok(())
                }
            },
            None,
            None,
        );
        assert!(text(&out).contains("EIO"));
        assert_code(out, ResultCode::InfrastructureFailure);
        assert_eq!(
            fs::read_to_string(root.path().join("file")).unwrap(),
            "original"
        );
        assert_no_temp(&root);
    }
    assert_code(
        view.execute_hooked(
            &plan,
            &CancelToken::new(),
            &mut |_| Ok(()),
            Some(Duration::ZERO),
            None,
        ),
        ResultCode::Timeout,
    );
}

#[test]
fn fs_bash_and_foreign_workspace_plan_are_infrastructure_failures() {
    let root = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();
    let view = workspace(&root);
    assert_code(
        execute(
            &view,
            &plan(&view, "bash", json!({"command":"true"})).unwrap(),
        ),
        ResultCode::InfrastructureFailure,
    );
    let foreign = workspace(&other);
    let plan = plan(&foreign, "write", json!({"path":"new","text":"data"})).unwrap();
    assert_code(execute(&view, &plan), ResultCode::InfrastructureFailure);
    assert!(!root.path().join("new").exists());
}

#[test]
fn s01_fifo_and_device_nodes_are_not_read_or_mutated() {
    let root = TempDir::new().unwrap();
    let directory = std::fs::File::open(root.path()).unwrap();
    rustix::fs::mkfifoat(
        &directory,
        "fifo",
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .unwrap();
    let view = workspace(&root);
    assert_eq!(view.inspect(&path("fifo")).unwrap().kind, FileKind::Other);
    for (name, arguments) in [
        ("read", json!({"path":"fifo"})),
        ("write", json!({"path":"fifo","text":"x"})),
        ("edit", json!({"path":"fifo","old_text":"x","new_text":"y"})),
    ] {
        assert_eq!(
            plan(&view, name, arguments).unwrap_err().code,
            ResultCode::NotRegularFile
        );
    }
    // Existing character device: no root privilege and no mknod/skip required.
    let dev =
        FsWorkspace::open(std::path::Path::new("/dev"), &FsProtectedPaths::default()).unwrap();
    assert_eq!(dev.inspect(&path("null")).unwrap().kind, FileKind::Other);
    assert_eq!(
        dev.read_text(&path("null"), 1).unwrap_err().code,
        ResultCode::NotRegularFile
    );
}

#[test]
fn s01_long_paths_and_names_are_bounded_refusals() {
    let root = TempDir::new().unwrap();
    let view = workspace(&root);
    let long = "n".repeat(256);
    let refusal = plan(&view, "read", json!({"path":long})).unwrap_err();
    assert_eq!(refusal.code, ResultCode::TooLarge);
    assert!(refusal.detail.len() < 240);
    assert!(WorkspacePath::new(&"n".repeat(4097)).is_err());
    let longest = format!("{}n", "a/".repeat(2047));
    assert!(WorkspacePath::new(&longest).is_ok());
    assert_eq!(
        plan(&view, "read", json!({"path":longest}))
            .unwrap_err()
            .code,
        ResultCode::NotFound
    );
}
