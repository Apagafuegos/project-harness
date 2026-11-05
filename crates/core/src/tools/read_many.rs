//! Strict argument parsing and bounded output helpers for line and batched reads.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{Refusal, WorkspacePath};
use crate::session::ResultCode;

pub(super) const MAX_PATHS: usize = 8;
pub(super) const MAX_LINE_COUNT: u64 = 10_000;
pub(super) const MAX_RANGE_BYTES: u64 = 1024 * 1024;
pub(super) const MAX_RESULT_BYTES: usize = 32 * 1024;
pub(super) const SINGLE_HEADER_RESERVE: usize = 256;
pub(super) const MULTI_HEADER_RESERVE: usize = 192;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(super) enum ReadRange {
    Bytes {
        offset_bytes: u64,
        max_bytes: u64,
    },
    Lines {
        start_line: u64,
        line_count: u64,
        max_bytes: u64,
    },
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(super) struct ReadSpec {
    pub(super) path: WorkspacePath,
    pub(super) range: ReadRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ReadRequest {
    Single(ReadSpec),
    Many(Vec<ReadSpec>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LineWindow {
    pub(super) start: u64,
    pub(super) end: u64,
    pub(super) more: bool,
    pub(super) selected_lines: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReadOutput {
    pub(super) header: String,
    pub(super) text: String,
    pub(super) truncated: bool,
}

/// The tool schema is closed at both the top level and each `paths` item.
pub(super) fn schema(path: Value) -> Value {
    let byte_fields = json!({
        "offset_bytes": {"type":"integer", "minimum":0, "maximum":u64::MAX},
        "max_bytes": {"type":"integer", "minimum":0, "maximum":MAX_RANGE_BYTES}
    });
    let line_fields = json!({
        "start_line": {"type":"integer", "minimum":1, "maximum":u64::MAX},
        "line_count": {"type":"integer", "minimum":1, "maximum":MAX_LINE_COUNT}
    });
    let mut item_properties = Map::new();
    item_properties.insert("path".into(), path.clone());
    item_properties.insert("offset_bytes".into(), byte_fields["offset_bytes"].clone());
    item_properties.insert("max_bytes".into(), byte_fields["max_bytes"].clone());
    item_properties.insert("start_line".into(), line_fields["start_line"].clone());
    item_properties.insert("line_count".into(), line_fields["line_count"].clone());
    let item = json!({
        "type":"object",
        "properties":item_properties,
        "required":["path"],
        "additionalProperties":false
    });

    let mut properties = item_properties;
    properties.insert(
        "paths".into(),
        json!({"type":"array", "minItems":1, "maxItems":MAX_PATHS, "items":item}),
    );
    json!({
        "type":"object",
        "properties":properties,
        "oneOf":[
            {"required":["path"]},
            {"required":["paths"]}
        ],
        "additionalProperties":false
    })
}

/// Parse after JSON Schema validation; semantic mode errors remain stable refusals.
pub(super) fn parse(arguments: &Value) -> Result<ReadRequest, Refusal> {
    let object = arguments
        .as_object()
        .ok_or_else(|| invalid("read arguments must be an object"))?;
    match (object.get("path"), object.get("paths")) {
        (Some(path), None) => {
            let spec = parse_spec(path, object)?;
            Ok(ReadRequest::Single(spec))
        }
        (None, Some(paths)) => {
            if ["offset_bytes", "max_bytes", "start_line", "line_count"]
                .iter()
                .any(|key| object.contains_key(*key))
            {
                return Err(invalid("per-path ranges belong inside paths items"));
            }
            let paths = paths
                .as_array()
                .ok_or_else(|| invalid("paths must be an array"))?;
            if paths.is_empty() || paths.len() > MAX_PATHS {
                return Err(invalid("paths must contain 1 to 8 items"));
            }
            let mut seen = BTreeSet::new();
            let specs = paths
                .iter()
                .map(|item| {
                    let item = item
                        .as_object()
                        .ok_or_else(|| invalid("each paths item must be an object"))?;
                    let path = item
                        .get("path")
                        .ok_or_else(|| invalid("each paths item requires path"))?;
                    let spec = parse_spec(path, item)?;
                    if !seen.insert(spec.path.clone()) {
                        return Err(invalid("paths must be unique"));
                    }
                    Ok(spec)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ReadRequest::Many(specs))
        }
        (Some(_), Some(_)) => Err(invalid("path and paths are mutually exclusive")),
        (None, None) => Err(invalid("read requires path or paths")),
    }
}

fn parse_spec(path_value: &Value, object: &Map<String, Value>) -> Result<ReadSpec, Refusal> {
    let raw = path_value
        .as_str()
        .ok_or_else(|| invalid("path must be a string"))?;
    let path = WorkspacePath::new(raw)?;
    let has_start = object.contains_key("start_line");
    let has_count = object.contains_key("line_count");
    let has_offset = object.contains_key("offset_bytes");
    let max_bytes = integer(object, "max_bytes", MAX_RANGE_BYTES)?;
    if max_bytes > MAX_RANGE_BYTES {
        return Err(invalid("max_bytes exceeds 1 MiB"));
    }
    let range = if has_start || has_count {
        if has_offset {
            return Err(invalid("byte and line ranges cannot be mixed"));
        }
        if !has_start || !has_count {
            return Err(invalid("start_line and line_count must be supplied together"));
        }
        let start_line = integer(object, "start_line", 0)?;
        let line_count = integer(object, "line_count", 0)?;
        if start_line == 0 || line_count == 0 || line_count > MAX_LINE_COUNT {
            return Err(invalid("line range is outside its bounds"));
        }
        ReadRange::Lines {
            start_line,
            line_count,
            max_bytes,
        }
    } else {
        ReadRange::Bytes {
            offset_bytes: integer(object, "offset_bytes", 0)?,
            max_bytes,
        }
    };
    Ok(ReadSpec { path, range })
}

fn integer(object: &Map<String, Value>, key: &str, default: u64) -> Result<u64, Refusal> {
    object
        .get(key)
        .map_or(Ok(default), |value| {
            value.as_u64().ok_or_else(|| invalid("range values must be unsigned integers"))
        })
}

fn invalid(detail: &str) -> Refusal {
    Refusal::new(ResultCode::InvalidArguments, detail)
}

/// Locate the requested LF-delimited lines using bounded reads, including when
/// a line is longer than the scan chunk. A trailing LF does not add a phantom line.
pub(super) fn line_window(
    file_len: u64,
    start_line: u64,
    line_count: u64,
    mut read_at: impl FnMut(u64, usize) -> Result<Vec<u8>, Refusal>,
) -> Result<LineWindow, Refusal> {
    fn next_lf(
        file_len: u64,
        cursor: &mut u64,
        read_at: &mut impl FnMut(u64, usize) -> Result<Vec<u8>, Refusal>,
    ) -> Result<Option<u64>, Refusal> {
        const CHUNK: usize = 64 * 1024;
        while *cursor < file_len {
            let count = (file_len - *cursor).min(CHUNK as u64) as usize;
            let bytes = read_at(*cursor, count)?;
            if bytes.len() != count {
                return Err(Refusal::new(ResultCode::StalePlan, "file changed while reading"));
            }
            if let Some(index) = bytes.iter().position(|byte| *byte == b'\n') {
                return Ok(Some(*cursor + index as u64));
            }
            *cursor += count as u64;
        }
        Ok(None)
    }

    if start_line == 0 || line_count == 0 || line_count > MAX_LINE_COUNT {
        return Err(invalid("line range is outside its bounds"));
    }
    let mut cursor = 0;
    for _ in 1..start_line {
        let Some(lf) = next_lf(file_len, &mut cursor, &mut read_at)? else {
            return Ok(LineWindow {
                start: file_len,
                end: file_len,
                more: false,
                selected_lines: 0,
            });
        };
        cursor = lf + 1;
    }
    if cursor >= file_len {
        return Ok(LineWindow {
            start: file_len,
            end: file_len,
            more: false,
            selected_lines: 0,
        });
    }
    let start = cursor;
    let mut selected_lines = 0;
    while selected_lines < line_count && cursor < file_len {
        cursor = next_lf(file_len, &mut cursor, &mut read_at)?
            .map_or(file_len, |lf| lf + 1);
        selected_lines += 1;
    }
    Ok(LineWindow {
        start,
        end: cursor,
        more: cursor < file_len,
        selected_lines,
    })
}

/// Move byte endpoints inward to UTF-8 boundaries, rejecting malformed bytes.
pub(super) fn inward_utf8<'a>(
    bytes: &'a [u8],
    mut start: usize,
    end: usize,
) -> Result<(usize, &'a str), Refusal> {
    let invalid = || Refusal::new(ResultCode::NotUtf8, "range is not UTF-8");
    if start == end {
        return Ok((start, ""));
    }
    if bytes[start] & 0xc0 == 0x80 {
        let mut lead = start;
        while lead > 0 && start - lead < 3 && bytes[lead] & 0xc0 == 0x80 {
            lead -= 1;
        }
        let length = character_length(bytes[lead]).ok_or_else(invalid)?;
        let character_end = lead + length;
        let character = bytes.get(lead..character_end).ok_or_else(invalid)?;
        std::str::from_utf8(character).map_err(|_| invalid())?;
        if character_end <= start {
            return Err(invalid());
        }
        start = character_end.min(end);
    }
    match std::str::from_utf8(&bytes[start..end]) {
        Ok(text) => Ok((start, text)),
        Err(error) if error.error_len().is_none() => {
            let tail = start + error.valid_up_to();
            let length = character_length(bytes[tail]).ok_or_else(invalid)?;
            let character = bytes.get(tail..tail + length).ok_or_else(invalid)?;
            std::str::from_utf8(character).map_err(|_| invalid())?;
            let text = std::str::from_utf8(&bytes[start..tail]).map_err(|_| invalid())?;
            Ok((start, text))
        }
        Err(_) => Err(invalid()),
    }
}

fn character_length(lead: u8) -> Option<usize> {
    match lead {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

pub(super) fn multi_payload_budget(count: usize) -> Result<usize, Refusal> {
    if count == 0 || count > MAX_PATHS {
        return Err(invalid("paths must contain 1 to 8 items"));
    }
    Ok(MAX_RESULT_BYTES - count * MULTI_HEADER_RESERVE)
}

pub(super) fn render_many(outputs: &[ReadOutput]) -> Result<(String, bool), Refusal> {
    let budget = multi_payload_budget(outputs.len())?;
    let payload = outputs.iter().map(|output| output.text.len()).sum::<usize>();
    if payload > budget {
        return Err(Refusal::new(
            ResultCode::InfrastructureFailure,
            "read output exceeded its aggregate bound",
        ));
    }
    let mut text = String::with_capacity(MAX_RESULT_BYTES);
    let mut truncated = false;
    for (index, output) in outputs.iter().enumerate() {
        if output.header.len() + 1 > MULTI_HEADER_RESERVE {
            return Err(Refusal::new(
                ResultCode::InfrastructureFailure,
                "read result header exceeded its bound",
            ));
        }
        if index > 0 && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&output.header);
        text.push('\n');
        text.push_str(&output.text);
        truncated |= output.truncated;
    }
    if text.len() > MAX_RESULT_BYTES {
        return Err(Refusal::new(
            ResultCode::InfrastructureFailure,
            "read output exceeded its aggregate bound",
        ));
    }
    Ok((text, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bytes(text: &[u8]) -> impl FnMut(u64, usize) -> Result<Vec<u8>, Refusal> + '_ {
        move |offset, count| Ok(text[offset as usize..offset as usize + count].to_vec())
    }

    #[test]
    fn parse_keeps_legacy_byte_defaults_and_accepts_line_and_many_ranges() {
        let ReadRequest::Single(single) = parse(&json!({"path":"src/lib.rs"})).unwrap() else {
            panic!("single read expected");
        };
        assert_eq!(
            single.range,
            ReadRange::Bytes {
                offset_bytes: 0,
                max_bytes: MAX_RANGE_BYTES
            }
        );
        let ReadRequest::Single(lines) = parse(&json!({
            "path":"src/lib.rs", "start_line":4, "line_count":12
        }))
        .unwrap()
        else {
            panic!("single line read expected");
        };
        assert_eq!(
            lines.range,
            ReadRange::Lines {
                start_line: 4,
                line_count: 12,
                max_bytes: MAX_RANGE_BYTES
            }
        );
        let ReadRequest::Many(many) = parse(&json!({"paths":[
            {"path":"a", "start_line":1, "line_count":1},
            {"path":"b", "offset_bytes":8, "max_bytes":3}
        ]}))
        .unwrap()
        else {
            panic!("many read expected");
        };
        assert_eq!(many.len(), 2);
    }

    #[test]
    fn parser_rejects_mixed_partial_duplicate_and_oversized_ranges() {
        for bad in [
            json!({"path":"a", "offset_bytes":0, "start_line":1, "line_count":1}),
            json!({"path":"a", "start_line":1}),
            json!({"path":"a", "line_count":1}),
            json!({"path":"a", "start_line":0, "line_count":1}),
            json!({"path":"a", "start_line":1, "line_count":MAX_LINE_COUNT+1}),
            json!({"path":"a", "max_bytes":MAX_RANGE_BYTES+1}),
            json!({"path":"a", "paths":[{"path":"b"}]}),
            json!({"paths":[{"path":"a"}, {"path":"a", "offset_bytes":1}]}),
            json!({"paths":[{"path":"a"}], "max_bytes":10}),
        ] {
            assert_eq!(parse(&bad).unwrap_err().code, ResultCode::InvalidArguments, "{bad}");
        }
        let nine = (0..9)
            .map(|index| json!({"path":format!("file-{index}")}))
            .collect::<Vec<_>>();
        assert_eq!(
            parse(&json!({"paths":nine})).unwrap_err().code,
            ResultCode::InvalidArguments
        );
    }

    #[test]
    fn schema_is_closed_and_requires_exactly_one_path_form() {
        let validator = jsonschema::validator_for(&schema(json!({"type":"string"}))).unwrap();
        for invalid in [
            json!({"path":"a", "unexpected":true}),
            json!({"paths":[{"path":"a", "unexpected":true}]}),
            json!({"path":"a", "paths":[{"path":"b"}]}),
            json!({}),
        ] {
            assert!(!validator.is_valid(&invalid), "{invalid}");
        }
        assert!(validator.is_valid(&json!({"path":"a", "start_line":1, "line_count":1}));
        assert!(validator.is_valid(&json!({"paths":[{"path":"a", "offset_bytes":0}]}));
    }

    #[test]
    fn line_windows_preserve_crlf_and_have_no_phantom_eof_line() {
        let source = b"one\r\ntwo\nlast";
        let second = line_window(source.len() as u64, 2, 2, bytes(source)).unwrap();
        assert_eq!(&source[second.start as usize..second.end as usize], b"two\nlast");
        assert_eq!(second.selected_lines, 2);
        assert!(!second.more);
        let past_eof = line_window(source.len() as u64, 4, 1, bytes(source)).unwrap();
        assert_eq!(past_eof.selected_lines, 0);
        assert_eq!(past_eof.start, source.len() as u64);

        let trailing = b"one\n";
        let no_phantom = line_window(trailing.len() as u64, 2, 1, bytes(trailing)).unwrap();
        assert_eq!(no_phantom.selected_lines, 0);
    }

    #[test]
    fn line_windows_scan_long_lines_across_chunks_and_eof() {
        let mut source = vec![b'x'; 70_000];
        source.extend_from_slice(b"\nnext\n");
        let next = line_window(source.len() as u64, 2, 1, bytes(&source)).unwrap();
        assert_eq!(&source[next.start as usize..next.end as usize], b"next\n");
        assert!(!next.more);
        let first = line_window(source.len() as u64, 1, 1, bytes(&source)).unwrap();
        assert_eq!(first.end, 70_001);
        assert!(first.more);
    }

    #[test]
    fn utf8_edges_clip_only_incomplete_boundary_codepoints() {
        let source = "a€z".as_bytes();
        let (start, text) = inward_utf8(source, 1, 3).unwrap();
        assert_eq!(start, 1);
        assert_eq!(text, "");
        let (start, text) = inward_utf8(source, 1, 4).unwrap();
        assert_eq!(start, 1);
        assert_eq!(text, "€");
        assert_eq!(inward_utf8(b"\xff", 0, 1).unwrap_err().code, ResultCode::NotUtf8);
        assert_eq!(inward_utf8(b"\xe2", 0, 1).unwrap_err().code, ResultCode::NotUtf8);
    }

    #[test]
    fn multi_output_budget_is_aggregate_and_each_file_has_a_header() {
        let outputs = (0..8)
            .map(|index| ReadOutput {
                header: format!("[{index}] bytes 0..0; more=false; output_truncated=true"),
                text: String::new(),
                truncated: true,
            })
            .collect::<Vec<_>>();
        let (rendered, truncated) = render_many(&outputs).unwrap();
        assert!(rendered.len() <= MAX_RESULT_BYTES);
        assert_eq!(rendered.matches("output_truncated=true").count(), 8);
        assert!(truncated);
        assert_eq!(multi_payload_budget(8).unwrap(), MAX_RESULT_BYTES - 8 * MULTI_HEADER_RESERVE);
    }
}
