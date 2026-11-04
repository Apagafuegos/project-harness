//! M8 search behavior on real files and deterministic descriptor race hooks.
use super::{FsProtectedPaths, FsWorkspace, resolve::Step};
use crate::{
    session::{ArtifactRef, CallRef, ContentBlock, Digest, ResultCode, ToolStatus},
    tools::{CancelToken, ExecutionOutcome, Plan, PolicySnapshot, Registry, WorkspacePath},
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::{ffi::OsStringExt, fs::symlink},
    time::{Duration, Instant},
};
use tempfile::TempDir;

fn path(text: &str) -> WorkspacePath {
    WorkspacePath::new(text).unwrap()
}
fn workspace(root: &TempDir) -> FsWorkspace {
    FsWorkspace::open(root.path(), &FsProtectedPaths::default()).unwrap()
}
fn policy(protected: &[&str]) -> PolicySnapshot {
    let bytes = serde_json::to_vec(&json!({"policy_revision":"search-v1","enabled_tools":["read","write","edit","bash","list","glob","grep"],"workspace_read":"allow","workspace_mutation":"ask","process":"ask","network":"deny","protected_paths":protected})).unwrap();
    PolicySnapshot::parse(
        ArtifactRef {
            sha256: Digest::of(&bytes),
            bytes: bytes.len() as u64,
        },
        &bytes,
    )
    .unwrap()
}
fn plan(view: &FsWorkspace, name: &str, args: Value) -> Plan {
    plan_policy(view, name, args, &policy(&[])).unwrap()
}
fn plan_policy(
    view: &FsWorkspace,
    name: &str,
    args: Value,
    policy: &PolicySnapshot,
) -> Result<Plan, crate::tools::Refusal> {
    let registry = Registry::builtin();
    let tool = registry.resolve(name, policy).unwrap();
    tool.plan(
        &tool.validate(&args)?,
        &CallRef {
            response_entry_id: "019a0000-0000-7000-8000-00000000019a".parse().unwrap(),
            call_id: "019a0000-0000-7000-8000-00000000019b".parse().unwrap(),
        },
        view,
        policy,
    )
}
fn inline(outcome: &ExecutionOutcome) -> Value {
    let ContentBlock::Text { text } = &outcome.content[0] else {
        panic!("text result");
    };
    assert!(text.len() <= 32768);
    assert!(serde_json::to_vec(&outcome.content).unwrap().len() <= 32768);
    serde_json::from_str(text).unwrap()
}
fn full(outcome: &ExecutionOutcome) -> Value {
    outcome.artifacts.first().map_or_else(
        || inline(outcome),
        |bytes| serde_json::from_slice(bytes).unwrap(),
    )
}
fn run(view: &FsWorkspace, name: &str, args: Value) -> Value {
    let outcome = view.execute(&plan(view, name, args), &CancelToken::new());
    assert_eq!(outcome.status, ToolStatus::Succeeded, "{outcome:?}");
    full(&outcome)
}
fn write(root: &TempDir, name: &str, text: impl AsRef<[u8]>) {
    let name = root.path().join(name);
    fs::create_dir_all(name.parent().unwrap()).unwrap();
    fs::write(name, text).unwrap();
}

#[test]
fn search_list_defaults_types_sizes_byte_order_and_determinism() {
    let root = TempDir::new().unwrap();
    write(&root, "z", b"123");
    write(&root, "a", b"x");
    fs::create_dir(root.path().join(".hidden")).unwrap();
    symlink("z", root.path().join("alias")).unwrap();
    let view = workspace(&root);
    let result = run(&view, "list", json!({}));
    assert_eq!(
        result["entries"],
        json!([{"name":".hidden","type":"dir"},{"name":"a","type":"file","size":1},{"name":"alias","type":"symlink"},{"name":"z","type":"file","size":3}])
    );
    assert_eq!(result["truncated"], false);
    assert_eq!(result, run(&view, "list", json!({"path":"."})));
}
#[test]
fn search_list_and_glob_path_limits_are_explicit_and_exact() {
    let root = TempDir::new().unwrap();
    for n in (0..1001).rev() {
        write(&root, &format!("f{n:04}"), b"");
    }
    let view = workspace(&root);
    for (name, args, key, limit) in [
        ("list", json!({}), "entries", "entries:1000"),
        ("glob", json!({"pattern":"*"}), "paths", "paths:1000"),
    ] {
        let result = run(&view, name, args.clone());
        assert_eq!(result["count"], 1000);
        assert_eq!(result["truncated"], true);
        assert!(result["limits"].as_array().unwrap().contains(&json!(limit)));
        assert_eq!(result, run(&view, name, args));
        assert_eq!(result[key].as_array().unwrap().len(), 1000);
    }
    fs::remove_file(root.path().join("f1000")).unwrap();
    assert_eq!(
        run(&view, "glob", json!({"pattern":"*"}))["truncated"],
        false
    );
}
#[test]
fn search_glob_walk_directory_limit_is_explicit_and_counts_root() {
    let root = TempDir::new().unwrap();
    for n in 0..20000 {
        fs::create_dir(root.path().join(format!("d{n:05}"))).unwrap();
    }
    let view = workspace(&root);
    let result = run(
        &view,
        "glob",
        json!({"pattern":"*.rs","include_ignored":true}),
    );
    assert_eq!(result["directories_visited"], 20000);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["limits"], json!(["directories:20000"]));
    let grep = run(
        &view,
        "grep",
        json!({"pattern":"never","include_ignored":true}),
    );
    assert_eq!(grep["directories_visited"], 20000);
    assert_eq!(grep["limits"], json!(["directories:20000"]));
}
#[test]
fn search_glob_and_grep_ignore_nested_overrides_negation_and_opt_out() {
    let root = TempDir::new().unwrap();
    write(&root, ".gitignore", b"*.rs\n!keep.rs\nignored/\n");
    write(&root, "drop.rs", b"needle");
    write(&root, "keep.rs", b"needle");
    write(&root, "a/.gitignore", b"!local.rs\n");
    write(&root, "a/local.rs", b"needle");
    write(&root, "a/drop.rs", b"needle");
    write(&root, "ignored/keep.rs", b"needle");
    let view = workspace(&root);
    for (name, args) in [
        ("glob", json!({"pattern":"**/*.rs"})),
        (
            "grep",
            json!({"pattern":"needle","glob":"**/*.rs","output":"files"}),
        ),
    ] {
        let result = run(&view, name, args.clone());
        assert_eq!(result["paths"], json!(["a/local.rs", "keep.rs"]));
        assert_eq!(result, run(&view, name, args.clone()));
        let mut args = args;
        args["include_ignored"] = true.into();
        assert_eq!(
            run(&view, name, args)["paths"],
            json!([
                "a/drop.rs",
                "a/local.rs",
                "drop.rs",
                "ignored/keep.rs",
                "keep.rs"
            ])
        );
    }
    let result = run(&view, "list", json!({"path":"a"}));
    assert_eq!(
        result["entries"],
        json!([{"name":".gitignore","type":"file","size":10},{"name":"local.rs","type":"file","size":6}])
    );
}
#[test]
fn search_dot_directories_require_literal_pattern_components() {
    let root = TempDir::new().unwrap();
    write(&root, ".hidden/a.rs", b"needle");
    write(&root, "a.rs", b"needle");
    let view = workspace(&root);
    assert_eq!(
        run(&view, "glob", json!({"pattern":"**/*.rs"}))["paths"],
        json!(["a.rs"])
    );
    assert_eq!(
        run(&view, "glob", json!({"pattern":".hidden/**/*.rs"}))["paths"],
        json!([".hidden/a.rs"])
    );
    assert_eq!(
        run(&view, "grep", json!({"pattern":"needle","output":"files"}))["paths"],
        json!(["a.rs"])
    );
    assert_eq!(
        run(
            &view,
            "grep",
            json!({"pattern":"needle","glob":".hidden/**/*.rs","output":"files"})
        )["paths"],
        json!([".hidden/a.rs"])
    );
    assert_eq!(
        run(&view, "glob", json!({"path":".hidden","pattern":"*.rs"}))["paths"],
        json!([".hidden/a.rs"])
    );
}
#[test]
fn search_symlink_escape_and_loop_are_never_followed() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    write(&root, "safe/file.rs", b"needle");
    write(&outside, "secret.rs", b"needle outside-secret");
    symlink(outside.path(), root.path().join("escape")).unwrap();
    symlink(
        outside.path().join("secret.rs"),
        root.path().join("alias.rs"),
    )
    .unwrap();
    symlink(".", root.path().join("loop")).unwrap();
    let view = workspace(&root);
    let grep = run(
        &view,
        "grep",
        json!({"pattern":"needle","include_ignored":true}),
    );
    assert_eq!(grep["count"], 1);
    assert!(!grep.to_string().contains("outside-secret"));
    assert_eq!(grep["skipped"]["unreadable"], 0);
    let list = run(&view, "list", json!({}));
    assert_eq!(list["entries"][0]["type"], "symlink");
    assert_eq!(
        run(
            &view,
            "glob",
            json!({"pattern":"**/*.rs","include_ignored":true})
        )["paths"],
        json!(["safe/file.rs"])
    );
    for name in ["list", "glob", "grep"] {
        let args = match name {
            "list" => json!({"path":"escape"}),
            "glob" => json!({"path":"escape","pattern":"*"}),
            _ => json!({"path":"escape","pattern":"needle"}),
        };
        assert_eq!(
            plan_policy(&view, name, args, &policy(&[]))
                .unwrap_err()
                .code,
            ResultCode::SymlinkRejected
        );
    }
}
#[test]
fn search_directory_symlink_swap_mid_walk_never_reads_outside() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    write(&root, "a/inside.rs", b"needle");
    write(&outside, "secret.rs", b"needle secret");
    let view = workspace(&root);
    let plan = plan(&view, "grep", json!({"pattern":"needle"}));
    let mut directories = 0;
    let mut swapped = false;
    let outcome = view.execute_hooked(
        &plan,
        &CancelToken::new(),
        &mut |step| {
            if step == Step::WalkDirectory {
                directories += 1;
            }
            if step == Step::WalkDirectory && directories == 2 {
                fs::rename(root.path().join("a"), root.path().join("saved")).unwrap();
                symlink(outside.path(), root.path().join("a")).unwrap();
                swapped = true;
            }
            Ok(())
        },
        None,
        None,
    );
    assert!(swapped);
    let result = full(&outcome);
    assert_eq!(result["count"], 0);
    assert_eq!(result["truncated"], true);
    assert!(!result.to_string().contains("secret"));
}
#[test]
fn search_protected_paths_and_git_never_appear_or_count_even_with_opt_out() {
    let root = TempDir::new().unwrap();
    write(&root, ".git/secret.rs", b"needle");
    write(&root, "private/secret.rs", b"needle");
    write(&root, "safe.rs", b"needle");
    let view = FsWorkspace::open(
        root.path(),
        &FsProtectedPaths {
            configured: vec![path("private"), path("later")],
            harness_data_root: None,
        },
    )
    .unwrap();
    write(&root, "later/new.rs", b"needle");
    for (name, args) in [
        ("list", json!({})),
        ("glob", json!({"pattern":"**/*.rs","include_ignored":true})),
        ("grep", json!({"pattern":"needle","include_ignored":true})),
    ] {
        let result = run(&view, name, args);
        let rendered = result.to_string();
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("private"));
        assert!(!rendered.contains("later"));
        assert!(!rendered.contains(".git"));
        assert_eq!(
            result["skipped"],
            json!({"binary":0,"non_utf8":0,"oversized":0,"unreadable":0})
        );
    }
    for target in [".git", ".git/secret.rs", "private", "later"] {
        for name in ["list", "glob", "grep"] {
            let mut args = json!({"path":target});
            if name != "list" {
                args["pattern"] = if name == "grep" { "needle" } else { "*" }.into();
            }
            assert_eq!(
                plan_policy(&view, name, args, &policy(&["private", "later"]))
                    .unwrap_err()
                    .code,
                ResultCode::ProtectedPath
            );
        }
    }
    // Renamed protected identities are hidden even under a new, ordinary spelling.
    fs::rename(root.path().join("private"), root.path().join("renamed")).unwrap();
    assert!(
        !run(&view, "list", json!({}))
            .to_string()
            .contains("renamed")
    );
}
#[test]
fn search_ignore_cap_unreadable_and_protected_rules_are_content_free() {
    let root = TempDir::new().unwrap();
    write(&root, ".gitignore", vec![b'x'; 65537]);
    write(&root, "a.rs", b"needle");
    let view = workspace(&root);
    let result = run(&view, "glob", json!({"pattern":"*.rs"}));
    assert_eq!(result["paths"], json!(["a.rs"]));
    assert_eq!(result["warnings"].as_array().unwrap().len(), 1);
    assert!(!result["warnings"].to_string().contains(&"x".repeat(100)));
    fs::remove_file(root.path().join(".gitignore")).unwrap();
    symlink("a.rs", root.path().join(".gitignore")).unwrap();
    let result = run(&view, "glob", json!({"pattern":"*.rs"}));
    assert_eq!(result["warnings"].as_array().unwrap().len(), 1);
    assert_eq!(
        run(
            &view,
            "glob",
            json!({"pattern":"*.rs","include_ignored":true})
        )["warnings"],
        json!([])
    );
}
#[test]
fn search_exact_ignore_cap_is_accepted() {
    let root = TempDir::new().unwrap();
    let mut rule = b"drop.rs\n#".to_vec();
    rule.resize(65536, b'x');
    write(&root, ".gitignore", rule);
    write(&root, "drop.rs", b"needle");
    let view = workspace(&root);
    let result = run(&view, "glob", json!({"pattern":"*.rs"}));
    assert_eq!(result["count"], 0);
    assert_eq!(result["warnings"], json!([]));
}
#[test]
fn search_fifo_devices_long_and_non_utf8_names_do_not_block() {
    let root = TempDir::new().unwrap();
    let fd = fs::File::open(root.path()).unwrap();
    rustix::fs::mkfifoat(&fd, "fifo", rustix::fs::Mode::RUSR).unwrap();
    let name = "n".repeat(255);
    write(&root, &name, b"needle");
    let invalid = std::ffi::OsString::from_vec(b"bad\xff".to_vec());
    fs::write(root.path().join(invalid), b"needle").unwrap();
    let view = workspace(&root);
    let result = run(&view, "list", json!({}));
    assert_eq!(result["entries"][0]["name"], "bad\\xff");
    assert_eq!(result["count"], 2);
    assert!(!result.to_string().contains("fifo"));
    assert_eq!(
        run(&view, "glob", json!({"pattern":"*"}))["paths"],
        json!([name])
    );
    write(&root, ".gitignore", b"bad*\n");
    assert!(!run(&view, "list", json!({})).to_string().contains("bad"));
    let dev = FsWorkspace::open("/dev", &FsProtectedPaths::default()).unwrap();
    let result = run(&dev, "list", json!({}));
    assert!(
        !result["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "null")
    );
}
#[test]
fn search_grep_defaults_sort_context_edges_case_and_files_mode() {
    let root = TempDir::new().unwrap();
    write(&root, "z.rs", b"needle\nmid\nNEEDLE\n");
    write(&root, "a.rs", b"needle\nlast\n");
    let view = workspace(&root);
    let result = run(&view, "grep", json!({"pattern":"needle"}));
    assert_eq!(result["count"], 2);
    assert_eq!(result["matches"][0]["path"], "a.rs");
    assert_eq!(result["matches"][1]["path"], "z.rs");
    assert_eq!(result, run(&view, "grep", json!({"pattern":"needle"})));
    let result = run(
        &view,
        "grep",
        json!({"pattern":"needle","case_insensitive":true,"context_lines":5}),
    );
    assert_eq!(result["count"], 3);
    assert_eq!(result["matches"][0]["before"], json!([]));
    assert_eq!(
        result["matches"][0]["after"],
        json!([{"line":2,"text":"last","text_truncated":false}])
    );
    assert_eq!(result["matches"][2]["after"], json!([]));
    assert_eq!(
        run(
            &view,
            "grep",
            json!({"path":"z.rs","pattern":"NEEDLE","output":"files"})
        )["paths"],
        json!(["z.rs"])
    );
}
#[test]
fn search_grep_binary_size_utf8_and_hardlink_skip_counts() {
    let root = TempDir::new().unwrap();
    write(&root, "binary", b"needle\0");
    write(&root, "utf8", b"needle\xff");
    write(&root, "good", b"needle");
    write(&root, "linked", b"needle");
    fs::hard_link(root.path().join("linked"), root.path().join("alias")).unwrap();
    fs::File::create(root.path().join("large"))
        .unwrap()
        .set_len(1048577)
        .unwrap();
    let view = workspace(&root);
    let result = run(&view, "grep", json!({"pattern":"needle"}));
    assert_eq!(result["count"], 1);
    assert_eq!(
        result["skipped"],
        json!({"binary":1,"non_utf8":1,"oversized":1,"unreadable":2})
    );
}
#[test]
fn search_grep_result_limits_defaults_maximum_and_file_count() {
    let root = TempDir::new().unwrap();
    write(&root, "many", "needle\n".repeat(2001));
    let view = workspace(&root);
    for (args, count) in [
        (json!({"pattern":"needle"}), 200),
        (json!({"pattern":"needle","max_results":2000}), 2000),
    ] {
        let result = run(&view, "grep", args);
        assert_eq!(result["count"], count);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["limits"], json!(["max_results:matches"]));
    }
    write(&root, "other", b"needle");
    let result = run(
        &view,
        "grep",
        json!({"pattern":"needle","output":"files","max_results":1}),
    );
    assert_eq!(result["paths"], json!(["many"]));
    assert_eq!(result["limits"], json!(["max_results:files"]));
    write(&root, "many", "needle\n".repeat(200));
    fs::remove_file(root.path().join("other")).unwrap();
    assert_eq!(
        run(&view, "grep", json!({"pattern":"needle"}))["truncated"],
        false
    );
}
#[test]
fn search_grep_line_limit_is_utf8_safe_and_marked_in_context() {
    let root = TempDir::new().unwrap();
    write(
        &root,
        "text",
        format!("{}\nneedle\n{}", "é".repeat(251), "x".repeat(501)),
    );
    let view = workspace(&root);
    let result = run(&view, "grep", json!({"pattern":"needle","context_lines":1}));
    let row = &result["matches"][0];
    assert_eq!(row["before"][0]["text"].as_str().unwrap().len(), 511);
    assert_eq!(row["before"][0]["text_truncated"], true);
    assert!(
        row["after"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("[truncated]")
    );
    assert_eq!(result["limits"], json!(["line_bytes:500"]));
}
#[test]
fn search_grep_scan_budget_never_exceeds_64_mib() {
    let root = TempDir::new().unwrap();
    let data = vec![b'a'; 1048576];
    for n in 0..65 {
        write(&root, &format!("f{n:02}"), &data);
    }
    let view = workspace(&root);
    let result = run(
        &view,
        "grep",
        json!({"pattern":"never","include_ignored":true}),
    );
    assert_eq!(result["scanned_bytes"], 67108864u64);
    assert_eq!(result["count"], 0);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["limits"], json!(["scanned_bytes:67108864"]));
}
#[test]
fn search_grep_catastrophic_shaped_pattern_is_bounded() {
    let root = TempDir::new().unwrap();
    write(&root, "adversarial", format!("{}!", "a".repeat(900000)));
    let view = workspace(&root);
    let started = Instant::now();
    assert_eq!(run(&view, "grep", json!({"pattern":"(a+)+$"}))["count"], 0);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "pure-Rust regex must not backtrack exponentially"
    );
}
#[test]
fn search_capture_is_bounded_and_retains_complete_large_output() {
    let root = TempDir::new().unwrap();
    write(
        &root,
        "many",
        format!("{}needle\n", "x".repeat(480)).repeat(200),
    );
    let view = workspace(&root);
    let outcome = view.execute(
        &plan(&view, "grep", json!({"pattern":"needle"})),
        &CancelToken::new(),
    );
    assert_eq!(outcome.artifacts.len(), 1);
    assert_eq!(full(&outcome)["count"], 200);
    let small = inline(&outcome);
    assert_eq!(small["captured"], true);
    assert_eq!(small["truncated"], true);
    assert!(small["inline_omitted"].as_u64().unwrap() > 0);
    assert!(outcome.output_truncated);
}
#[test]
fn search_cancellation_and_deadline_report_partial_truncation() {
    let root = TempDir::new().unwrap();
    write(&root, "a", b"needle");
    let view = workspace(&root);
    for (name, args) in [
        ("list", json!({})),
        ("glob", json!({"pattern":"*"})),
        ("grep", json!({"pattern":"needle"})),
    ] {
        let plan = plan(&view, name, args);
        let token = CancelToken::new();
        token.cancel();
        let outcome = view.execute(&plan, &token);
        assert_eq!(outcome.code, Some(ResultCode::Cancelled));
        assert_eq!(inline(&outcome)["limits"], json!(["cancelled"]));
        assert!(outcome.output_truncated);
        let outcome = view.execute_hooked(
            &plan,
            &CancelToken::new(),
            &mut |_| Ok(()),
            Some(Duration::ZERO),
            None,
        );
        assert_eq!(outcome.code, Some(ResultCode::Timeout));
        assert_eq!(inline(&outcome)["truncated"], true);
    }
}
#[test]
fn search_direct_grep_file_honours_ignore_rules_and_opt_out() {
    let root = TempDir::new().unwrap();
    write(&root, ".gitignore", b"drop.rs\n");
    write(&root, "drop.rs", b"needle");
    write(&root, "keep.rs", b"needle");
    let view = workspace(&root);
    let ignored = run(&view, "grep", json!({"path":"drop.rs","pattern":"needle"}));
    assert_eq!(ignored["count"], 0);
    assert_eq!(ignored["truncated"], false);
    assert_eq!(
        run(
            &view,
            "grep",
            json!({"path":"drop.rs","pattern":"needle","include_ignored":true})
        )["count"],
        1
    );
    assert_eq!(
        run(&view, "grep", json!({"path":"keep.rs","pattern":"needle"}))["count"],
        1
    );
}
#[test]
fn search_protected_gitignore_is_absent_without_warning_or_skip_count() {
    let root = TempDir::new().unwrap();
    write(&root, ".gitignore", b"a.rs\n");
    write(&root, "a.rs", b"needle");
    let view = FsWorkspace::open(
        root.path(),
        &FsProtectedPaths {
            configured: vec![path(".gitignore")],
            harness_data_root: None,
        },
    )
    .unwrap();
    let result = run(&view, "glob", json!({"pattern":"*.rs"}));
    assert_eq!(result["paths"], json!(["a.rs"]));
    assert_eq!(result["warnings"], json!([]));
    assert_eq!(result["skipped"]["unreadable"], 0);
}
#[test]
fn search_gitignore_utf8_bom_does_not_disable_the_first_rule() {
    let root = TempDir::new().unwrap();
    write(&root, ".gitignore", b"\xef\xbb\xbfdrop.rs\n");
    write(&root, "drop.rs", b"needle");
    write(&root, "keep.rs", b"needle");
    let view = workspace(&root);
    assert_eq!(
        run(&view, "glob", json!({"pattern":"*.rs"}))["paths"],
        json!(["keep.rs"])
    );
}
