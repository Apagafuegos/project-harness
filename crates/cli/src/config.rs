//! User-owned defaults only: repositories never supply client configuration.
#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
use clap::{Arg, ArgAction, ArgMatches, Command};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

const CAP: u64 = 64 * 1024;
const KEYS: [&str; 8] = [
    "provider",
    "auth",
    "model",
    "summary_model",
    "compaction_strategy",
    "switch_policy",
    "data_root",
    "approve_read_only",
];

pub fn globals() -> Vec<Arg> {
    vec![
        Arg::new("config")
            .long("config")
            .value_name("PATH")
            .value_parser(clap::value_parser!(PathBuf))
            .global(true)
            .conflicts_with("no-config")
            .help("User TOML defaults (approve_read_only and switch_policy apply to ask only)"),
        Arg::new("no-config")
            .long("no-config")
            .action(ArgAction::SetTrue)
            .global(true)
            .help("Ignore the user TOML config file"),
    ]
}

pub fn command() -> Command {
    Command::new("config")
        .about("Inspect user defaults; approve_read_only and startup switch_policy are ask-only")
        .subcommand_required(true)
        .subcommand(Command::new("path").about("Print the resolved config path and existence"))
        .subcommand(
            Command::new("show")
                .about("Print resolved defaults and sources; ask-only keys do not control the TUI")
                .args(
                    crate::ask_cli::command()
                        .get_arguments()
                        .filter(|a| {
                            KEYS.iter()
                                .any(|k| k.replace('_', "-") == a.get_id().as_str())
                        })
                        .cloned(),
                ),
        )
}

#[derive(Debug)]
pub struct Settings {
    values: BTreeMap<&'static str, Option<String>>,
    sources: BTreeMap<&'static str, &'static str>,
}
impl Settings {
    pub fn text(&self, key: &str) -> Option<&str> {
        self.values[key].as_deref()
    }
    pub fn given(&self, key: &str) -> bool {
        self.sources[key] != "default"
    }
    pub fn approve_read_only(&self) -> bool {
        self.text("approve_read_only") == Some("true")
    }
    pub fn data_root(&self) -> PathBuf {
        PathBuf::from(self.text("data_root").expect("resolved"))
    }
    pub fn strategy(&self) -> harness_core::context::StrategyId {
        self.text("compaction_strategy")
            .expect("resolved")
            .parse()
            .expect("shared parser")
    }
    pub fn show(&self) -> String {
        KEYS.iter()
            .map(|key| {
                format!(
                    "{key} = {} [{}]{}",
                    self.values[key]
                        .as_deref()
                        .map(|s| serde_json::to_string(s).expect("string"))
                        .unwrap_or_else(|| "unset".into()),
                    self.sources[key],
                    if matches!(*key, "approve_read_only" | "switch_policy") {
                        " (ask only)"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub fn path(args: &ArgMatches) -> Result<PathBuf, String> {
    args.get_one::<PathBuf>("config")
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| {
            harness::wiring::user_config_directory()
                .map(|p| p.join("config.toml"))
                .map_err(str::to_owned)
        })
}

pub fn resolve(args: &ArgMatches) -> Result<Settings, String> {
    let values = if args
        .try_get_one::<bool>("no-config")
        .ok()
        .flatten()
        .copied()
        .unwrap_or(false)
    {
        BTreeMap::new()
    } else {
        let path = path(args)?;
        load(&path, args.get_one::<PathBuf>("config").is_some())?
    };
    resolve_values(args, values)
}

fn invalid(path: &Path, key: &str, reason: &str) -> String {
    format!(
        "config_invalid: {}: {}: {reason}",
        path.display().to_string().replace(['\n', '\r'], " "),
        key.escape_default()
    )
}

fn credential(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "api_key"
        || key.ends_with("_key")
        || matches!(key.as_str(), "token" | "secret" | "password")
}
fn has_credentials(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(t) => t.iter().any(|(k, v)| credential(k) || has_credentials(v)),
        toml::Value::Array(a) => a.iter().any(has_credentials),
        _ => false,
    }
}

fn load(path: &Path, explicit: bool) -> Result<BTreeMap<String, String>, String> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {
            return Ok(BTreeMap::new());
        }
        Err(_) => return Err(invalid(path, "file", "cannot read file")),
    };
    if !file
        .metadata()
        .map_err(|_| invalid(path, "file", "cannot inspect file"))?
        .is_file()
    {
        return Err(invalid(path, "file", "expected regular file"));
    }
    let mut bytes = Vec::new();
    file.take(CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid(path, "file", "cannot read file"))?;
    if bytes.len() as u64 > CAP {
        return Err(invalid(path, "file", "exceeds 64 KiB cap"));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid(path, "file", "not valid UTF-8"))?;
    let value: toml::Value = text
        .parse()
        .map_err(|_| invalid(path, "file", "invalid TOML"))?;
    if has_credentials(&value) {
        return Err("config_credential_refused: put credentials in environment variables".into());
    }
    let table = value
        .as_table()
        .ok_or_else(|| invalid(path, "file", "expected table"))?;
    let mut values = BTreeMap::new();
    for (key, value) in table {
        if !KEYS.contains(&key.as_str()) {
            return Err(invalid(path, key, "unknown key"));
        }
        let text = if key == "approve_read_only" {
            value
                .as_bool()
                .map(|b| b.to_string())
                .ok_or_else(|| invalid(path, key, "expected boolean"))?
        } else {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid(path, key, "expected string"))?
        };
        validate(path, key, &text)?;
        values.insert(key.clone(), text);
    }
    Ok(values)
}

// Validate against the actual ask flag, including its type/value parser. Never
// display clap's error: it can contain the rejected value or TOML source text.
fn validate(path: &Path, key: &str, value: &str) -> Result<(), String> {
    let id = key.replace('_', "-");
    if key == "approve_read_only" {
        return Ok(());
    }
    let arg = crate::ask_cli::command()
        .get_arguments()
        .find(|a| a.get_id().as_str() == id)
        .expect("known key")
        .clone()
        .default_value(None::<&str>);
    Command::new("config-value")
        .arg(arg)
        .try_get_matches_from(["config-value", &format!("--{id}"), value])
        .map(|_| ())
        .map_err(|_| invalid(path, key, "invalid value for matching flag"))
}

fn resolve_values(args: &ArgMatches, config: BTreeMap<String, String>) -> Result<Settings, String> {
    let mut values = BTreeMap::new();
    let mut sources = BTreeMap::new();
    for key in KEYS {
        let id = key.replace('_', "-");
        let flag = if args.try_contains_id(&id).is_ok()
            && args.value_source(&id) == Some(clap::parser::ValueSource::CommandLine)
        {
            if key == "data_root" {
                args.get_one::<PathBuf>(&id)
                    .map(|p| p.to_string_lossy().into_owned())
            } else if key == "approve_read_only" {
                Some(args.get_flag(&id).to_string())
            } else {
                args.get_one::<String>(&id).cloned()
            }
        } else {
            None
        };
        let (value, source) = if let Some(v) = flag {
            (Some(v), "flag")
        } else if let Some(v) = config.get(key) {
            (Some(v.clone()), "config")
        } else {
            (None, "default")
        };
        values.insert(key, value);
        sources.insert(key, source);
    }
    for (key, default) in [
        ("provider", "openai"),
        ("auth", "codex"),
        (
            "compaction_strategy",
            harness::wiring::CREATION_COMPACTION_STRATEGY,
        ),
        ("switch_policy", "reject"),
        ("approve_read_only", "false"),
    ] {
        values
            .get_mut(key)
            .expect("key")
            .get_or_insert_with(|| default.into());
    }
    let provider = values["provider"].as_deref().expect("default");
    let codex = values["auth"].as_deref() == Some("codex");
    let model = harness::wiring::default_model(provider, codex).to_owned();
    values.get_mut("model").expect("key").get_or_insert(model);
    if values["data_root"].is_none() {
        values.insert(
            "data_root",
            Some(
                harness::wiring::default_data_root()
                    .map_err(str::to_owned)?
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
    }
    Ok(Settings { values, sources })
}

pub fn run(args: &ArgMatches) -> std::process::ExitCode {
    let result = match args.subcommand() {
        Some(("path", _)) => path(args).and_then(|p| {
            if !args.get_flag("no-config") {
                load(&p, args.get_one::<PathBuf>("config").is_some())?;
            }
            Ok(format!("{} (exists: {})", p.display(), p.exists()))
        }),
        Some(("show", a)) => resolve(a).map(|s| s.show()),
        _ => unreachable!("required subcommand"),
    };
    match result {
        Ok(output) => {
            println!("{output}");
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::from(2)
        }
    }
}
