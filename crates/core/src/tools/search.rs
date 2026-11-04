//! Pure matchers and provider-facing search contracts. Walking stays in fs/.
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::json;

use super::{Refusal, ToolDefinition};
use crate::session::{EffectClass, EffectSet, ExecutionBackend, ResultCode};

pub(super) const PATTERN_BYTES: usize = 16 * 1024;
pub(super) const REGEX_BYTES: usize = 1024 * 1024;
pub(super) const ENTRIES: usize = 1000;
pub(super) const DIRECTORIES: usize = 20_000;
pub(super) const SCAN_BYTES: u64 = 64 * 1024 * 1024;
pub(super) const IGNORE_BYTES: u64 = 65536;
pub(super) const LINE_BYTES: usize = 500;

pub(super) fn definition(index: usize) -> ToolDefinition {
    let path = json!({"type":"string","minLength":1,"description":"Workspace-relative path; default '.', at most 4096 UTF-8 bytes; no traversal, absolute paths, NUL or symlinks."});
    let pattern = json!({"type":"string","minLength":1,"maxLength":PATTERN_BYTES});
    let (name, description, properties, required) = match index {
        4 => (
            "list",
            "List one workspace directory (path defaults to '.'). Return byte-sorted file/dir/symlink entries and file sizes, at most 1000 entries. Honour root/nested .gitignore (65536-byte cap; unreadable/oversized files warn and are ignored). Hide .git and protected paths; never follow symlinks; skip special files. Invalid UTF-8 names are escaped as \\xNN bytes (backslash doubled). Shared deadline 30000 ms. Every reached limit is reported with truncated:true and counts; large output is captured as an artifact.",
            json!({"path":path}),
            json!([]),
        ),
        5 => (
            "glob",
            "Find byte-sorted file paths matching one gitignore-style pattern relative to path (default '.'). include_ignored defaults false; honour root/nested .gitignore (65536-byte cap; unreadable/oversized files warn and are ignored); no global excludes. At most 1000 paths and 20000 visited directories including the start. Pattern at most 16384 UTF-8 bytes. Skip dot-directories unless literal pattern components name them. Hide .git/protected paths; skip symlinks, special files and non-UTF-8 paths. Shared deadline 30000 ms. Every reached limit reports truncated:true, counts and the limit name; large output is captured as an artifact.",
            json!({"pattern":pattern,"path":path,"include_ignored":{"type":"boolean"}}),
            json!(["pattern"]),
        ),
        6 => (
            "grep",
            "Search UTF-8 workspace files using pure-Rust regex syntax (no look-around/backreferences). pattern at most 16384 UTF-8 bytes; compiled regex at most 1 MiB. path defaults '.', optional glob is a gitignore-style filter (at most 16384 UTF-8 bytes); case_insensitive defaults false; output defaults 'matches' (path,line,text), or 'files'; context_lines defaults 0 (0..5); max_results defaults 200 (1..2000); include_ignored defaults false. Results byte-sorted by path then line. Line text at most 500 UTF-8 bytes plus '[truncated]' marker. Skip/count files over 1 MiB, binary (NUL in first 8192 bytes), non-UTF-8, and unreadable/unsafe files. At most 64 MiB read and 20000 directories visited including start; shared deadline 30000 ms. Honour root/nested .gitignore (65536-byte cap; unreadable/oversized files warn and are ignored); no global excludes. Skip dot-directories unless glob literal components name them. Hide .git/protected paths; never follow symlinks; skip special files/non-UTF-8 paths. Every reached limit reports truncated:true, counts and the limit name; large output is captured as an artifact.",
            json!({"pattern":pattern,"path":path,"glob":pattern,"case_insensitive":{"type":"boolean"},"output":{"type":"string","enum":["matches","files"]},"context_lines":{"type":"integer","minimum":0,"maximum":5},"max_results":{"type":"integer","minimum":1,"maximum":2000},"include_ignored":{"type":"boolean"}}),
            json!(["pattern"]),
        ),
        _ => unreachable!(),
    };
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        effects: EffectSet::new([EffectClass::Read]).expect("one effect"),
        backend: ExecutionBackend::TrustedFs,
    }
}

fn pattern_bound(pattern: &str) -> Result<(), Refusal> {
    if pattern.len() > PATTERN_BYTES {
        return Err(Refusal::new(
            ResultCode::InvalidArguments,
            "pattern exceeds 16384 UTF-8 bytes",
        ));
    }
    Ok(())
}
pub(super) fn regex(pattern: &str, insensitive: bool) -> Result<regex::Regex, Refusal> {
    pattern_bound(pattern)?;
    regex::RegexBuilder::new(pattern)
        .case_insensitive(insensitive)
        .size_limit(REGEX_BYTES)
        .build()
        .map_err(|_| {
            Refusal::new(
                ResultCode::InvalidArguments,
                "pattern: invalid regex or compiled program exceeds 1048576 bytes",
            )
        })
}
pub(super) fn glob_matcher(pattern: &str) -> Result<Gitignore, Refusal> {
    pattern_bound(pattern)?;
    let mut builder = GitignoreBuilder::new("");
    builder.add_line(None, pattern).map_err(|_| {
        Refusal::new(
            ResultCode::InvalidArguments,
            "pattern: invalid gitignore-style glob",
        )
    })?;
    builder.build().map_err(|_| {
        Refusal::new(
            ResultCode::InvalidArguments,
            "pattern: invalid gitignore-style glob",
        )
    })
}

pub(super) fn names_dot_directory(pattern: Option<&str>, name: &str) -> bool {
    pattern.is_some_and(|pattern| {
        pattern
            .split('/')
            .any(|component| component == name && !component.contains(['*', '?', '[', '{', '\\']))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        session::{ArtifactRef, CallRef, Digest},
        tools::{Allowance, Authorization, Plan, PolicySnapshot, Registry, tests::FakeWorkspace},
    };
    use serde_json::Value;
    fn configuration() -> Value {
        json!({"policy_revision":"initial","enabled_tools":["read","write","edit","bash","list","glob","grep"],"workspace_read":"allow","workspace_mutation":"ask","process":"ask","network":"deny","protected_paths":[],"allowances":[{"tool":"write","path":"."}]})
    }
    fn policy(value: Value) -> PolicySnapshot {
        let bytes = serde_json::to_vec(&value).unwrap();
        PolicySnapshot::parse(
            ArtifactRef {
                sha256: Digest::of(&bytes),
                bytes: bytes.len() as u64,
            },
            &bytes,
        )
        .unwrap()
    }
    fn plan(
        name: &str,
        args: Value,
        view: &FakeWorkspace,
        p: &PolicySnapshot,
    ) -> Result<Plan, Refusal> {
        let registry = Registry::builtin();
        let tool = registry.resolve(name, p)?;
        tool.plan(
            &tool.validate(&args)?,
            &CallRef {
                response_entry_id: "019a0000-0000-7000-8000-00000000019a".parse().unwrap(),
                call_id: "019a0000-0000-7000-8000-00000000019b".parse().unwrap(),
            },
            view,
            p,
        )
    }
    #[test]
    fn search_definitions_defaults_strict_validation_and_golden_plans() {
        let p = policy(configuration());
        let view = FakeWorkspace::new();
        for (name, args, golden, defaults) in [
            (
                "list",
                json!({}),
                include_str!("golden/list.json"),
                json!({"path":"."}),
            ),
            (
                "glob",
                json!({"pattern":"**/*.rs"}),
                include_str!("golden/glob.json"),
                json!({"pattern":"**/*.rs","path":".","include_ignored":false}),
            ),
            (
                "grep",
                json!({"pattern":"hello"}),
                include_str!("golden/grep.json"),
                json!({"pattern":"hello","path":".","glob":null,"case_insensitive":false,"output":"matches","context_lines":0,"max_results":200,"include_ignored":false}),
            ),
        ] {
            let registry = Registry::builtin();
            let tool = registry.resolve(name, &p).unwrap();
            assert_eq!(
                tool.definition().effects,
                EffectSet::new([EffectClass::Read]).unwrap()
            );
            assert_eq!(tool.definition().backend, ExecutionBackend::TrustedFs);
            assert_eq!(
                tool.definition().input_schema["additionalProperties"],
                false
            );
            let a = plan(name, args.clone(), &view, &p).unwrap();
            assert_eq!(a.bytes(), golden.trim_end().as_bytes());
            assert_eq!(a.digest(), &Digest::of(a.bytes()));
            assert_eq!(
                a.bytes(),
                plan(name, args.clone(), &view, &p).unwrap().bytes()
            );
            let wire: Value = serde_json::from_slice(a.bytes()).unwrap();
            assert_eq!(wire["arguments"], defaults);
            assert_eq!(wire["target"], Value::Null);
            let mut extra = args;
            extra["unknown"] = true.into();
            assert_eq!(
                tool.validate(&extra).unwrap_err().code,
                ResultCode::InvalidArguments
            );
        }
    }
    #[test]
    fn search_regex_and_glob_validation_bounds_are_pure_refusals() {
        let p = policy(configuration());
        let registry = Registry::builtin();
        let grep = registry.resolve("grep", &p).unwrap();
        for args in [
            json!({"pattern":"["}),
            json!({"pattern":"(?=a)"}),
            json!({"pattern":"(a)\\1"}),
            json!({"pattern":"(a{1000}){1000}"}),
            json!({"pattern":"x","context_lines":6}),
            json!({"pattern":"x","max_results":2001}),
            json!({"pattern":"x","max_results":0}),
            json!({"pattern":"x","output":"raw"}),
            json!({"pattern":"x","glob":"[".repeat(20000)}),
            json!({"pattern":"é".repeat(8193)}),
        ] {
            assert_eq!(
                grep.validate(&args).unwrap_err().code,
                ResultCode::InvalidArguments,
                "{args}"
            );
        }
        let glob = registry.resolve("glob", &p).unwrap();
        assert_eq!(
            glob.validate(&json!({"pattern":"é".repeat(8193)}))
                .unwrap_err()
                .code,
            ResultCode::InvalidArguments
        );
        for name in ["list", "glob", "grep"] {
            let tool = registry.resolve(name, &p).unwrap();
            for bad in ["../outside", "/etc", "bad\0name"] {
                let mut args = if name == "list" {
                    json!({})
                } else {
                    json!({"pattern":"x"})
                };
                args["path"] = bad.into();
                assert_eq!(
                    tool.validate(&args).unwrap_err().code,
                    ResultCode::InvalidArguments
                );
            }
        }
    }
    #[test]
    fn search_policy_reads_ignore_allowances_and_honour_every_read_policy() {
        let view = FakeWorkspace::new();
        for (name, args) in [
            ("list", json!({})),
            ("glob", json!({"pattern":"*"})),
            ("grep", json!({"pattern":"x"})),
        ] {
            for rule in ["allow", "ask", "deny"] {
                let mut config = configuration();
                config["workspace_read"] = rule.into();
                let p = policy(config);
                let plan = plan(name, args.clone(), &view, &p).unwrap();
                assert!(Allowance::remember(&plan).is_none());
                match (rule, crate::tools::decide(&plan, plan.facts(), &p)) {
                    ("allow", Authorization::Allow(_)) | ("ask", Authorization::Ask(_)) => {}
                    ("deny", Authorization::Deny(error)) => {
                        assert_eq!(error.code, ResultCode::PolicyDenied)
                    }
                    _ => panic!("wrong read policy"),
                };
            }
            let mut config = configuration();
            config["protected_paths"] = json!(["."]);
            assert_eq!(
                plan(name, args.clone(), &view, &policy(config))
                    .unwrap_err()
                    .code,
                ResultCode::ProtectedPath
            );
            let mut config = configuration();
            config["enabled_tools"] = json!(["read", "write", "edit", "bash"]);
            assert_eq!(
                Registry::builtin()
                    .resolve(name, &policy(config))
                    .err()
                    .unwrap()
                    .code,
                ResultCode::ToolDisabled
            );
        }
    }
    #[test]
    fn search_every_argument_is_bound_into_approval_digest() {
        let mut view = FakeWorkspace::new();
        view.directory("src");
        let p = policy(configuration());
        for (name, args, changes) in [
            ("list", json!({}), vec![("path", json!("src"))]),
            (
                "glob",
                json!({"pattern":"*.rs"}),
                vec![
                    ("pattern", json!("*.md")),
                    ("path", json!("src")),
                    ("include_ignored", json!(true)),
                ],
            ),
            (
                "grep",
                json!({"pattern":"hello"}),
                vec![
                    ("pattern", json!("world")),
                    ("path", json!("src")),
                    ("glob", json!("*.rs")),
                    ("case_insensitive", json!(true)),
                    ("output", json!("files")),
                    ("context_lines", json!(1)),
                    ("max_results", json!(201)),
                    ("include_ignored", json!(true)),
                ],
            ),
        ] {
            let base = plan(name, args.clone(), &view, &p).unwrap();
            for (key, value) in changes {
                let mut changed = args.clone();
                changed[key] = value;
                assert_ne!(
                    base.digest(),
                    plan(name, changed, &view, &p).unwrap().digest(),
                    "{name}.{key}"
                );
            }
        }
    }
}
