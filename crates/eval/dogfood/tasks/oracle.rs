//! Host-only mechanical checks. Not linked into the product or ordinary tests.
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, io, path::Path, process::Command};

pub fn tree(root: &Path) -> io::Result<BTreeMap<String, Vec<u8>>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_name() == "target" {
                continue;
            }
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(io::Error::other("symlink in fixture"));
            }
            if kind.is_dir() {
                visit(root, &path, out)?;
            } else {
                out.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    fs::read(path)?,
                );
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out)?;
    Ok(out)
}

pub fn copy(from: &Path, to: &Path) -> io::Result<()> {
    for (name, bytes) in tree(from)? {
        let path = to.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
    }
    Ok(())
}

fn text(tree: &BTreeMap<String, Vec<u8>>, name: &str) -> String {
    String::from_utf8_lossy(tree.get(name).map(Vec::as_slice).unwrap_or_default()).into_owned()
}

// Preserve existing test functions verbatim modulo whitespace and T4's rename.
fn tests(source: &str) -> Vec<String> {
    let mut result = Vec::new();
    for tail in source.split("#[test]").skip(1) {
        let mut depth = 0;
        let mut opened = false;
        let mut body = String::new();
        for ch in tail.chars() {
            if ch == '{' {
                depth += 1;
                opened = true;
            }
            if ch == '}' {
                depth -= 1;
            }
            if !ch.is_whitespace() {
                body.push(ch);
            }
            if opened && depth == 0 {
                break;
            }
        }
        result.push(body);
    }
    result
}

pub fn check(task: &str, pristine: &Path, work: &Path, answer: &str) -> io::Result<Value> {
    let before = tree(pristine)?;
    let after = tree(work)?;
    let changed: Vec<_> = before
        .keys()
        .chain(after.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|key| before.get(*key) != after.get(*key))
        .cloned()
        .collect();
    let answer = answer.to_lowercase();
    let unchanged = changed.is_empty();
    let facts = if task == "T1" {
        answer.contains("btreemap")
            && (answer.contains("sorted") || answer.contains("lexicograph"))
            && answer.contains("zero")
            && answer.contains("blank")
            && answer.contains("duplicate")
            && (answer.contains("sum") || answer.contains("add"))
            && answer.contains("render_balance")
    } else {
        answer.contains("balance_includes_cutoff")
            && answer.contains("ledger")
            && answer.contains('2')
            && answer.contains('5')
    };
    if matches!(task, "T1" | "T5") {
        return Ok(
            json!({"pass":unchanged && facts,"changed_files":changed,"answer_facts":facts,"cargo_exit":null,"tests_preserved":true}),
        );
    }
    // Host cargo is explicitly opt-in dev tooling. Refuse new build scripts,
    // dependencies, symlinks, and manifest edits before executing it.
    let safe = before.get("Cargo.toml") == after.get("Cargo.toml")
        && after.keys().all(|p| {
            before.contains_key(p)
                || p == "Cargo.lock"
                || (p.starts_with("src/") && p.ends_with(".rs"))
        });
    let preserved = ["src/lib.rs", "src/ledger.rs", "src/display.rs"]
        .iter()
        .all(|name| {
            let old = text(&before, name);
            let new = text(&after, name);
            let old = if task == "T4" {
                old.replace("render_balance", "format_balance")
            } else {
                old
            };
            let expected = tests(&old);
            let actual = tests(&new);
            !new.contains("#[ignore")
                && !new.contains("cfg(FALSE)")
                && expected.iter().all(|t| actual.contains(t))
        });
    let feature = if task == "T3" {
        ["src/lib.rs", "src/ledger.rs"]
            .iter()
            .all(|name| tests(&text(&after, name)).len() > tests(&text(&before, name)).len())
            && changed.contains(&"src/lib.rs".to_owned())
            && changed.contains(&"src/ledger.rs".to_owned())
    } else if task == "T4" {
        after
            .iter()
            .filter(|(p, _)| p.ends_with(".rs"))
            .all(|(_, b)| !String::from_utf8_lossy(b).contains("render_balance"))
            && text(&after, "src/display.rs").contains("pub fn format_balance")
    } else {
        true
    };
    if !safe || !preserved || !feature || unchanged {
        return Ok(
            json!({"pass":false,"changed_files":changed,"cargo_exit":null,"tests_preserved":preserved,"safe":safe,"feature":feature}),
        );
    }
    // Inject independent API assertions only after the measured run ends.
    let probe = match task {
        "T3" => {
            "use stock_notebook::{ledger::Ledger,summary_for}; #[test] fn dogfood_api() { let mut l=Ledger::new(); l.record(\"pear\",1,3); l.record(\"pear\",2,-5); l.record(\"apple\",1,7); assert_eq!(l.balance_for(\"pear\"),-2); assert_eq!(l.balance_for(\"missing\"),0); assert_eq!(summary_for(&l,\"pear\"),\"pear: -2\\n\"); assert_eq!(summary_for(&l,\"missing\"),\"missing: 0\\n\"); }"
        }
        "T4" => {
            "use stock_notebook::display::format_balance; #[test] fn dogfood_api(){ assert_eq!(format_balance(&std::collections::BTreeMap::from([(\"pear\".into(),2)])),\"pear: 2\\n\"); }"
        }
        _ => {
            "use stock_notebook::ledger::Ledger; #[test] fn dogfood_api(){ let mut l=Ledger::new(); l.record(\"pear\",1,2); l.record(\"pear\",2,3); l.record(\"pear\",3,100); assert_eq!(l.balance_through(2)[\"pear\"],5); }"
        }
    };
    fs::create_dir_all(work.join("tests"))?;
    fs::write(work.join("tests/dogfood_oracle.rs"), probe)?;
    let mut cmd = Command::new("cargo");
    cmd.current_dir(work).args(["test", "--offline"]);
    if task != "T2" {
        cmd.args(["--", "--skip", "ledger::tests::balance_includes_cutoff"]);
    }
    let result = cmd.output()?;
    fs::remove_file(work.join("tests/dogfood_oracle.rs"))?;
    Ok(
        json!({"pass":result.status.success(),"changed_files":changed,"cargo_exit":result.status.code(),"tests_preserved":preserved,"safe":safe,"feature":feature}),
    )
}
