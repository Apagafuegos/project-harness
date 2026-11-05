//! Shared provider, model and durable session assembly for headless and terminal clients.
#[cfg(test)]
#[path = "wiring_instructions_tests.rs"]
mod instruction_tests;
use harness_core::{
    context::{CompactionConfig, ModelCatalog, PreviewReport, StrategyId},
    model::ModelProvider,
    runtime::*,
    session::*,
    tools::{
        FsProtectedPaths, FsWorkspace, PolicySnapshot, Registry, SandboxConfig, WorkspacePath,
        WorkspaceView,
    },
};
use harness_providers::{
    anthropic::{self, AnthropicMessages, Profile as MessagesProfile},
    openai::{self, OpenAiResponses, Profile},
    router::Router,
};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

/// Compaction strategy recorded in the creation configuration of every newly
/// created session, for both clients.
///
/// This is a **client creation default only**. `StrategyId::default()` and
/// `CompactionConfig`'s decoding keep reading an omitted strategy as the legacy
/// `rolling_summary`, so an old journal is never reinterpreted by a new session's
/// default. A continuation without an explicit selection keeps the strategy that
/// is already recorded. See the m6-07 amendment to ADR 0026.
pub const fn creation_compaction_strategy() -> StrategyId {
    StrategyId::ObservationMasking
}

/// Stable name of [`creation_compaction_strategy`] for the shared parser flag.
pub const CREATION_COMPACTION_STRATEGY: &str = creation_compaction_strategy().as_str();

/// Session assembly choices, independent of client input and rendering.
#[derive(Clone, Debug)]
pub struct SessionOptions {
    /// Absolute private storage root, outside the workspace.
    pub data_root: PathBuf,
    /// Explicitly authorized workspace.
    pub workspace: PathBuf,
    /// Continue this session, or create a new one.
    pub session: Option<SessionId>,
    /// Model for a new session; an explicit mismatch on continuation is refused.
    pub model: ModelRef,
    /// Whether the user supplied a model override.
    pub explicit_model: bool,
    /// Whether the user supplied a provider override.
    pub explicit_provider: bool,
    /// Explicit policy for a mid-session switch, applied only when the requested
    /// provider or model differs from the branch's current one.
    pub switch_policy: Portability,
    /// Provider instances the injected provider can actually serve, for an
    /// early refusal instead of a failed inference. Empty disables validation.
    pub known_instances: Vec<String>,
    /// Independent summary model, enabling automatic compaction only when set.
    pub summary_model: Option<ModelRef>,
    /// Compaction strategy recorded in a new session's configuration. Clients
    /// default it to [`creation_compaction_strategy`]; a continuation keeps the
    /// recorded strategy unless the user names a different one.
    pub compaction_strategy: StrategyId,
    /// Whether the user supplied a strategy override. A continuation refuses an
    /// explicit strategy that differs from the recorded one with
    /// `session_policy_mismatch`, exactly as an explicit summary-model change.
    pub explicit_strategy: bool,
    /// Runtime defaults and sourced catalog. No caller-installed tool host.
    pub driver: DriverConfig,
    /// Real host assembly settings (tests can disable the shell).
    pub sandbox: Option<SandboxConfig>,
    /// Whether a continuation resumes the remembered grants the session already
    /// recorded. The TUI records them and resumes them; `ask` never inherits
    /// them (audit L06) and refuses with `session_has_remembered_grants`.
    pub inherit_remembered_grants: bool,
}
/// Sanitized client-facing failure: process classification and stable diagnostic.
pub type Failure = (u8, String);
fn configuration(code: impl Into<String>) -> Failure {
    (2, code.into())
}
fn storage(error: PersistError) -> Failure {
    configuration(error.to_string())
}
/// Resolve the default outside the workspace: XDG_DATA_HOME/harness or ~/.local/share/harness.
pub fn default_data_root() -> Result<PathBuf, &'static str> {
    let base = if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(path)
    } else {
        PathBuf::from(std::env::var_os("HOME").ok_or("data_root_unconfigured")?)
            .join(".local/share")
    };
    if !base.is_absolute() {
        return Err("data_root_not_absolute");
    }
    Ok(base.join("harness"))
}

/// Opened session and nonfatal creation diagnostics.
pub struct WiredSession {
    /// Runtime handle; the caller must shut it down on every exit path.
    pub handle: SessionHandle,
    /// New or continued durable identity.
    pub session: SessionId,
    /// User/project instruction warnings, absent on continuation.
    pub warnings: Vec<String>,
}

/// Create or open with an injected provider and the real policy-owned tool host.
pub async fn open_session(
    o: &SessionOptions,
    provider: Arc<dyn ModelProvider>,
) -> Result<WiredSession, Failure> {
    if !o.workspace.is_absolute() || !o.workspace.is_dir() || !o.data_root.is_absolute() {
        return Err(configuration("invalid_workspace_or_data_root"));
    }
    let mut warnings = Vec::new();
    if let Some(parent) = o.data_root.parent() {
        std::fs::create_dir_all(parent).map_err(|_| configuration("invalid_data_root_parent"))?;
    }
    let root = Arc::new(
        DataRoot::open(&o.data_root, true).map_err(|_| configuration("invalid_data_root"))?,
    );
    let session = o.session.clone().unwrap_or_default();
    let host = ToolHostConfig {
        workspace_root: o.workspace.clone(),
        data_root: root.path().to_owned(),
        sandbox: o.sandbox.clone(),
    };
    let handle = if o.session.is_some() {
        // Refuse imported remembered grants: this client never auto-approves mutations.
        let inspection =
            JournalStore::inspect(&root, &session).map_err(|e| configuration(e.to_string()))?;
        if inspection.diagnostic.is_some() {
            return Err(configuration("journal_requires_explicit_repair"));
        }
        let snapshot = inspection
            .snapshot
            .ok_or_else(|| configuration("missing_session_header"))?;
        let bytes = root
            .read_artifact(snapshot.configuration(), 16 * 1024 * 1024)
            .map_err(|_| configuration("configuration_unreadable"))?;
        let policy: Value =
            serde_json::from_slice(&bytes).map_err(|_| configuration("invalid_configuration"))?;
        let expected = configured_policy(o);
        for field in [
            "workspace_read",
            "workspace_mutation",
            "process",
            "network",
            "sandbox",
        ] {
            if policy[field] != expected[field] {
                return Err(configuration("session_policy_mismatch"));
            }
        }
        // This client never inherits remembered grants (audit L06). Say so
        // instead of the generic policy mismatch: the session is intact, and the
        // TUI, which recorded the grants, resumes them.
        if !o.inherit_remembered_grants {
            if policy["allowances"]
                .as_array()
                .is_some_and(|grants| !grants.is_empty())
            {
                return Err(configuration(
                    "session_has_remembered_grants: ask never uses remembered grants; \
                     continue this session in the TUI",
                ));
            }
            if policy["allowances"] != expected["allowances"] {
                return Err(configuration("session_policy_mismatch"));
            }
        }
        // The TUI resumes exactly the allowances the session recorded: it is the
        // client that granted them, and a grant never widens past the protected
        // paths or the recorded tool/path or exact command/cwd binding.
        // The compaction policy is compared semantically, so an explicit default
        // is not a mismatch and an explicit strategy is refused only when it
        // differs from the recorded one.
        let recorded = CompactionConfig::from_value(policy.get("compaction"))
            .map_err(|_| configuration("invalid_configuration"))?;
        continuation_compaction(&recorded, &expected, o.explicit_strategy)?;
        let head_model = snapshot
            .model_at(snapshot.head())
            .map_err(|_| configuration("unknown_head"))?
            .clone();
        // A continuation keeps the branch's model unless the user asked for
        // another provider or model. Asking for one is an explicit switch, never
        // an implicit rewrite: the policy decides whether optional native state
        // may be dropped, and a refusal writes nothing.
        let switch =
            (o.explicit_provider || o.explicit_model).then(|| (o.model.clone(), o.switch_policy));
        known(o, switch.as_ref().map_or(&head_model, |(model, _)| model))?;
        SessionHandle::open_with_tools(root, session.clone(), provider, o.driver.clone(), &host)
            .await
            .map_err(storage)?
    } else {
        known(o, &o.model)?;
        let policy = configured_policy(o);
        let policy_bytes =
            serde_json::to_vec(&policy).map_err(|_| configuration("invalid_configuration"))?;
        let parsed = PolicySnapshot::parse(
            ArtifactRef {
                sha256: Digest::of(&policy_bytes),
                bytes: policy_bytes.len() as u64,
            },
            &policy_bytes,
        )
        .map_err(|_| configuration("invalid_configuration"))?;
        let (preflight, availability) = ToolHost::assemble(&host, &parsed);
        drop(preflight);
        let (configuration_json, notices) = session_configuration(o, &availability)?;
        warnings = notices;
        let bytes = serde_json::to_vec(&configuration_json)
            .map_err(|_| configuration("invalid_configuration"))?;
        let header = SessionHeader {
            workspace: o
                .workspace
                .to_str()
                .ok_or_else(|| configuration("workspace_not_utf8"))?
                .into(),
            initial_model: o.model.clone(),
            policy_revision: "ask-v1".into(),
            configuration: root
                .put_artifact(&bytes)
                .map_err(|_| configuration("configuration_publish_failed"))?,
        };
        let handle = SessionHandle::create_with_tools(
            root,
            session.clone(),
            header,
            provider,
            o.driver.clone(),
            &host,
        )
        .await
        .map_err(storage)?;
        if handle
            .tool_availability()
            .is_none_or(|actual| available_tools(actual) != available_tools(&availability))
        {
            handle.shutdown().await;
            return Err(configuration("tool_availability_changed"));
        }
        handle
    };

    Ok(WiredSession {
        handle,
        session,
        warnings,
    })
}
/// The compaction policy a continuation is allowed to continue with.
///
/// Without an explicit strategy selection the **recorded** strategy wins, even
/// though new sessions now default to `observation_masking`. That includes the
/// legacy reading of a configuration that omits the field entirely
/// (`CompactionConfig::from_value(None)` is `rolling_summary`), so reopening old
/// work never adds a strategy or rewrites its configuration. Every other field,
/// including the summary model and `automatic`, must still match exactly; an
/// explicit differing selection is refused by the caller's mismatch policy.
fn continuation_compaction(
    recorded: &CompactionConfig,
    expected: &Value,
    explicit_strategy: bool,
) -> Result<(), Failure> {
    let mut requested = CompactionConfig::from_value(expected.get("compaction"))
        .map_err(|_| configuration("invalid_configuration"))?;
    if !explicit_strategy {
        requested.strategy = recorded.strategy;
    }
    if recorded != &requested {
        return Err(configuration("session_policy_mismatch"));
    }
    Ok(())
}

/// Refuse a model whose provider instance the injected provider cannot serve,
/// before anything is opened or written. An empty registry is not validated.
fn known(o: &SessionOptions, model: &ModelRef) -> Result<(), Failure> {
    if o.known_instances.is_empty()
        || o.known_instances
            .iter()
            .any(|instance| instance == &model.provider_instance)
    {
        return Ok(());
    }
    if let Some(summary) = &o.summary_model
        && !o
            .known_instances
            .iter()
            .any(|instance| instance == &summary.provider_instance)
    {
        return Err(configuration("provider_not_configured"));
    }
    Err(configuration("provider_not_configured"))
}

/// The switch a continuation asks for, if the user named a provider or model.
///
/// Without an override the branch keeps its model. With one, the requested
/// model is applied through `SwitchModel`, whose policy decides whether
/// optional native state may be dropped. An identical request is not a switch.
pub fn continuation_switch(o: &SessionOptions) -> Option<(ModelRef, Portability)> {
    if o.session.is_none() || !(o.explicit_provider || o.explicit_model) {
        return None;
    }
    Some((o.model.clone(), o.switch_policy))
}

fn policy_json(summary: Option<&ModelRef>, strategy: StrategyId) -> Value {
    json!({"policy_revision":"ask-v1", "enabled_tools":["read","write","edit","bash","list","glob","grep"],
        "workspace_read":"ask", "workspace_mutation":"ask", "process":"ask", "network":"deny",
        "protected_paths":[], "allowances":[],
        "compaction":{"strategy":strategy.as_str(),"summary_model":summary,"automatic":summary.is_some()}})
}

/// Read-only approval summary from the exact bound plan, never live config.
/// Only environment key names are rendered.
pub fn sandbox_summary(plan: &[u8]) -> Vec<String> {
    let Ok(value) = serde_json::from_slice::<Value>(plan) else {
        return Vec::new();
    };
    if value["name"] != "bash" {
        return Vec::new();
    }
    let Some(sandbox) = value.get("sandbox") else {
        return Vec::new();
    };
    let Ok(settings) =
        serde_json::from_value::<harness_core::tools::sandbox::ToolchainSettings>(sandbox.clone())
    else {
        return Vec::new();
    };
    let mut rows = vec!["sandbox: network denied; private per-command caches".into()];
    rows.extend(
        settings
            .toolchain_paths
            .iter()
            .map(|p| format!("toolchain (read-only): {}", p.display())),
    );
    rows.push(format!("sandbox PATH: {}", settings.path()));
    rows.push(format!(
        "sandbox env keys: {}",
        settings.env.keys().cloned().collect::<Vec<_>>().join(", ")
    ));
    rows
}

fn configured_policy(o: &SessionOptions) -> Value {
    let mut policy = policy_json(o.summary_model.as_ref(), o.compaction_strategy);
    if let Some(sandbox) = &o.sandbox
        && !sandbox.toolchains.is_empty()
    {
        policy["sandbox"] = serde_json::to_value(&sandbox.toolchains).expect("sandbox settings");
    }
    policy
}

/// Apply an explicit continuation switch after recovery and before starting a turn.
/// Identical models do not append a marker; refusals retain their full preview.
pub async fn switch_session(
    handle: &SessionHandle,
    o: &SessionOptions,
) -> Result<Option<Box<PreviewReport>>, Rejection> {
    let view = handle.view().await.map_err(|_| Rejection::Faulted)?;
    let current = view
        .snapshot
        .model_at(view.snapshot.head())
        .map_err(|_| Rejection::Invalid("unknown_head"))?;
    let Some((model, policy)) = continuation_switch(o).filter(|(model, _)| model != current) else {
        return Ok(None);
    };
    match handle
        .command(Command::SwitchModel { model, policy })
        .await?
    {
        Accepted::ModelSwitched { preview, .. } => Ok(Some(preview)),
        _ => Err(Rejection::Invalid("unexpected_switch_reply")),
    }
}

const AGENTS_CAP: u64 = 64 * 1024;
/// Names come from the actual built-in registry filtered by backend availability.
fn available_tools(availability: &ToolAvailability) -> Vec<String> {
    Registry::builtin()
        .definitions()
        .filter(|d| match d.backend {
            ExecutionBackend::TrustedFs => availability.file_tools.is_available(),
            ExecutionBackend::Bubblewrap => availability.shell.is_available(),
        })
        .map(|d| d.name.clone())
        .collect()
}

fn session_configuration(
    o: &SessionOptions,
    availability: &ToolAvailability,
) -> Result<(Value, Vec<String>), Failure> {
    let mut value = configured_policy(o);
    let tools = available_tools(availability);
    let search = |name: &str| tools.iter().any(|tool| tool == name);
    let search_hint = if search("list") && search("glob") && search("grep") {
        if search("bash") {
            "Prefer list, glob and grep over bash for finding files and text."
        } else {
            "Prefer list, glob and grep for finding files and text."
        }
    } else {
        ""
    };
    let base = include_str!("base-instruction.txt")
        .replace(
            "{tools}",
            &if tools.is_empty() {
                "none".into()
            } else {
                tools.join(", ")
            },
        )
        .replace(
            "{workspace}",
            o.workspace
                .to_str()
                .ok_or_else(|| configuration("workspace_not_utf8"))?,
        )
        .replace("{search_hint}", search_hint);
    let mut instructions = vec![json!({"level":"system", "text":base})];
    let (files, warnings) = instruction_files(
        &o.workspace,
        user_config_directory()
            .ok()
            .map(|p| p.join("AGENTS.md"))
            .as_deref(),
    );
    instructions.extend(files);
    value["instructions"] = Value::Array(instructions);
    Ok((value, warnings))
}

/// User-owned configuration directory, never selected by a workspace file.
pub fn user_config_directory() -> Result<PathBuf, &'static str> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
        .ok_or("config_path_unconfigured")?;
    if !base.is_absolute() {
        return Err("config_path_not_absolute");
    }
    Ok(base.join("harness"))
}

fn user_instructions(path: &std::path::Path) -> Result<Option<String>, String> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("unavailable".into()),
    };
    if !file.metadata().map_err(|_| "unavailable")?.is_file() {
        return Err("not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(AGENTS_CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "unavailable")?;
    if bytes.len() as u64 > AGENTS_CAP {
        return Err("exceeds 64 KiB cap".into());
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "not valid UTF-8".into())
}

fn instruction_files(
    workspace: &std::path::Path,
    user: Option<&std::path::Path>,
) -> (Vec<Value>, Vec<String>) {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    let mut total = 0;
    let mut total_warned = false;
    let mut append =
        |result: Result<Option<String>, String>, label: &str, frame: String| match result {
            Ok(Some(text)) => {
                if total + text.len() > 256 * 1024 {
                    if !total_warned {
                        warnings.push("instructions skipped: exceeds 256 KiB total cap".into());
                        total_warned = true;
                    }
                } else {
                    total += text.len();
                    files.push(json!({"level":"developer", "text":format!("{frame}:\n{text}")}));
                }
            }
            Ok(None) => {}
            Err(reason) => warnings.push(format!("{label} skipped: {reason}")),
        };
    if let Some(user) = user {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let display = home
            .as_ref()
            .and_then(|home| user.strip_prefix(home).ok())
            .map_or_else(
                || user.display().to_string(),
                |p| format!("~/{}", p.display()),
            );
        append(
            user_instructions(user),
            "User AGENTS.md",
            format!("User instructions from {display}"),
        );
    }
    let workspace = match FsWorkspace::open(workspace, &FsProtectedPaths::default()) {
        Ok(workspace) => workspace,
        Err(_) => {
            warnings.push("AGENTS.md skipped: descriptor-rooted workspace unavailable".into());
            return (files, warnings);
        }
    };
    append(
        project_instructions(&workspace, "AGENTS.md"),
        "AGENTS.md",
        "Project instructions from AGENTS.md".into(),
    );
    let (paths, notices) = nested_directories(&workspace);
    for relative in paths {
        let name = format!("{relative}/AGENTS.md");
        append(
            project_instructions(&workspace, &name),
            &name,
            format!("Project instructions from {name} (apply to files under {relative}/)"),
        );
    }
    warnings.extend(notices);
    (files, warnings)
}

fn nested_directories(workspace: &FsWorkspace) -> (Vec<String>, Vec<String>) {
    use std::collections::BTreeSet;
    let mut pending = BTreeSet::from([String::new()]);
    let mut visited = Vec::new();
    let mut depth_limited = false;
    let mut directory_limited = false;
    let mut unsafe_read = false;
    while let Some(relative) = pending.pop_first() {
        let depth = if relative.is_empty() {
            0
        } else {
            relative.split('/').count()
        };
        let path = (!relative.is_empty())
            .then(|| WorkspacePath::new(&relative).expect("validated directory"));
        let remaining = 4000usize.saturating_sub(visited.len() + pending.len() + 1);
        match workspace.list_directories(
            path.as_ref(),
            if depth == 8 { 0 } else { remaining },
            |name| {
                !name.starts_with('.')
                    && !matches!(
                        name,
                        "target" | "node_modules" | "dist" | "build" | "vendor"
                    )
            },
        ) {
            Ok((children, limited)) => {
                if limited {
                    if depth == 8 {
                        depth_limited = true;
                    } else {
                        directory_limited = true;
                    }
                }
                pending.extend(children.into_iter().map(|p| p.as_str().to_owned()));
            }
            Err(_) => unsafe_read = true,
        }
        visited.push(relative);
    }
    visited.retain(|p| !p.is_empty());
    // Sort complete instruction paths, not just directory prefixes: e.g.
    // a-b/AGENTS.md sorts before a/AGENTS.md, despite "a" preceding "a-b".
    visited.sort_by_cached_key(|relative| format!("{relative}/AGENTS.md"));
    let mut warnings = Vec::new();
    if depth_limited {
        warnings.push("AGENTS.md discovery stopped: depth 8 limit".into());
    }
    if directory_limited {
        warnings.push("AGENTS.md discovery stopped: 4,000 directories limit".into());
    }
    if unsafe_read {
        warnings
            .push("AGENTS.md discovery skipped: descriptor-rooted directory read refused".into());
    }
    (visited, warnings)
}

fn project_instructions(workspace: &FsWorkspace, name: &str) -> Result<Option<String>, String> {
    let path = WorkspacePath::new(name).expect("discovered workspace-relative path");
    // read_text is bounded before allocation, refuses symlinks and validates UTF-8.
    match workspace.read_text(&path, AGENTS_CAP) {
        Ok(text) => Ok(Some(text)),
        Err(error) => match error.code {
            ResultCode::NotFound => Ok(None),
            ResultCode::TooLarge => Err("exceeds 64 KiB cap".into()),
            ResultCode::NotUtf8 => Err("not valid UTF-8".into()),
            _ => Err(format!(
                "descriptor-rooted read refused ({})",
                error.code.as_str()
            )),
        },
    }
}

/// Provider instance for the Anthropic Messages adapter. One account, one owner of
/// native state.
pub const ANTHROPIC: &str = "anthropic";
/// DeepSeek's Anthropic-compatible Messages endpoint, `DEEPSEEK_API_KEY`. It is
/// its own instance because it is its own account and its own native-state
/// owner: thinking written by one instance is never replayed to the other.
pub const DEEPSEEK: &str = "deepseek";
/// OpenAI's two authentication profiles are separate instances: subscription
/// state must never be replayed to the API-key endpoint, or the reverse.
pub const CODEX: &str = "openai-codex";
/// Platform API-key native-state owner.
pub const API_KEY: &str = "openai-api-key";

/// Sanitized provider setup failure; adapter messages and credentials are never retained.
#[derive(Debug)]
pub struct ProviderSetupError {
    /// Process exit classification.
    pub code: u8,
    /// Stable content-free diagnostic.
    pub diagnostic: &'static str,
    /// Whether the client should suggest `codex login`.
    pub codex_hint: bool,
}
fn skip_or_fail<T>(
    result: Result<T, ModelError>,
    selected: bool,
    codex: bool,
) -> Result<Option<T>, ProviderSetupError> {
    match result {
        Ok(adapter) => Ok(Some(adapter)),
        Err(_) if !selected => Ok(None),
        Err(error) => {
            let auth = error.code == ModelErrorCode::Authentication;
            Err(ProviderSetupError {
                code: if auth { 3 } else { 2 },
                diagnostic: if auth {
                    "authentication"
                } else {
                    "provider_setup_failed"
                },
                codex_hint: auth && codex,
            })
        }
    }
}
/// Router plus sourced model metadata for every credential available to this process.
pub struct Providers {
    /// Inject this provider into shared session assembly.
    pub provider: Arc<dyn ModelProvider>,
    /// Provider instances that the router can serve.
    pub instances: Vec<String>,
    /// Runtime defaults with the union of the registered providers' catalogs.
    pub driver: DriverConfig,
}
/// Construct real adapters; only a selected instance's failure is fatal.
pub fn providers(selected: &str, codex_hint: bool) -> Result<Providers, ProviderSetupError> {
    // Every instance whose credential exists is registered, so a session can
    // switch between providers mid-flight. The selected one is required.
    let mut router = Router::new();
    let mut catalog = ModelCatalog::new();
    for (instance, profile, result) in [
        (CODEX, Profile::ChatGptCodex, OpenAiResponses::codex_login()),
        (
            API_KEY,
            Profile::Platform,
            OpenAiResponses::platform_from_env(),
        ),
    ] {
        if let Some(adapter) = skip_or_fail(result, selected == instance, codex_hint)? {
            openai::extend_catalog(&mut catalog, profile, instance);
            router.register(instance, Arc::new(adapter));
        }
    }
    match skip_or_fail(
        AnthropicMessages::from_env(),
        selected == ANTHROPIC,
        codex_hint,
    ) {
        Ok(Some(adapter)) => {
            anthropic::extend_catalog(&mut catalog, MessagesProfile::Anthropic, ANTHROPIC);
            router.register(ANTHROPIC, Arc::new(adapter));
        }
        Ok(None) => {}
        Err(code) => return Err(code),
    }
    match skip_or_fail(
        AnthropicMessages::deepseek_from_env(),
        selected == DEEPSEEK,
        codex_hint,
    ) {
        Ok(Some(adapter)) => {
            anthropic::extend_catalog(&mut catalog, MessagesProfile::DeepSeek, DEEPSEEK);
            router.register(DEEPSEEK, Arc::new(adapter));
        }
        Ok(None) => {}
        Err(code) => return Err(code),
    }

    let instances = router.instances().map(str::to_owned).collect();
    let mut driver = DriverConfig::default();
    driver.runtime.context.catalog = catalog;
    Ok(Providers {
        provider: Arc::new(router),
        instances,
        driver,
    })
}
/// Authentication profiles have separate native-state owners.
pub fn provider_instance(provider: &str, codex: bool) -> &'static str {
    match provider {
        "anthropic" => ANTHROPIC,
        "deepseek" => DEEPSEEK,
        _ if codex => CODEX,
        _ => API_KEY,
    }
}
/// Whether `--auth` selects this provider's authentication.
///
/// Only the OpenAI instances take it; the Messages profiles each read their own
/// API-key variable, so an explicit `--auth` with them is a usage error rather
/// than a silently ignored flag.
pub fn openai_auth_applies(provider: &str) -> bool {
    provider == "openai"
}
/// New-session defaults; never silently applied to a continued session.
pub fn default_model(provider: &str, codex: bool) -> &'static str {
    match (provider, codex) {
        ("anthropic", _) => "claude-sonnet-5-5",
        ("deepseek", _) => anthropic::DEEPSEEK_LIVE_CHECK_MODEL,
        (_, true) => openai::LIVE_CHECK_MODEL,
        _ => "gpt-5.5",
    }
}
