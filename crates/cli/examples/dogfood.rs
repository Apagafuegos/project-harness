//! Opt-in live dogfood measurement; host oracles deliberately run host cargo.
//! Never invoked by check.sh. No raw journals/transcripts are persisted.
#[path = "../../eval/dogfood/tasks/oracle.rs"]
mod oracle;
#[path = "../tests/support/tui_pty.rs"]
#[allow(dead_code)]
mod support;

use harness_core::session::{DataRoot, JournalStore};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() {
    if run().is_err() {
        // Never format an error which might contain model or credential text.
        eprintln!("dogfood: infrastructure_failure (no live retry)");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let suite = root.join("crates/eval/dogfood");
    let scratch = root.join("target/dogfood-tmp");
    fs::create_dir_all(&scratch)?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--self-test"] {
        let result = self_test(&suite, &scratch)
            .and_then(|()| journal_selftest(&scratch, &root.join("target/debug/harness")))
            .and_then(|()| config_selftest(&scratch))
            .and_then(|()| tty_selftest());
        clean_scratch(&scratch);
        return result;
    }
    if args.first().map(String::as_str) == Some("--summarize") {
        let dir = args.get(1).ok_or("usage")?;
        let mut records = Vec::new();
        let mut cells: Vec<_> = fs::read_dir(dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path().join("metrics.json"))
            .filter(|path| path.is_file())
            .collect();
        cells.sort();
        for cell in cells {
            records.push(serde_json::from_str::<Value>(&fs::read_to_string(cell)?)?);
        }
        write_summary(Path::new(dir), &records)?;
        return Ok(());
    }
    let mut provider = "all";
    let mut task = "all";
    let mut out = None;
    // Opt-in user config for the measured harness invocation (decision 15).
    // Absent means `--no-config`, so the baseline method still reproduces.
    let mut config: Option<PathBuf> = None;
    for pair in args.chunks(2) {
        if pair.len() != 2 {
            return Err("usage".into());
        }
        match pair[0].as_str() {
            "--provider" => provider = &pair[1],
            "--task" => task = &pair[1],
            "--out" => out = Some(PathBuf::from(&pair[1])),
            "--config" => config = Some(PathBuf::from(&pair[1])),
            _ => return Err("usage".into()),
        }
    }
    if !["all", "openai", "deepseek"].contains(&provider)
        || !["all", "T1", "T2", "T3", "T4", "T5", "T6"].contains(&task)
    {
        return Err("usage".into());
    }
    let out = out.ok_or("--out required")?;
    fs::create_dir_all(&out)?;
    let binary = root.join("target/debug/harness");
    let commit = Command::new("git")
        .current_dir(&root)
        .args(["rev-parse", "HEAD"])
        .output()?;
    let commit = String::from_utf8(commit.stdout)?.trim().to_owned();
    let mut records = Vec::new();
    let mut skipped = false;
    for p in ["openai", "deepseek"]
        .into_iter()
        .filter(|p| provider == "all" || provider == *p)
    {
        let missing =
            p == "deepseek" && std::env::var_os("DEEPSEEK_API_KEY").is_none_or(|k| k.is_empty());
        skipped |= missing;
        let mut blocked = missing;
        for t in ["T1", "T2", "T3", "T4", "T5", "T6"]
            .into_iter()
            .filter(|t| task == "all" || task == *t)
        {
            let dir = out.join(format!("{p}-{t}"));
            fs::create_dir(&dir)?; // Never overwrite a measurement or silently retry.
            let mut record = skeleton(p, t, &commit);
            if t == "T6" {
                record["undo"] = json!({"attempted":false,"result":"not_run"});
            }
            record["config"] = json!(if config.is_some() {
                "file"
            } else {
                "no_config"
            });
            if blocked {
                // Decision 4 and the DeepSeek ruling: an unrun cell is data.
                record["status"] = json!("not_run");
                record["reason"] = json!(if missing {
                    "key_unavailable"
                } else {
                    "provider_quota"
                });
                fs::write(
                    dir.join("excerpt.txt"),
                    if missing {
                        "Not run: DeepSeek key unavailable (exit 77 semantics); no transcript exists.\n"
                    } else {
                        "Not run after a provider quota/rate-limit code; no retry.\n"
                    },
                )?;
            } else {
                let temp = tempfile::Builder::new()
                    .prefix("cell-")
                    .tempdir_in(&scratch)?;
                let ws = temp.path().join("ws");
                let data = temp.path().join("data");
                oracle::copy(&suite.join("fixture"), &ws)?;
                let prompt = fs::read_to_string(suite.join(format!("tasks/{t}/prompt.md")))?;
                let execution = ask(&binary, p, &prompt, &ws, &data, config.as_deref())?;
                record["status"] = json!(if execution.deadline {
                    "deadline"
                } else {
                    "run"
                });
                record["exit_code"] = json!(execution.exit);
                record["wall_ms"] = json!(execution.wall_ms);
                record["approvals"] = json!(execution.approvals);
                let mut sandbox_paths = Vec::new();
                let answer = journal(&data, &mut record, &mut sandbox_paths)?;
                approval_split(&mut record, &execution.transcript);
                if execution.deadline {
                    record["result_code"] = json!("deadline");
                }
                // An oracle failure on a model-modified tree is data, not a
                // driver crash, so it must never discard the other cells.
                record["oracle"] = match oracle::check(t, &suite.join("fixture"), &ws, &answer) {
                    Ok(value) => value,
                    Err(_) => json!({"pass":false,"error":"oracle_failed"}),
                };
                if t == "T6" {
                    record["undo"] = undo_metrics(&binary, &data, &ws, &suite.join("fixture"));
                }
                let clean = sanitize(&execution.transcript, &ws, &data, &sandbox_paths);
                let lines: Vec<_> = clean
                    .lines()
                    .filter(|line| !line.starts_with("session:") && !line.starts_with("usage:"))
                    .collect();
                // Head keeps setup; tail keeps the final answer and outcome.
                let excerpt = if lines.len() <= 38 {
                    lines.join("\n")
                } else {
                    let mut kept: Vec<&str> = lines[..10].to_vec();
                    kept.push("...");
                    kept.extend_from_slice(&lines[lines.len() - 27..]);
                    kept.join("\n")
                };
                fs::write(dir.join("excerpt.txt"), format!("{excerpt}\n"))?;
                blocked = record["result_code"] == "rate_limit"
                    || record["model_errors"].get("rate_limit").is_some()
                    || record["model_errors"].get("insufficient_quota").is_some();
                // The entire raw journal, artifacts, transcript and workspace die here.
                temp.close()?;
            }
            fs::write(
                dir.join("metrics.json"),
                serde_json::to_string_pretty(&record)? + "\n",
            )?;
            println!(
                "{p}-{t}: {} oracle={}",
                record["result_code"], record["oracle"]["pass"]
            );
            records.push(record);
        }
    }
    write_summary(&out, &records)?;
    clean_scratch(&scratch);
    if skipped {
        std::process::exit(77);
    }
    Ok(())
}

/// Stable aggregate: per-provider decision-7 data plus every cell record.
/// It is also usable on its own (`--summarize DIR`) so the aggregate can be
/// rebuilt from committed cells without spending provider quota again.
fn write_summary(out: &Path, records: &[Value]) -> Result<()> {
    let mut decision = BTreeMap::new();
    let mut approvals_by_provider = BTreeMap::new();
    for provider in ["openai", "deepseek"] {
        let cells: Vec<_> = records
            .iter()
            .filter(|r| r["provider"] == provider && r["task"] != "T6")
            .collect();
        let sum = |key: &str| {
            cells
                .iter()
                .map(|r| r[key].as_f64().unwrap_or(0.0))
                .sum::<f64>()
        };
        // A qualifying turn is one whose whole turn had such a segment.
        let eligible: Vec<_> = cells
            .iter()
            .filter(|r| r["readonly_segments"].as_u64().unwrap_or(0) > 0)
            .collect();
        let wall: f64 = eligible
            .iter()
            .map(|r| r["turn_wall_ms"].as_f64().unwrap_or(0.0))
            .sum::<f64>()
            + 0.0;
        let tool = sum("readonly_segment_tool_ms").max(0.0);
        let percent = (wall > 0.0).then(|| 100.0 * tool / wall);
        let count = |key: &str, class: &str, field: &str| {
            cells
                .iter()
                .map(|r| r[class][key][field].as_u64().unwrap_or(0))
                .sum::<u64>()
        };
        let approvals = json!({
            "prompts": cells.iter().map(|r| r["approval_prompts"].as_u64().unwrap_or(0)).sum::<u64>(),
            "read_only_prompts": count("read_only", "approval_classes", "prompts"),
            "other_prompts": count("other", "approval_classes", "prompts"),
            "auto_approved_read_only": count("read_only", "approval_classes", "auto_approved"),
            "requests_by_tool": cells.iter().fold(BTreeMap::<String, u64>::new(), |mut acc, r| {
                for (name, n) in r["approvals_by_tool"].as_object().into_iter().flatten() {
                    *acc.entry(name.clone()).or_default() += n.as_u64().unwrap_or(0);
                }
                acc
            }),
            "split_consistent": cells.iter().all(|r| r["approval_split_consistent"] == json!(true)),
        });
        decision.insert(
            provider,
            json!({
                "segments": cells.iter().map(|r| r["readonly_segments"].as_u64().unwrap_or(0)).sum::<u64>(),
                "eligible_turns": eligible.len(),
                "tool_ms": tool,
                "eligible_turn_wall_ms": wall,
                "share_percent": percent,
                "threshold_percent": 10,
                "meets_threshold": percent.is_some_and(|p| p >= 10.0),
            }),
        );
        approvals_by_provider.insert(provider, approvals);
    }
    fs::write(
        out.join("summary.json"),
        serde_json::to_string_pretty(&json!({
            "schema": 2,
            "comparison_tasks": ["T1", "T2", "T3", "T4", "T5"],
            "separate_probe_tasks": ["T6"],
            "decision_7_record_basis": "execution_intent.turn_id call to its tool_result commit, in a turn segment with two or more consecutive read-only calls (read/list/glob/grep) in one model response",
            "approval_basis": "requests from journal approval_requested joined to the call; prompts from the client's own approve prompt; auto-approved read-only from the client's auto-approval line, subtracted from the read-only requests",
            "runs": records,
            "decision_7": decision,
            "approvals": approvals_by_provider,
        }))? + "\n",
    )?;
    Ok(())
}

fn skeleton(provider: &str, task: &str, commit: &str) -> Value {
    json!({"schema":2,"harness_commit":commit,"provider":provider,"model":if provider=="openai" {"gpt-6.1-sol"} else {"deepseek-flash"},"task":task,"config":"no_config",
        "status":"not_run","reason":null,"retry_after_ms":null,"provider_code":null,"oracle":null,"exit_code":null,"result_code":null,"inferences":0,"tool_calls":{},"approvals":0,"approvals_by_tool":{},"approval_prompts":0,"approval_classes":{"read_only":{"requests":0,"auto_approved":0,"prompts":0},"other":{"requests":0,"prompts":0}},"approval_split_consistent":true,"sandbox":{"configured":false},"refusals":{},"errors":{},"model_errors":{},
        "wall_ms":null,"turn_wall_ms":null,"input_tokens":null,"output_tokens":null,"tool_ms":0.0,"tool_time_percent":null,"readonly_segments":0,"readonly_segment_tool_ms":0.0,"readonly_segments_detail":[],"tool_facts":[]})
}

/// Human prompts split by class, from the journal's approval requests and the
/// client's own auto-approval line. `approve_read_only` never changes the
/// recorded policy, so the request is journaled either way; only the transcript
/// says whether a human was asked.
fn approval_split(metrics: &mut Value, transcript: &str) {
    let prompts = transcript.matches("approve? [y/N] ").count() as u64;
    let auto = transcript
        .matches("approval: read-only auto-approved")
        .count() as u64;
    let mut read_only_requests = 0u64;
    let mut other_requests = 0u64;
    if let Some(by_tool) = metrics["approvals_by_tool"].as_object() {
        for (name, count) in by_tool {
            let count = count.as_u64().unwrap_or(0);
            if ["read", "list", "glob", "grep"].contains(&name.as_str()) {
                read_only_requests += count;
            } else {
                other_requests += count;
            }
        }
    }
    let read_only_prompts = read_only_requests.saturating_sub(auto);
    metrics["approval_prompts"] = json!(prompts);
    metrics["approval_classes"] = json!({
        "read_only": {"requests": read_only_requests, "auto_approved": auto, "prompts": read_only_prompts},
        "other": {"requests": other_requests, "prompts": other_requests},
    });
    // The split must reconstruct the human prompts exactly and never claim more
    // auto-approvals than read-only requests.
    metrics["approval_split_consistent"] =
        json!(auto <= read_only_requests && read_only_prompts + other_requests == prompts);
}

struct Execution {
    exit: u32,
    wall_ms: u128,
    approvals: usize,
    deadline: bool,
    transcript: String,
}

fn ask(
    binary: &Path,
    provider: &str,
    prompt: &str,
    ws: &Path,
    data: &Path,
    config: Option<&Path>,
) -> Result<Execution> {
    // Same portable-pty setup and reader-channel pattern as support::probe_inner.
    let pair = native_pty_system().openpty(PtySize {
        rows: 40,
        cols: 160,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut cmd = CommandBuilder::new(binary);
    cmd.cwd(ws);
    // Decision 15: the opt-in rerun measures the user's own config; the default
    // keeps the baseline's `--no-config` method reproducible.
    match config {
        Some(path) => {
            cmd.arg("--config");
            cmd.arg(path);
        }
        None => {
            cmd.arg("--no-config");
        }
    }
    cmd.args(["ask", "--workspace"]);
    cmd.arg(ws);
    cmd.arg("--data-root");
    cmd.arg(data);
    cmd.args(["--provider", provider]);
    if provider == "openai" {
        cmd.args(["--auth", "codex"]);
    }
    cmd.arg(prompt);
    cmd.env("NO_COLOR", "1");
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let (tx, rx) = mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut buf = [0; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let start = Instant::now();
    let mut output = Vec::new();
    let mut approvals = 0;
    let mut deadline = false;
    let status = loop {
        while let Ok(bytes) = rx.try_recv() {
            output.extend(bytes);
        }
        let count = String::from_utf8_lossy(&output)
            .matches("approve? [y/N] ")
            .count();
        while approvals < count {
            writer.write_all(b"y\n")?;
            writer.flush()?;
            approvals += 1;
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if !deadline && start.elapsed() >= Duration::from_secs(600) {
            writer.write_all(b"\x03")?;
            writer.flush()?;
            deadline = true;
        }
        if deadline && start.elapsed() >= Duration::from_secs(610) {
            child.kill()?;
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    reader_thread.join().map_err(|_| "reader panic")?;
    while let Ok(bytes) = rx.try_recv() {
        output.extend(bytes);
    }
    Ok(Execution {
        exit: status.exit_code(),
        wall_ms: start.elapsed().as_millis(),
        approvals,
        deadline,
        transcript: String::from_utf8_lossy(&output).replace('\r', ""),
    })
}

fn bump(map: &mut Value, key: &str) {
    let n = map[key].as_u64().unwrap_or(0);
    map[key] = json!(n + 1);
}

// UTC time-of-day milliseconds. Differences cross midnight at most once under
// the ten-minute deadline. Journal order, not timestamp order, is authoritative.
fn millis(at: &str) -> Result<f64> {
    let time = at
        .split('T')
        .nth(1)
        .ok_or("timestamp")?
        .trim_end_matches('Z');
    let parts: Vec<f64> = time
        .split(':')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    Ok((parts[0] * 3600.0 + parts[1] * 60.0 + parts[2]) * 1000.0)
}
fn elapsed(a: f64, b: f64) -> f64 {
    if b >= a { b - a } else { b + 86_400_000.0 - a }
}

fn journal(data: &Path, metrics: &mut Value, sandbox_paths: &mut Vec<String>) -> Result<String> {
    if !data.exists() {
        metrics["result_code"] = json!("no_session");
        return Ok(String::new());
    }
    let root = DataRoot::open(data, false)?;
    let sessions = root.sessions()?;
    let Some(session) = sessions.first() else {
        metrics["result_code"] = json!("no_session");
        return Ok(String::new());
    };
    let inspection = JournalStore::inspect(&root, &session.session_id)?;
    if inspection.diagnostic.is_some() {
        return Err("invalid journal".into());
    }
    // inspect validates checksums/schema/replay first, then parse read-only for
    // per-record timestamps absent from the snapshot's public event iterator.
    let raw = fs::read_to_string(
        data.join("sessions")
            .join(format!("{}.jsonl", session.session_id)),
    )?;
    let mut calls = BTreeMap::<String, (String, Value, usize)>::new();
    let mut intents = BTreeMap::<String, f64>::new();
    let mut segments = Vec::<(usize, Vec<String>)>::new();
    let mut durations = BTreeMap::<String, f64>::new();
    let mut answer = String::new();
    let mut start = None;
    let mut end = None;
    let mut configuration = None;
    for line in raw.lines() {
        let envelope: Value = serde_json::from_str(line)?;
        let record = envelope.get("record").unwrap_or(&envelope);
        let at = millis(record["at"].as_str().ok_or("missing at")?)?;
        let payload = &record["payload"];
        if payload["kind"] == "session_created" {
            configuration = Some(payload["configuration"].clone());
            continue;
        }
        if payload["kind"] != "commit" {
            continue;
        }
        for entry in payload["entries"].as_array().ok_or("entries")? {
            let p = &entry["payload"];
            if p["kind"] == "assistant_response" {
                let index = metrics["inferences"].as_u64().unwrap_or(0) as usize + 1;
                metrics["inferences"] = json!(index);
                for key in ["input_tokens", "output_tokens"] {
                    if let Some(n) = p["usage"][key].as_u64() {
                        metrics[key] = json!(metrics[key].as_u64().unwrap_or(0) + n);
                    }
                }
                if let Some(code) = p["error"]["code"].as_str() {
                    bump(&mut metrics["model_errors"], code);
                }
                // Provider diagnostics only: a retry hint and a bounded code.
                if let Some(ms) = p["error"]["retry_after_ms"].as_u64() {
                    metrics["retry_after_ms"] = json!(ms);
                }
                if let Some(code) = p["error"]["provider_code"].as_str()
                    && code.len() <= 64
                    && code
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                {
                    metrics["provider_code"] = json!(code);
                }
                let mut consecutive = Vec::new();
                for block in p["content"].as_array().ok_or("content")? {
                    if block["kind"] == "text" {
                        answer.push_str(block["text"].as_str().unwrap_or(""));
                        answer.push('\n');
                    }
                    if block["kind"] != "tool_call" {
                        continue;
                    }
                    let id = block["call_id"].as_str().ok_or("call id")?.to_owned();
                    let name = block["name"].as_str().ok_or("tool name")?.to_owned();
                    bump(&mut metrics["tool_calls"], &name);
                    calls.insert(
                        id.clone(),
                        (name.clone(), block["arguments"].clone(), index),
                    );
                    // Only registry trusted read tools, never bash even if its command looks read-only.
                    if ["read", "list", "glob", "grep"].contains(&name.as_str()) {
                        consecutive.push(id);
                    } else {
                        if consecutive.len() >= 2 {
                            segments.push((index, consecutive.clone()));
                        }
                        consecutive.clear();
                    }
                }
                if consecutive.len() >= 2 {
                    segments.push((index, consecutive));
                }
            } else if p["kind"] == "tool_result" {
                let r = &p["result"];
                let id = r["call"]["call_id"].as_str().ok_or("result call")?;
                let code = r["metadata"]["code"].as_str().unwrap_or("none");
                let status = r["status"].as_str().unwrap_or("unknown");
                if status != "succeeded" {
                    if matches!(status, "denied" | "refused" | "invalid_arguments") {
                        bump(&mut metrics["refusals"], code);
                    } else {
                        bump(&mut metrics["errors"], code);
                    }
                }
                if let Some(begin) = intents.get(id) {
                    durations.insert(id.into(), elapsed(*begin, at));
                }
                if let Some((name, args, index)) = calls.get(id) {
                    // Journal-derived content-free facts; never retain arbitrary model text.
                    let body = r["content"].to_string();
                    let cargo = name == "bash" && args.to_string().contains("cargo");
                    metrics["tool_facts"].as_array_mut().unwrap().push(json!({"inference":index,"tool":name,"status":status,"code":code,"exit_code":r["metadata"]["exit_code"],"cargo_attempt":cargo,"cargo_not_found":cargo && (body.contains("command not found") || body.contains("cargo: not found")),"duration_ms":durations.get(id)}));
                }
            }
        }
        for event in payload["events"].as_array().ok_or("events")? {
            match event["kind"].as_str().unwrap_or("") {
                "turn_started" => start = Some(at),
                "turn_finished" => {
                    end = Some(at);
                    metrics["result_code"] = event["code"]
                        .as_str()
                        .map_or(json!("completed"), |c| json!(c));
                }
                "execution_intent" => {
                    intents.insert(
                        event["call"]["call_id"].as_str().ok_or("intent")?.into(),
                        at,
                    );
                }
                "approval_requested" => {
                    if let Some((name, _, _)) =
                        calls.get(event["call"]["call_id"].as_str().unwrap_or(""))
                    {
                        bump(&mut metrics["approvals_by_tool"], name);
                    }
                }
                _ => {}
            }
        }
    }
    let wall = start.zip(end).map(|(a, b)| elapsed(a, b));
    metrics["turn_wall_ms"] = json!(wall);
    let tool: f64 = durations.values().sum::<f64>().max(0.0) + 0.0;
    metrics["tool_ms"] = json!(tool);
    metrics["tool_time_percent"] = json!(wall.filter(|v| *v > 0.0).map(|v| 100.0 * tool / v));
    let mut segment_total = 0.0;
    for (index, ids) in &segments {
        let time: f64 = ids.iter().filter_map(|id| durations.get(id)).sum();
        segment_total += time;
        metrics["readonly_segments_detail"].as_array_mut().unwrap().push(json!({"inference":index,"calls":ids.len(),"tool_ms":time,"turn_wall_ms":wall,"percent":wall.filter(|v|*v>0.0).map(|v|100.0*time/v)}));
    }
    metrics["readonly_segments"] = json!(segments.len());
    metrics["readonly_segment_tool_ms"] = json!(segment_total);
    // The recorded configuration is the only honest source for what was mounted:
    // it is what the run actually used. Only counts and key names are kept.
    if let Some(reference) = configuration
        && let Ok(reference) =
            serde_json::from_value::<harness_core::session::ArtifactRef>(reference)
        && let Ok(bytes) = root.read_artifact(&reference, 1024 * 1024)
        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
    {
        match value.get("sandbox") {
            Some(sandbox) => {
                let paths = sandbox["toolchain_paths"]
                    .as_array()
                    .map(|entries| entries.as_slice())
                    .unwrap_or_default();
                sandbox_paths.extend(
                    paths
                        .iter()
                        .filter_map(|path| path.as_str().map(str::to_owned)),
                );
                if let Some(registry) = sandbox["cargo_registry"].as_str() {
                    sandbox_paths.push(registry.to_owned());
                }
                let mut env_keys: Vec<_> = sandbox["env"]
                    .as_object()
                    .map(|env| env.keys().cloned().collect())
                    .unwrap_or_default();
                env_keys.sort();
                metrics["sandbox"] = json!({
                    "configured": true,
                    "toolchain_mounts": paths.len(),
                    "path_prepend_entries": sandbox["path_prepend"].as_array().map(|p| p.len()).unwrap_or(0),
                    "cargo_registry": sandbox["cargo_registry"].is_string(),
                    "env_keys": env_keys,
                });
            }
            None => metrics["sandbox"] = json!({"configured": false}),
        }
    }
    Ok(answer)
}

fn sanitize(raw: &str, ws: &Path, data: &Path, sandbox_paths: &[String]) -> String {
    let mut clean = raw
        .replace(&ws.to_string_lossy().to_string(), "<ws>")
        .replace(&data.to_string_lossy().to_string(), "<data>");
    // Recorded toolchain and registry paths are replaced instead of redacted, so
    // the sanitized line still carries the fact without a username in it.
    for path in sandbox_paths {
        clean = clean.replace(path, "<toolchain>");
    }
    // Strip terminal control sequences, then redact risky lines wholesale. This
    // is deliberately conservative: manually review every excerpt before commit.
    let mut plain = String::new();
    let mut escape = false;
    for ch in clean.chars() {
        if ch == '\x1b' {
            escape = true;
            continue;
        }
        if escape {
            if ch.is_ascii_alphabetic() {
                escape = false;
            }
            continue;
        }
        if ch == '\n' || ch == '\t' || !ch.is_control() {
            plain.push(ch);
        }
    }
    clean = plain
        .lines()
        .map(|line| {
            let lower = line.to_lowercase();
            if [
                "/home/",
                "sk-",
                "bearer",
                "account",
                "@",
                "target/dogfood-tmp",
                "token\":",
                "api_key",
                "auth.json",
            ]
            .iter()
            .any(|term| lower.contains(term))
            {
                "[sensitive line omitted]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    clean
}

/// F09: a leftover scratch file is not a measurement failure. Remove the
/// whole driver scratch tree, and never fail recorded cells if it survives.
fn clean_scratch(scratch: &Path) {
    if let Err(error) = fs::remove_dir_all(scratch)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("dogfood: warning: leftover scratch directory was not removed");
    }
}

fn self_test(suite: &Path, scratch: &Path) -> Result<()> {
    for task in ["T1", "T2", "T3", "T4", "T5", "T6"] {
        let temp = tempfile::Builder::new()
            .prefix("oracle-")
            .tempdir_in(scratch)?;
        let ws = temp.path().join("ws");
        oracle::copy(&suite.join("fixture"), &ws)?;
        assert_eq!(
            oracle::check(task, &suite.join("fixture"), &ws, "")?["pass"],
            false,
            "pristine {task}"
        );
        let patch = suite.join(format!("reference/{task}.patch"));
        if fs::metadata(&patch)?.len() > 0 {
            let result = Command::new("patch")
                .current_dir(&ws)
                .args(["--batch", "--no-backup-if-mismatch", "-p1", "-i"])
                .arg(patch)
                .output()?;
            assert!(result.status.success(), "reference patch {task}");
        }
        let answer = fs::read_to_string(suite.join(format!("reference/{task}.answer.md")))?;
        assert_eq!(
            oracle::check(task, &suite.join("fixture"), &ws, &answer)?["pass"],
            true,
            "reference {task}"
        );
        if task == "T6" {
            let display_path = ws.join("src/display.rs");
            let display = fs::read_to_string(&display_path)?;
            fs::write(&display_path, display.replacen("product", "item", 1))?;
            assert_eq!(
                oracle::check(task, &suite.join("fixture"), &ws, &answer)?["pass"],
                false,
                "incomplete rename T6"
            );
            let original = fs::read_to_string(suite.join("fixture/src/display.rs"))?;
            let (reference, _) = rename_fixture_words(&original);
            fs::write(&display_path, reference)?;
            let display = fs::read_to_string(&display_path)?;
            let unsafe_change = display.replace(
                "text.push_str(&format!(\"{product}: {quantity}\\n\"));",
                "text.push_str(&format!(\"unsafe {product}: {quantity}\\n\"));",
            );
            assert_ne!(unsafe_change, display, "unsafe behavior fixture matched");
            fs::write(&display_path, unsafe_change)?;
            assert_eq!(
                oracle::check(task, &suite.join("fixture"), &ws, &answer)?["pass"],
                false,
                "behavior change T6"
            );
            println!("T6: pristine fails; reference passes; incomplete and unsafe edits fail");
            temp.close()?;
            continue;
        }
        if matches!(task, "T1" | "T5") {
            // F06 self-test: a created lockfile is build output, not a change,
            // so the read-only oracle still passes on it.
            fs::write(ws.join("Cargo.lock"), "version = 4\n")?;
            assert_eq!(
                oracle::check(task, &suite.join("fixture"), &ws, &answer)?["pass"],
                true,
                "created Cargo.lock only {task}"
            );
            fs::remove_file(ws.join("Cargo.lock"))?;
            fs::write(ws.join("extra.txt"), "change")?;
            assert_eq!(
                oracle::check(task, &suite.join("fixture"), &ws, &answer)?["pass"],
                false,
                "mutation {task}"
            );
        }
        println!(
            "{task}: pristine fails; reference passes; read-only mutation rejected where applicable"
        );
        temp.close()?;
    }
    Ok(())
}

/// Offline proof that the durable-timestamp extraction matches what the runtime
/// actually writes. It runs two trusted reads in one inference through a real
/// journal, then checks the counts, the result code and the read-only segment.
fn journal_selftest(scratch: &Path, binary: &Path) -> Result<()> {
    use harness_core::{
        context::ModelLimits,
        model::{
            ProviderEvent,
            scripted::{Script, ScriptedProvider, Step},
        },
        runtime::*,
        session::{FinishReason, *},
    };
    use std::sync::Arc;

    let temp = tempfile::Builder::new()
        .prefix("journal-")
        .tempdir_in(scratch)?;
    let ws = temp.path().join("ws");
    let data = temp.path().join("data");
    fs::create_dir(&ws)?;
    fs::write(ws.join("note"), "hello")?;
    fs::create_dir(ws.join("src"))?;
    for name in ["display.rs", "ledger.rs", "lib.rs"] {
        fs::write(ws.join("src").join(name), "item\n")?;
    }
    let pristine = temp.path().join("pristine");
    oracle::copy(&ws, &pristine)?;
    let model = ModelRef {
        provider_instance: "mock".into(),
        model: "scripted".into(),
    };
    let policy = json!({"policy_revision":"p1","enabled_tools":["read","multi_edit"],
        "workspace_read":"ask","workspace_mutation":"ask","process":"ask",
        "network":"deny","protected_paths":[],"allowances":[]});
    let root = Arc::new(DataRoot::open(&data, true)?);
    let header = SessionHeader {
        workspace: ws.to_str().ok_or("workspace")?.into(),
        initial_model: model.clone(),
        policy_revision: "p1".into(),
        configuration: root.put_artifact(&serde_json::to_vec(&policy)?)?,
    };
    let call = |index: u32, name: &str, arguments: Value| {
        [
            Step::Event(ProviderEvent::ToolCallStarted {
                block_index: index,
                name: name.into(),
                provider_call_id: None,
            }),
            Step::Event(ProviderEvent::ToolArgumentsDelta {
                block_index: index,
                fragment: arguments.to_string(),
            }),
            Step::Event(ProviderEvent::ToolCallReady { block_index: index }),
        ]
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        Script::new(
            [Step::Event(ProviderEvent::Started)]
                .into_iter()
                .chain(call(0, "read", json!({"path":"note"})))
                .chain(call(1, "read", json!({"path":"note"})))
                .chain(call(
                    2,
                    "multi_edit",
                    json!({"edits":[
                        {"path":"src/display.rs","old_text":"item","new_text":"product"},
                        {"path":"src/ledger.rs","old_text":"item","new_text":"product"},
                        {"path":"src/lib.rs","old_text":"item","new_text":"product"}
                    ]}),
                ))
                .chain([Step::completed(FinishReason::ToolCalls)])
                .collect::<Vec<_>>(),
        ),
        Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, "done"),
            Step::completed(FinishReason::Stop),
        ]),
    ]));
    let mut config = DriverConfig::default();
    config.runtime.context.catalog.insert(
        model,
        ModelLimits {
            context_tokens: Some(128_000),
            max_output_tokens: None,
        },
    );
    let host = ToolHostConfig {
        workspace_root: ws.clone(),
        data_root: data.clone(),
        sandbox: None,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    runtime.block_on(async {
        let handle = SessionHandle::create_with_tools(
            root.clone(),
            SessionId::new(),
            header,
            provider,
            config,
            &host,
        )
        .await?;
        handle
            .command(Command::StartTurn {
                command_id: CommandId::new(),
                content: vec![ContentBlock::Text {
                    text: "read twice".into(),
                }],
            })
            .await
            .map_err(|e| format!("{e:?}"))?;
        loop {
            let view = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let view = handle.view().await.map_err(|_| "view")?;
                    if view.pending_approval.is_some()
                        || view.phase == Phase::Durable(RuntimeState::Idle)
                    {
                        return Ok::<_, &str>(view);
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await??;
            let Some(ticket) = view.pending_approval else {
                break;
            };
            handle
                .command(Command::SubmitApproval {
                    approval_id: ticket.approval_id,
                    plan_digest: ticket.plan_digest,
                    decision: Decision::Allow,
                    remember: false,
                })
                .await
                .map_err(|e| format!("{e:?}"))?;
        }
        handle.shutdown().await;
        Ok::<(), Box<dyn std::error::Error>>(())
    })?;
    let mut metrics = skeleton("openai", "selftest", "offline");
    let mut sandbox_paths = Vec::new();
    let answer = journal(&data, &mut metrics, &mut sandbox_paths)?;
    assert_eq!(answer.trim(), "done", "assistant text extraction");
    assert_eq!(metrics["inferences"], 2, "inference count");
    assert_eq!(metrics["tool_calls"]["read"], 2, "tool counts");
    assert_eq!(
        metrics["tool_calls"]["multi_edit"], 1,
        "multi_edit tool count"
    );
    assert_eq!(metrics["approvals_by_tool"]["read"], 2, "approvals by tool");
    assert_eq!(
        metrics["approvals_by_tool"]["multi_edit"], 1,
        "multi_edit approval count"
    );
    assert_eq!(metrics["result_code"], "completed", "result code");
    assert!(metrics["turn_wall_ms"].is_number(), "turn wall time");
    assert_eq!(metrics["readonly_segments"], 1, "one read-only segment");
    assert_eq!(
        metrics["readonly_segments_detail"][0]["calls"], 2,
        "segment call count"
    );
    assert_eq!(
        metrics["tool_facts"][0]["tool"], "read",
        "journal-derived tool facts"
    );
    // This configuration records no `[sandbox]` table, so the summary stays off
    // and no path is offered for redaction.
    assert_eq!(metrics["sandbox"]["configured"], false, "sandbox summary");
    assert!(sandbox_paths.is_empty(), "no recorded mounts");
    // The future multi_edit tool is a mutation and remains in the other class.
    approval_split(
        &mut metrics,
        "tool: read {\napprove? [y/N] y\napproval: read-only auto-approved\ntool: multi_edit {\napprove? [y/N] y\n",
    );
    assert_eq!(
        metrics["approval_prompts"], 2,
        "read and multi_edit prompts"
    );
    assert_eq!(
        metrics["approval_classes"]["read_only"]["prompts"], 1,
        "one read-only prompt"
    );
    assert_eq!(
        metrics["approval_classes"]["read_only"]["auto_approved"], 1,
        "one auto-approval"
    );
    assert_eq!(
        metrics["approval_classes"]["other"]["prompts"], 1,
        "one other prompt"
    );
    assert_eq!(metrics["approval_classes"]["other"]["requests"], 1);
    assert_eq!(
        metrics["approval_split_consistent"], true,
        "split reconstructs the prompts"
    );
    println!("journal self-test: real read and multi_edit counts, approval split");
    let undo = undo_metrics(binary, &data, &ws, &pristine);
    assert_eq!(undo["exit_code"], 0, "trusted multi_edit undo exit");
    assert_eq!(
        undo["workspace_matches_pristine"], true,
        "trusted multi_edit restored"
    );
    assert_eq!(
        undo["trusted_edits_reverted"], true,
        "trusted multi_edit undo proof"
    );
    println!("undo self-test: trusted multi_edit restored the original workspace");
    temp.close()?;
    Ok(())
}

fn rename_fixture_words(source: &str) -> (String, usize) {
    replace_word(source, "item", "product")
}

fn replace_word(source: &str, from: &str, to: &str) -> (String, usize) {
    let chars: Vec<char> = source.chars().collect();
    let word: Vec<char> = from.chars().collect();
    let mut output = String::with_capacity(source.len());
    let mut index = 0;
    let mut replaced = 0;
    while index < chars.len() {
        let end = index + word.len();
        let matches = end <= chars.len() && chars[index..end] == word;
        let left_boundary = index == 0 || !identifier_char(chars[index - 1]);
        let right_boundary = end >= chars.len() || !identifier_char(chars[end]);
        if matches && left_boundary && right_boundary {
            output.push_str(to);
            index = end;
            replaced += 1;
        } else {
            output.push(chars[index]);
            index += 1;
        }
    }
    (output, replaced)
}

fn identifier_char(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

fn undo_metrics(binary: &Path, data: &Path, workspace: &Path, pristine: &Path) -> Value {
    if !data.is_dir() {
        return json!({"attempted":false,"result":"no_session"});
    }
    let Ok(root) = DataRoot::open(data, false) else {
        return json!({"attempted":false,"result":"data_root_unavailable"});
    };
    let Ok(sessions) = root.sessions() else {
        return json!({"attempted":false,"result":"journal_unavailable"});
    };
    let Some(session) = sessions.first() else {
        return json!({"attempted":false,"result":"no_session"});
    };
    let output = Command::new(binary)
        .args(["--no-config", "sessions", "--data-root"])
        .arg(data)
        .arg("undo")
        .arg(session.session_id.to_string())
        .arg("--yes")
        .output();
    let Ok(output) = output else {
        return json!({"attempted":true,"result":"undo_command_unavailable"});
    };
    let matches = workspace_matches_fixture(workspace, pristine).unwrap_or(false);
    json!({
        "attempted": true,
        "exit_code": output.status.code(),
        "workspace_matches_pristine": matches,
        "trusted_edits_reverted": output.status.success() && matches,
    })
}

fn workspace_matches_fixture(workspace: &Path, pristine: &Path) -> Result<bool> {
    let before = oracle::tree(pristine)?;
    let mut after = oracle::tree(workspace)?;
    if !before.contains_key("Cargo.lock") {
        after.remove("Cargo.lock");
    }
    Ok(before == after)
}

/// Offline proof that `--config FILE` reaches the harness command line and that
/// the default still sends `--no-config` (decision 15): a stand-in binary prints
/// its own argv inside the same PTY path a live cell uses.
fn config_selftest(scratch: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::Builder::new()
        .prefix("argv-")
        .tempdir_in(scratch)?;
    let stub = temp.path().join("argv.sh");
    fs::write(
        &stub,
        "#!/bin/sh\nfor a in \"$@\"; do printf 'ARGV:%s\\n' \"$a\"; done\n",
    )?;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755))?;
    let ws = temp.path().join("ws");
    let data = temp.path().join("data");
    fs::create_dir(&ws)?;
    let config = temp.path().join("private.toml");
    fs::write(&config, "approve_read_only = true\n")?;
    let config_arg = format!("ARGV:{}", config.display());

    let with = ask(&stub, "openai", "PROMPT", &ws, &data, Some(&config))?;
    assert!(with.transcript.contains("ARGV:--config"));
    assert!(
        with.transcript.contains(&config_arg),
        "config path forwarded"
    );
    assert!(!with.transcript.contains("ARGV:--no-config"));
    assert!(with.transcript.contains("ARGV:ask"));
    assert!(with.transcript.contains("ARGV:openai"));

    let without = ask(&stub, "openai", "PROMPT", &ws, &data, None)?;
    assert!(without.transcript.contains("ARGV:--no-config"));
    assert!(!without.transcript.contains("ARGV:--config"));
    println!("config self-test: --config FILE forwarded, default stays --no-config");
    temp.close()?;
    Ok(())
}

/// The driver's approvals arrive on `/dev/tty`, so prove the PTY we open really
/// becomes the child's controlling terminal before spending a live run on it.
fn tty_selftest() -> Result<()> {
    let pair = native_pty_system().openpty(PtySize {
        rows: 10,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.args([
        "-c",
        "if exec 3<>/dev/tty; then echo tty-opened; else echo tty-missing; fi",
    ]);
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader()?;
    let mut output = String::new();
    let mut buf = [0; 256];
    while let Ok(n) = reader.read(&mut buf) {
        if n == 0 {
            break;
        }
        output.push_str(&String::from_utf8_lossy(&buf[..n]));
        if output.contains("tty-opened") || output.contains("tty-missing") {
            break;
        }
    }
    let _ = child.wait();
    assert!(
        output.contains("tty-opened"),
        "the PTY child has no controlling terminal; approvals cannot use /dev/tty"
    );
    println!("pty self-test: /dev/tty opens inside the spawned PTY");
    Ok(())
}
