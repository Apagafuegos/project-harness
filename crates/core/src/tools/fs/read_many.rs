//! Secure line and multi-file reads on the existing descriptor-rooted executor.

use rustix::{
    fd::{AsFd, OwnedFd},
    fs::Stat,
};

use super::{
    FsWorkspace,
    execute::Target,
    resolve::{Control, hash, read_range, stale, verify_unchanged},
};
use crate::{
    session::ResultCode,
    tools::{
        Refusal, WorkspacePath,
        read_many::{
            LineWindow, MAX_RESULT_BYTES, MULTI_HEADER_RESERVE, ReadOutput, ReadRange, ReadSpec,
            SINGLE_HEADER_RESERVE, inward_utf8, line_window, multi_payload_budget, render_many,
        },
    },
};

struct OpenRead {
    file: OwnedFd,
    stat: Stat,
}

struct ReadSlice {
    start: u64,
    end: u64,
    more: bool,
    selected_lines: Option<u64>,
    requested_lines: Option<(u64, u64)>,
}

struct ReadResult {
    start: u64,
    end: u64,
    more: bool,
    selected_lines: Option<u64>,
    requested_lines: Option<(u64, u64)>,
    text: String,
    truncated: bool,
}

impl FsWorkspace {
    pub(super) fn execute_single_lines(
        &self,
        path: &WorkspacePath,
        start_line: u64,
        line_count: u64,
        max_bytes: u64,
        target: &Target,
        control: &mut Control<'_>,
    ) -> Result<(String, bool), Refusal> {
        if target.path != path.as_str() {
            return Err(internal("read target mismatch"));
        }
        let spec = ReadSpec {
            path: path.clone(),
            range: ReadRange::Lines {
                start_line,
                line_count,
                max_bytes,
            },
        };
        let output = self.execute_reads(&[spec], std::slice::from_ref(target), control)?;
        let output = output
            .into_iter()
            .next()
            .ok_or_else(|| internal("single read result missing"))?;
        let header = output.header.strip_prefix("[0] ").unwrap_or(&output.header);
        let text = format!("{header}\n{}", output.text);
        if text.len() > MAX_RESULT_BYTES || header.len() + 1 > SINGLE_HEADER_RESERVE {
            return Err(internal("single read output exceeded its bound"));
        }
        Ok((text, output.truncated))
    }

    pub(super) fn execute_many(
        &self,
        specs: &[ReadSpec],
        targets: &[Target],
        control: &mut Control<'_>,
    ) -> Result<(String, bool), Refusal> {
        let outputs = self.execute_reads(specs, targets, control)?;
        render_many(&outputs)
    }

    fn execute_reads(
        &self,
        specs: &[ReadSpec],
        targets: &[Target],
        control: &mut Control<'_>,
    ) -> Result<Vec<ReadOutput>, Refusal> {
        if specs.is_empty() || specs.len() != targets.len() || specs.len() > 8 {
            return Err(internal("read target count mismatch"));
        }
        let payload_budget = if specs.len() == 1 {
            MAX_RESULT_BYTES - SINGLE_HEADER_RESERVE
        } else {
            multi_payload_budget(specs.len())?
        };

        // Resolve every file and reject protected, unsupported, or multiply
        // linked objects before hashing or returning bytes from any file.
        let mut opened = Vec::with_capacity(specs.len());
        for (spec, target) in specs.iter().zip(targets) {
            if spec.path != target.path {
                return Err(internal("read path binding mismatch"));
            }
            let resolved = self.resolve(&spec.path, control)?;
            let (file, stat) = self.regular(&resolved, false, control)?;
            opened.push(OpenRead { file, stat });
        }
        for (read, target) in opened.iter().zip(targets) {
            if hash(&read.file, &read.stat, control)?.as_str() != target.pre_image {
                return Err(stale());
            }
        }

        let mut remaining = payload_budget;
        let mut outputs = Vec::with_capacity(specs.len());
        for (index, (spec, opened)) in specs.iter().zip(opened.iter()).enumerate() {
            let result = read_slice(&opened.file, &opened.stat, &spec.range, remaining, control)?;
            remaining = remaining.saturating_sub(result.text.len());
            let header = output_header(index, &spec.range, &result);
            outputs.push(ReadOutput {
                header,
                text: result.text,
                truncated: result.truncated,
            });
        }
        if specs.len() > 1 {
            let actual_headers = outputs
                .iter()
                .map(|output| output.header.len() + 1)
                .sum::<usize>();
            if actual_headers > specs.len() * MULTI_HEADER_RESERVE {
                return Err(internal("multi-read headers exceeded their bound"));
            }
        }
        Ok(outputs)
    }
}

fn read_slice(
    file: &OwnedFd,
    stat: &Stat,
    range: &ReadRange,
    payload_budget: usize,
    control: &mut Control<'_>,
) -> Result<ReadResult, Refusal> {
    let file_len = stat.st_size as u64;
    let (slice, max_bytes) = match range {
        ReadRange::Bytes {
            offset_bytes,
            max_bytes,
        } => {
            let start = (*offset_bytes).min(file_len);
            let end = start.saturating_add(*max_bytes).min(file_len);
            (
                ReadSlice {
                    start,
                    end,
                    more: end < file_len,
                    selected_lines: None,
                    requested_lines: None,
                },
                *max_bytes,
            )
        }
        ReadRange::Lines {
            start_line,
            line_count,
            max_bytes,
        } => {
            let window = line_window(file_len, *start_line, *line_count, |offset, count| {
                read_range(file, offset, count, control)
            })?;
            (
                ReadSlice::from_lines(window, *start_line, *line_count),
                *max_bytes,
            )
        }
    };
    let requested_end = slice.start.saturating_add(max_bytes).min(slice.end);
    let cap_end = slice
        .start
        .saturating_add(payload_budget as u64)
        .min(requested_end);
    let mut text = String::new();
    let mut consumed_start = slice.start;
    let mut consumed_end = slice.start;
    if cap_end > slice.start {
        // Three context bytes distinguish a split codepoint from malformed
        // UTF-8; context bytes are never included in the returned payload.
        let context_start = slice.start.saturating_sub(3);
        let context_end = cap_end.saturating_add(3).min(file_len);
        let bytes = read_range(
            file,
            context_start,
            (context_end - context_start) as usize,
            control,
        )?;
        let (adjusted_start, valid) = inward_utf8(
            &bytes,
            (slice.start - context_start) as usize,
            (cap_end - context_start) as usize,
        )?;
        consumed_start = context_start + adjusted_start as u64;
        consumed_end = consumed_start + valid.len() as u64;
        text.push_str(valid);
    }
    verify_unchanged(file.as_fd(), stat, control)?;
    let line_range_truncated = slice.selected_lines.is_some() && requested_end < slice.end;
    let truncated = line_range_truncated || cap_end < requested_end || consumed_end < cap_end;
    Ok(ReadResult {
        start: consumed_start,
        end: consumed_end,
        more: slice.more || consumed_end < file_len,
        selected_lines: slice.selected_lines,
        requested_lines: slice.requested_lines,
        text,
        truncated,
    })
}

impl ReadSlice {
    fn from_lines(window: LineWindow, start_line: u64, line_count: u64) -> Self {
        Self {
            start: window.start,
            end: window.end,
            more: window.more,
            selected_lines: Some(window.selected_lines),
            requested_lines: Some((start_line, line_count)),
        }
    }
}

fn output_header(index: usize, range: &ReadRange, result: &ReadResult) -> String {
    let position = match (range, result.selected_lines) {
        (ReadRange::Lines { .. }, Some(selected)) => {
            let (start_line, _) = result.requested_lines.expect("line range metadata");
            format!(
                "lines {start_line}+{selected}; bytes {}..{}",
                result.start, result.end
            )
        }
        _ => format!("bytes {}..{}", result.start, result.end),
    };
    format!(
        "[{index}] {position}; more={}; output_truncated={}",
        result.more, result.truncated
    )
}

fn internal(detail: &str) -> Refusal {
    Refusal::new(ResultCode::InfrastructureFailure, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::read_many::{MAX_RESULT_BYTES, ReadOutput, render_many};

    #[test]
    fn every_multi_item_reserves_its_header_before_payload_budgeting() {
        let outputs = (0..8)
            .map(|index| ReadOutput {
                header: format!("[{index}] lines 10000+10000; bytes 18446744073709551615..18446744073709551615; more=true; output_truncated=true"),
                text: String::new(),
                truncated: true,
            })
            .collect::<Vec<_>>();
        let (text, _) = render_many(&outputs).unwrap();
        assert!(text.len() <= MAX_RESULT_BYTES);
    }
}
