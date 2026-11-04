//! Descriptor-rooted search. No ambient filesystem walker or pathname reopen.
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rustix::fs::{self, FileType};
use serde_json::{Value, json};

use super::{
    FsWorkspace,
    resolve::{Control, DirectoryEntry, EntryKind, Step, errno, read_bounded},
};
use crate::{
    session::{ContentBlock, ExecutionBackend, ResultCode, ToolStatus},
    tools::{
        CancelToken, ExecutionOutcome, Plan, Protection, Refusal, WorkspacePath,
        builtin::{MIB, Operation},
        search::{self, DIRECTORIES, ENTRIES, IGNORE_BYTES, LINE_BYTES, SCAN_BYTES},
    },
};

#[derive(Default)]
struct SearchResult {
    rows: Vec<Value>,
    limits: Vec<&'static str>,
    warnings: Vec<&'static str>,
    directories: usize,
    scanned_bytes: u64,
    skipped_large: usize,
    skipped_binary: usize,
    skipped_utf8: usize,
    skipped_unreadable: usize,
    matches: usize,
}
impl SearchResult {
    fn limit(&mut self, name: &'static str) {
        if !self.limits.contains(&name) {
            self.limits.push(name);
        }
    }
    fn warning(&mut self, warning: &'static str) {
        if !self.warnings.contains(&warning) {
            self.warnings.push(warning);
        }
    }
    fn value(&self, key: &str) -> Value {
        json!({key: self.rows, "count":self.rows.len(),"match_count":self.matches,"truncated":!self.limits.is_empty(),"limits":self.limits,"warnings":self.warnings,"directories_visited":self.directories,"scanned_bytes":self.scanned_bytes,"skipped":{"oversized":self.skipped_large,"binary":self.skipped_binary,"non_utf8":self.skipped_utf8,"unreadable":self.skipped_unreadable}})
    }
}

struct WalkOptions<'a> {
    base: &'a WorkspacePath,
    pattern: Option<&'a str>,
    include_ignored: bool,
}
struct GrepOptions<'a> {
    regex: &'a regex::Regex,
    output: &'a str,
    context: usize,
    max: usize,
}

impl FsWorkspace {
    pub(super) fn search_hidden(&self, path: &WorkspacePath) -> bool {
        path.as_str().split('/').any(|name| name == ".git")
            || self.excluded.iter().any(|excluded| path.beneath(excluded))
    }

    pub(super) fn execute_search(
        &self,
        plan: &Plan,
        cancel: &CancelToken,
        hook: &mut dyn FnMut(Step) -> Result<(), Refusal>,
        timeout: Option<Duration>,
        started: Instant,
    ) -> ExecutionOutcome {
        let mut result = SearchResult::default();
        let mut control = Control {
            started,
            timeout: Some(timeout.unwrap_or(Duration::from_millis(plan.timeout_ms()))),
            cancellation: Some(cancel),
            hook,
        };
        let key = match plan.operation() {
            Operation::List { .. } => "entries",
            Operation::Grep { output, .. } if output == "matches" => "matches",
            _ => "paths",
        };
        let execution = (|| {
            let binding: Value = serde_json::from_slice(plan.bytes()).map_err(|_| {
                Refusal::new(
                    ResultCode::InfrastructureFailure,
                    "invalid internal search binding",
                )
            })?;
            if plan.backend() != ExecutionBackend::TrustedFs
                || binding["workspace"].as_str() != Some(self.identity.0.as_str())
            {
                return Err(Refusal::new(
                    ResultCode::InfrastructureFailure,
                    "plan workspace mismatch",
                ));
            }
            let path = plan.operation().path().expect("search path");
            if self.search_hidden(path) {
                return Err(Refusal::new(
                    ResultCode::ProtectedPath,
                    "target or directory is protected",
                ));
            }
            let resolved = self.resolve(path, &mut control)?;
            if resolved.protection != Protection::None {
                return Err(Refusal::new(
                    ResultCode::ProtectedPath,
                    "target or directory is protected",
                ));
            }
            let object = resolved
                .object
                .as_ref()
                .ok_or_else(|| Refusal::new(ResultCode::NotFound, "search path absent"))?;
            let stat = fs::fstat(object).map_err(errno)?;
            let include_ignored = match plan.operation() {
                Operation::Glob {
                    include_ignored, ..
                }
                | Operation::Grep {
                    include_ignored, ..
                } => *include_ignored,
                _ => false,
            };
            let mut ignores = Vec::new();
            if !include_ignored {
                self.ancestor_ignores(path, &mut ignores, &mut result, &mut control)?;
            }
            match plan.operation() {
                Operation::List { path } => {
                    self.load_ignore(path, &mut ignores, &mut result, &mut control)?;
                    let (entries, limited) = self.directory_entries(
                        path,
                        ENTRIES,
                        |entry| !ignored(&ignores, entry),
                        &mut control,
                    )?;
                    result.directories = 1;
                    for entry in entries {
                        let kind = match entry.kind {
                            EntryKind::File => "file",
                            EntryKind::Directory => "dir",
                            EntryKind::Symlink => "symlink",
                        };
                        let mut value =
                            json!({"name":display_name(entry.name.to_bytes()),"type":kind});
                        if entry.kind == EntryKind::File {
                            value["size"] = entry.size.into();
                        }
                        result.rows.push(value);
                    }
                    if limited {
                        result.limit("entries:1000");
                    }
                }
                Operation::Glob {
                    pattern,
                    path,
                    include_ignored,
                } => {
                    let matcher = search::glob_matcher(pattern)?;
                    self.walk(
                        WalkOptions {
                            base: path,
                            pattern: Some(pattern),
                            include_ignored: *include_ignored,
                        },
                        ignores,
                        &mut result,
                        &mut control,
                        &mut |entry, result, _control| {
                            if matcher
                                .matched_path_or_any_parents(relative(path, entry), false)
                                .is_none()
                            {
                                return Ok(true);
                            }
                            if result.rows.len() == ENTRIES {
                                result.limit("paths:1000");
                                return Ok(false);
                            }
                            result.rows.push(json!(entry.as_str()));
                            Ok(true)
                        },
                    )?;
                }
                Operation::Grep {
                    pattern,
                    path,
                    glob,
                    case_insensitive,
                    output,
                    context_lines,
                    max_results,
                    include_ignored,
                } => {
                    let regex = search::regex(pattern, *case_insensitive)?;
                    let filter = glob.as_deref().map(search::glob_matcher).transpose()?;
                    let mut visit =
                        |entry: &WorkspacePath,
                         result: &mut SearchResult,
                         control: &mut Control<'_>| {
                            if filter.as_ref().is_some_and(|filter| {
                                filter
                                    .matched_path_or_any_parents(relative(path, entry), false)
                                    .is_none()
                            }) {
                                return Ok(true);
                            }
                            self.grep_file(
                                entry,
                                &GrepOptions {
                                    regex: &regex,
                                    output,
                                    context: *context_lines as usize,
                                    max: *max_results as usize,
                                },
                                result,
                                control,
                            )
                        };
                    if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile {
                        let ignored = !*include_ignored
                            && ignores.iter().rev().any(|matcher| {
                                matcher
                                    .matched_path_or_any_parents(path.as_str(), false)
                                    .is_ignore()
                            });
                        if !ignored {
                            visit(path, &mut result, &mut control)?;
                        }
                    } else {
                        self.walk(
                            WalkOptions {
                                base: path,
                                pattern: glob.as_deref(),
                                include_ignored: *include_ignored,
                            },
                            ignores,
                            &mut result,
                            &mut control,
                            &mut visit,
                        )?;
                    }
                }
                _ => unreachable!(),
            }
            Ok(())
        })();
        let (status, code) = match execution {
            Ok(()) => (ToolStatus::Succeeded, None),
            Err(error) => match error.code {
                ResultCode::Cancelled => {
                    result.limit("cancelled");
                    (ToolStatus::Cancelled, Some(error.code))
                }
                ResultCode::Timeout => {
                    result.limit("deadline:30000ms");
                    (ToolStatus::TimedOut, Some(error.code))
                }
                ResultCode::InfrastructureFailure => {
                    result.limit("infrastructure_failure");
                    (ToolStatus::InfrastructureFailure, Some(error.code))
                }
                _ => {
                    result.limit("walk_unavailable");
                    (ToolStatus::Failed, Some(error.code))
                }
            },
        };
        let mut value = result.value(key);
        let full = serde_json::to_vec(&value).expect("search result JSON");
        let mut artifacts = Vec::new();
        if serde_json::to_vec(&result_content(&value))
            .expect("content JSON")
            .len()
            > 32768
        {
            artifacts.push(full);
            let mut rows = value[key].as_array().expect("rows").clone();
            value["captured"] = true.into();
            value["truncated"] = true.into();
            value["inline_omitted"] = 0.into();
            value["limits"]
                .as_array_mut()
                .expect("limits")
                .push(json!("inline_bytes:32768; full output in artifact"));
            // Account for the ContentBlock wrapper and JSON escaping too.
            while serde_json::to_vec(&result_content(&value))
                .expect("content JSON")
                .len()
                > 32768
            {
                rows.truncate(rows.len() / 2);
                value[key] = json!(rows);
                value["inline_omitted"] = (result.rows.len() - rows.len()).into();
            }
        }
        ExecutionOutcome {
            status,
            code,
            content: result_content(&value),
            exit_code: None,
            duration_ms: Some(started.elapsed().as_millis().min(u64::MAX as u128) as u64),
            output_truncated: value["truncated"] == true,
            artifacts,
            file_effect: None,
        }
    }

    fn ancestor_ignores(
        &self,
        path: &WorkspacePath,
        ignores: &mut Vec<Gitignore>,
        result: &mut SearchResult,
        control: &mut Control<'_>,
    ) -> Result<(), Refusal> {
        if path.as_str() == "." {
            return Ok(());
        }
        self.load_ignore(&WorkspacePath::new(".")?, ignores, result, control)?;
        let mut prefix = String::new();
        let components: Vec<_> = path.as_str().split('/').collect();
        for component in &components[..components.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            self.load_ignore(&WorkspacePath::new(&prefix)?, ignores, result, control)?;
        }
        Ok(())
    }

    fn load_ignore(
        &self,
        directory: &WorkspacePath,
        ignores: &mut Vec<Gitignore>,
        result: &mut SearchResult,
        control: &mut Control<'_>,
    ) -> Result<(), Refusal> {
        let name = if directory.as_str() == "." {
            ".gitignore".to_owned()
        } else {
            format!("{}/.gitignore", directory.as_str())
        };
        let Ok(path) = WorkspacePath::new(&name) else {
            return Ok(());
        };
        if self.search_hidden(&path) {
            return Ok(());
        }
        let read = (|| {
            let resolved = self.resolve(&path, control)?;
            if resolved.protection != Protection::None || resolved.object.is_none() {
                return Ok(None);
            }
            let (file, stat) = self.regular(&resolved, false, control)?;
            let bytes = read_bounded(&file, &stat, IGNORE_BYTES, control)?;
            String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| Refusal::new(ResultCode::NotUtf8, "ignore file is not UTF-8"))
        })();
        let text = match read {
            Ok(Some(text)) => text,
            Ok(None) => return Ok(()),
            Err(error) if matches!(error.code, ResultCode::Cancelled | ResultCode::Timeout) => {
                return Err(error);
            }
            // Protected paths are never revealed, not even by a warning.
            Err(error) if error.code == ResultCode::ProtectedPath => return Ok(()),
            Err(_) => {
                result.warning("unreadable or oversized .gitignore ignored (65536-byte cap)");
                return Ok(());
            }
        };
        let mut builder = GitignoreBuilder::new(if directory.as_str() == "." {
            ""
        } else {
            directory.as_str()
        });
        for (index, line) in text.lines().enumerate() {
            let line = if index == 0 {
                line.trim_start_matches('\u{feff}')
            } else {
                line
            };
            if builder.add_line(None, line).is_err() {
                result.warning("invalid .gitignore rule ignored");
            }
        }
        match builder.build() {
            Ok(matcher) => ignores.push(matcher),
            Err(_) => result.warning("invalid .gitignore ignored"),
        }
        Ok(())
    }

    fn walk(
        &self,
        options: WalkOptions<'_>,
        ignores: Vec<Gitignore>,
        result: &mut SearchResult,
        control: &mut Control<'_>,
        visit: &mut dyn FnMut(
            &WorkspacePath,
            &mut SearchResult,
            &mut Control<'_>,
        ) -> Result<bool, Refusal>,
    ) -> Result<(), Refusal> {
        let WalkOptions {
            base,
            pattern,
            include_ignored,
        } = options;
        // A byte-sorted frontier makes bounded results independent of readdir order,
        // including punctuation that sorts before '/'. Ignore stacks follow directories.
        enum Pending {
            Directory(Vec<Gitignore>),
            File,
        }
        let mut pending = BTreeMap::new();
        pending.insert(base.as_str().to_owned(), Pending::Directory(ignores));
        while let Some((name, item)) = pending.pop_first() {
            control.check(Step::WalkEntry)?;
            let path = WorkspacePath::new(&name)?;
            match item {
                Pending::File => {
                    if !visit(&path, result, control)? {
                        break;
                    }
                }
                Pending::Directory(mut ignores) => {
                    control.check(Step::WalkDirectory)?;
                    if result.directories == DIRECTORIES {
                        result.limit("directories:20000");
                        break;
                    }
                    // Resolve once more immediately before entry; symlink swaps cannot use
                    // an earlier directory descriptor to walk outside the workspace.
                    let resolved = match self.resolve(&path, control) {
                        Ok(resolved) => resolved,
                        Err(error)
                            if matches!(
                                error.code,
                                ResultCode::Cancelled | ResultCode::Timeout
                            ) =>
                        {
                            return Err(error);
                        }
                        Err(_) => {
                            result.limit("walk_unavailable");
                            continue;
                        }
                    };
                    if self.search_hidden(&path) || resolved.protection != Protection::None {
                        continue;
                    }
                    result.directories += 1;
                    if !include_ignored {
                        self.load_ignore(&path, &mut ignores, result, control)?;
                    }
                    let entries = match self.directory_entries(
                        &path,
                        usize::MAX,
                        |entry| {
                            entry.kind != EntryKind::Symlink
                                && (entry.kind != EntryKind::Directory
                                    || entry.name.to_str().is_ok_and(|n| {
                                        !n.starts_with('.')
                                            || search::names_dot_directory(pattern, n)
                                    }))
                                && (include_ignored || !ignored(&ignores, entry))
                        },
                        control,
                    ) {
                        Ok((entries, _)) => entries,
                        Err(error)
                            if matches!(
                                error.code,
                                ResultCode::Cancelled | ResultCode::Timeout
                            ) =>
                        {
                            return Err(error);
                        }
                        Err(_) => {
                            result.limit("walk_unavailable");
                            continue;
                        }
                    };
                    for entry in entries {
                        let Some(path) = entry.path else {
                            if entry.name.to_str().is_ok() {
                                result.limit("path_bytes:4096");
                            }
                            continue;
                        };
                        pending.insert(
                            path.as_str().to_owned(),
                            if entry.kind == EntryKind::Directory {
                                Pending::Directory(ignores.clone())
                            } else {
                                Pending::File
                            },
                        );
                    }
                }
            }
        }
        Ok(())
    }

    fn grep_file(
        &self,
        path: &WorkspacePath,
        options: &GrepOptions<'_>,
        result: &mut SearchResult,
        control: &mut Control<'_>,
    ) -> Result<bool, Refusal> {
        let GrepOptions {
            regex,
            output,
            context,
            max,
        } = *options;
        control.check(Step::Read)?;
        if self.search_hidden(path) {
            return Ok(true);
        }
        let read: Result<Option<Vec<u8>>, Refusal> = (|| {
            let resolved = self.resolve(path, control)?;
            if resolved.protection != Protection::None {
                return Ok(None);
            }
            let (file, stat) = self.regular(&resolved, false, control)?;
            if stat.st_size as u64 > MIB {
                result.skipped_large += 1;
                return Ok(None);
            }
            if result.scanned_bytes + stat.st_size as u64 > SCAN_BYTES {
                result.limit("scanned_bytes:67108864");
                return Ok(None);
            }
            let bytes = read_bounded(&file, &stat, MIB, control)?;
            result.scanned_bytes += bytes.len() as u64;
            Ok(Some(bytes))
        })();
        let bytes = match read {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(!result.limits.contains(&"scanned_bytes:67108864")),
            Err(error) if matches!(error.code, ResultCode::Cancelled | ResultCode::Timeout) => {
                return Err(error);
            }
            // A protected path is absent from the walk, not an unreadable skip.
            Err(error) if error.code == ResultCode::ProtectedPath => return Ok(true),
            Err(_) => {
                result.skipped_unreadable += 1;
                return Ok(true);
            }
        };
        if bytes[..bytes.len().min(8192)].contains(&0) {
            result.skipped_binary += 1;
            return Ok(true);
        }
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(_) => {
                result.skipped_utf8 += 1;
                return Ok(true);
            }
        };
        let lines: Vec<_> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            control.check(Step::Read)?;
            if !regex.is_match(line) {
                continue;
            }
            if result.matches == max {
                result.limit(if output == "files" {
                    "max_results:files"
                } else {
                    "max_results:matches"
                });
                return Ok(false);
            }
            result.matches += 1;
            if output == "files" {
                result.rows.push(json!(path.as_str()));
                break;
            }
            let context_before: Vec<_> = (index.saturating_sub(context)..index)
                .map(|i| line_value(i, lines[i]))
                .collect();
            let context_after: Vec<_> = (index + 1..(index + context + 1).min(lines.len()))
                .map(|i| line_value(i, lines[i]))
                .collect();
            if lines[index.saturating_sub(context)..(index + context + 1).min(lines.len())]
                .iter()
                .any(|line| line.len() > LINE_BYTES)
            {
                result.limit("line_bytes:500");
            }
            let mut row = line_value(index, line);
            row["path"] = json!(path.as_str());
            if context > 0 {
                row["before"] = json!(context_before);
                row["after"] = json!(context_after);
            }
            result.rows.push(row);
        }
        Ok(true)
    }
}
fn relative<'a>(base: &WorkspacePath, path: &'a WorkspacePath) -> &'a str {
    if base.as_str() == "." || base == path {
        path.as_str()
    } else {
        path.as_str()
            .strip_prefix(base.as_str())
            .and_then(|p| p.strip_prefix('/'))
            .expect("walk beneath base")
    }
}
fn ignored(ignores: &[Gitignore], entry: &DirectoryEntry) -> bool {
    for matcher in ignores.iter().rev() {
        let matched = matcher
            .matched_path_or_any_parents(&entry.raw_path, entry.kind == EntryKind::Directory);
        if matched.is_ignore() {
            return true;
        }
        if matched.is_whitelist() {
            return false;
        }
    }
    false
}
fn line_value(index: usize, line: &str) -> Value {
    let mut end = line.len().min(LINE_BYTES);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = end < line.len();
    json!({"line":index+1,"text":if truncated { format!("{}[truncated]",&line[..end]) } else { line.to_owned() },"text_truncated":truncated})
}
fn display_name(bytes: &[u8]) -> String {
    if let Ok(name) = std::str::from_utf8(bytes) {
        return name.into();
    }
    let mut text = String::new();
    for &byte in bytes {
        match byte {
            b'\\' => text.push_str("\\\\"),
            0x20..=0x7e => text.push(byte as char),
            _ => text.push_str(&format!("\\x{byte:02x}")),
        }
    }
    text
}

fn result_content(value: &Value) -> Vec<ContentBlock> {
    vec![ContentBlock::Text {
        text: serde_json::to_string(value).expect("search JSON"),
    }]
}
