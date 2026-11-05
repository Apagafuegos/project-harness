use super::*;

#[test]
fn sandbox_env_names_are_refused_without_values() {
    for key in [
        "PATH",
        "HOME",
        "TMPDIR",
        "LANG",
        "TERM",
        "CARGO_HOME",
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_JOBS",
        "LD_PRELOAD",
        "OPENAI_BASE_URL",
        "ANTHROPIC_API_KEY",
        "DEEPSEEK_API_KEY",
        "CODEX_HOME",
        "api_key",
        "token",
        "SeCrEt",
        "password",
        "custom_key",
        "BASH_ENV",
        "bad=name",
        "1BAD",
    ] {
        let settings = ToolchainSettings {
            env: [(key.into(), "ENV_VALUE_SENTINEL".into())].into(),
            ..Default::default()
        };
        let error = settings.validate().unwrap_err();
        assert!(error.contains(key), "missing key diagnostic");
        assert!(!error.contains("ENV_VALUE_SENTINEL"));
    }
    ToolchainSettings {
        env: [("RUSTUP_HOME".into(), "/tools/rustup".into())].into(),
        ..Default::default()
    }
    .validate()
    .unwrap();
}

#[test]
fn sandbox_paths_refuse_invalid_spellings_duplicates_and_unmounted_prepends() {
    for path in [
        "relative",
        "/tools/",
        "/tools/../private",
        "/tools/./bin",
        "/tools//bin",
    ] {
        let settings = ToolchainSettings {
            toolchain_paths: vec![path.into()],
            ..Default::default()
        };
        let error = settings.validate().unwrap_err();
        assert!(error.contains(path), "{error}");
    }
    let mut settings = ToolchainSettings {
        toolchain_paths: vec!["/opt/tools".into(), "/opt/tools".into()],
        ..Default::default()
    };
    assert!(settings.validate().unwrap_err().contains("duplicate"));
    settings.toolchain_paths.pop();
    for path in ["/opt/tools-other/bin", "/bin", "/unmounted"] {
        settings.path_prepend = vec![path.into()];
        assert!(settings.validate().unwrap_err().contains(path));
    }
    settings.path_prepend = vec!["/opt/tools/bin".into(), "/usr/local/bin".into()];
    settings.validate().unwrap();
    assert_eq!(
        settings.path(),
        "/opt/tools/bin:/usr/local/bin:/usr/bin:/bin"
    );
}

#[test]
fn sandbox_location_refusals_cover_roots_ancestors_descendants_and_aliases() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let home = root.join("home");
    let workspace = home.join("projects/workspace");
    let data = home.join("state/harness");
    let config = home.join("xdg/harness");
    let allowed = home.join(".cargo");
    for p in [&workspace, &data, &config, &allowed] {
        fs::create_dir_all(p).unwrap();
    }
    let validate = |path: PathBuf| {
        ToolchainSettings {
            toolchain_paths: vec![path],
            ..Default::default()
        }
        .validate_locations(&workspace, &data, &home, std::slice::from_ref(&config))
    };
    validate(allowed.clone()).unwrap();
    for path in [
        PathBuf::from("/"),
        home.clone(),
        root.into(),
        workspace.clone(),
        workspace.parent().unwrap().into(),
        workspace.join("child"),
        data.clone(),
        data.parent().unwrap().into(),
        data.join("child"),
        config.clone(),
        config.join("child"),
        home.join(".ssh"),
        home.join(".gnupg"),
        home.join(".codex"),
        home.join(".aws"),
        home.join(".config"),
        home.join(".config/child"),
    ] {
        fs::create_dir_all(&path).unwrap();
        let error = validate(path.clone()).unwrap_err();
        assert!(error.contains(path.to_str().unwrap()), "{error}");
    }
    let file = home.join("file");
    fs::write(&file, "file").unwrap();
    assert!(validate(file).is_err());
    assert!(validate(home.join("missing")).is_err());
    let alias = home.join("alias");
    std::os::unix::fs::symlink(&allowed, &alias).unwrap();
    assert!(validate(alias).is_err());
    std::os::unix::fs::symlink(&workspace, allowed.join("escape")).unwrap();
    let settings = ToolchainSettings {
        toolchain_paths: vec![allowed.clone()],
        path_prepend: vec![allowed.join("escape")],
        ..Default::default()
    };
    assert!(
        settings
            .validate_locations(&workspace, &data, &home, std::slice::from_ref(&config))
            .is_err()
    );
}

#[test]
fn cargo_registry_uses_the_same_refusal_set_and_binds_inside_cargo_home() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let home = root.join("home");
    let workspace = home.join("projects/workspace");
    let data = home.join("state/harness");
    let config = home.join("xdg/harness");
    let registry = home.join(".cargo/registry");
    for p in [&workspace, &data, &config, &registry] {
        fs::create_dir_all(p).unwrap();
    }
    let check = |value: ToolchainSettings| {
        value.validate_locations(&workspace, &data, &home, std::slice::from_ref(&config))
    };
    let settings = ToolchainSettings {
        cargo_registry: Some(registry.clone()),
        ..Default::default()
    };
    check(settings.clone()).unwrap();
    assert_eq!(
        settings.mounts(),
        vec![(registry.as_path(), Path::new(CARGO_REGISTRY_DEST))]
    );
    assert_eq!(
        settings.report()["cargo_registry"]["sandbox_path"],
        CARGO_REGISTRY_DEST
    );
    assert!(!settings.is_empty());
    for path in [
        PathBuf::from("/"),
        home.clone(),
        workspace.clone(),
        workspace.join("child"),
        data.clone(),
        config.clone(),
        home.join(".ssh"),
        home.join(".config"),
    ] {
        fs::create_dir_all(&path).unwrap();
        let error = check(ToolchainSettings {
            cargo_registry: Some(path.clone()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(error.contains(path.to_str().unwrap()), "{error}");
    }
    for spelling in [
        PathBuf::from("registry"),
        home.join(".cargo/registry/"),
        home.join("missing"),
    ] {
        let error = check(ToolchainSettings {
            cargo_registry: Some(spelling.clone()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(!error.is_empty());
    }
}

#[test]
fn doctor_mirrors_the_masks_args_installs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::write(root.join("credentials"), "secret").unwrap();
    fs::create_dir(root.join("credentials.toml")).unwrap();
    let settings = ToolchainSettings {
        toolchain_paths: vec![root.into()],
        ..Default::default()
    };
    let status = |name: &str| {
        settings.report()["toolchain_paths"][0]["credential_masks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|mask| mask["path"] == json!(root.join(name)))
            .unwrap()["status"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(status("credentials"), "masked");
    assert_eq!(status("credentials.toml"), "masked");
    assert_eq!(status(".git-credentials"), "absent");
    // A kind `args.rs` refuses is reported as refused, never as masked.
    std::os::unix::fs::symlink(root.join("credentials"), root.join(".git-credentials")).unwrap();
    assert_eq!(status(".git-credentials"), "refused");
}
