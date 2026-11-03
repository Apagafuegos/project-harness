use super::*;
use std::{fs, os::unix::fs::symlink};

fn write(root: &std::path::Path, name: &str, text: &str) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn user_root_nested_order_scope_and_symlinks_and_exclusions() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let user = temp.path().join("user/AGENTS.md");
    write(temp.path(), "user/AGENTS.md", "user rules");
    for name in [
        "AGENTS.md",
        "z/AGENTS.md",
        "a/AGENTS.md",
        "a/deep/AGENTS.md",
        "a-b/AGENTS.md",
        "a/A/AGENTS.md",
    ] {
        write(&root, name, name);
    }
    for name in [
        ".hidden/AGENTS.md",
        "target/AGENTS.md",
        "node_modules/AGENTS.md",
        "dist/AGENTS.md",
        "build/AGENTS.md",
        "vendor/AGENTS.md",
    ] {
        write(&root, name, "MUST-NOT-READ");
    }
    write(temp.path(), "outside/AGENTS.md", "MUST-NOT-READ");
    symlink(temp.path().join("outside"), root.join("symlink-directory")).unwrap();
    symlink("a", root.join("symlink-directory-inside")).unwrap();
    fs::create_dir(root.join("symlink-file")).unwrap();
    symlink(
        temp.path().join("outside/AGENTS.md"),
        root.join("symlink-file/AGENTS.md"),
    )
    .unwrap();
    let (files, warnings) = instruction_files(&root, Some(&user));
    assert_eq!(files.len(), 7);
    assert_eq!(
        files[0]["text"],
        format!("User instructions from {}:\nuser rules", user.display())
    );
    assert_eq!(
        files[1]["text"],
        "Project instructions from AGENTS.md:\nAGENTS.md"
    );
    for (i, relative) in ["a-b", "a/A", "a", "a/deep", "z"].iter().enumerate() {
        assert_eq!(
            files[i + 2]["text"],
            format!(
                "Project instructions from {relative}/AGENTS.md (apply to files under {relative}/):\n{relative}/AGENTS.md"
            )
        );
    }
    assert!(files.iter().all(|v| v["level"] == "developer"));
    assert!(
        !serde_json::to_string(&files)
            .unwrap()
            .contains("MUST-NOT-READ")
    );
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("symlink-file/AGENTS.md skipped: descriptor-rooted read refused"));
    let workspace = FsWorkspace::open(&root, &FsProtectedPaths::default()).unwrap();
    let (directories, _) = nested_directories(&workspace);
    assert!(
        !directories
            .iter()
            .any(|p| p.starts_with("symlink-directory")),
        "symlinked directory was walked"
    );
}

#[test]
fn depth_eight_stops_and_warns_once_without_skipping_depth_eight_file() {
    let temp = tempfile::tempdir().unwrap();
    let mut relative = String::new();
    for _ in 0..10 {
        if !relative.is_empty() {
            relative.push('/');
        }
        relative.push('a');
        write(temp.path(), &format!("{relative}/AGENTS.md"), &relative);
    }
    let (files, warnings) = instruction_files(temp.path(), None);
    assert_eq!(files.len(), 8);
    assert!(
        files[7]["text"]
            .as_str()
            .unwrap()
            .ends_with("a/a/a/a/a/a/a/a")
    );
    assert_eq!(warnings, ["AGENTS.md discovery stopped: depth 8 limit"]);
}

#[test]
fn four_thousand_directory_limit_warns_once_and_is_bounded() {
    let temp = tempfile::tempdir().unwrap();
    for i in 0..4002 {
        fs::create_dir(temp.path().join(format!("d{i:04}"))).unwrap();
    }
    let workspace = FsWorkspace::open(temp.path(), &FsProtectedPaths::default()).unwrap();
    let (directories, warnings) = nested_directories(&workspace);
    // The root is the first of the 4,000 visited directories.
    assert_eq!(directories.len(), 3999);
    assert_eq!(directories[0], "d0000");
    assert_eq!(directories[3998], "d3998");
    assert_eq!(
        warnings,
        ["AGENTS.md discovery stopped: 4,000 directories limit"]
    );
}

#[test]
fn total_cap_skips_whole_files_and_continues_to_smaller_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let user = temp.path().join("user/AGENTS.md");
    write(temp.path(), "user/AGENTS.md", &"u".repeat(65536));
    write(&root, "AGENTS.md", &"r".repeat(65536));
    write(&root, "a/AGENTS.md", &"a".repeat(65536));
    write(&root, "b/AGENTS.md", &"b".repeat(65535));
    write(&root, "c/AGENTS.md", "WHOLE-FILE-MUST-BE-SKIPPED");
    write(&root, "d/AGENTS.md", "x");
    write(&root, "e/AGENTS.md", "ALSO-SKIPPED");
    let (files, warnings) = instruction_files(&root, Some(&user));
    assert_eq!(files.len(), 5);
    assert!(files[4]["text"].as_str().unwrap().ends_with(":\nx"));
    let all = serde_json::to_string(&files).unwrap();
    assert!(!all.contains("WHOLE-FILE") && !all.contains("ALSO-SKIPPED"));
    assert_eq!(
        warnings,
        ["instructions skipped: exceeds 256 KiB total cap"]
    );
}

#[test]
fn user_invalid_and_oversized_are_content_free_and_exact_cap_is_accepted() {
    let temp = tempfile::tempdir().unwrap();
    let user = temp.path().join("user-agents");
    for (bytes, reason) in [
        (vec![0xff], "not valid UTF-8"),
        (vec![b'x'; 65537], "exceeds 64 KiB cap"),
    ] {
        fs::write(&user, bytes).unwrap();
        let (files, warnings) = instruction_files(temp.path(), Some(&user));
        assert!(files.is_empty());
        assert_eq!(warnings, [format!("User AGENTS.md skipped: {reason}")]);
    }
    fs::write(&user, vec![b'x'; 65536]).unwrap();
    let (files, warnings) = instruction_files(temp.path(), Some(&user));
    assert_eq!(files.len(), 1);
    assert!(warnings.is_empty());
}
