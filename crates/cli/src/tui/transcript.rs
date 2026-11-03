//! Pure projection from a runtime view to transcript rows.
//!
//! Rows are width-independent Ratatui [`Line`]s. The renderer wraps them to the
//! pane, so the projection can be tested without a terminal and a `Reset`
//! always rebuilds exactly the same rows from the same view.
//!
//! Completed entries are rendered once and cached by entry identity plus the
//! switches that change their rows (reasoning expansion, output expansion, and
//! the tools' outcomes and approval decisions). Only the live partial response
//! is re-parsed on every rebuild.

use std::collections::{BTreeMap, BTreeSet};

use harness_core::{
    runtime::{PartialBlock, RuntimeView},
    session::{
        ApprovalId, CallId, ContentBlock, ContextMode, Decision, EntryId, EntryPayload,
        JournalEvent, ModelRef, Portability, ToolStatus, TurnId, TurnOutcome,
    },
};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use serde_json::{Map, Value};

use super::{
    markdown,
    theme::{Role, Theme},
    tools,
};
use crate::text::{bounded, line};

/// An observed non-durable fact that belongs in the log but not in a snapshot.
#[derive(Clone, Debug)]
pub(crate) enum Notice {
    /// A non-fatal runtime observation.
    Diagnostic(String),
    /// The runtime stopped.
    Fault(String),
    /// A compaction attempt failed with its stable code.
    CompactionFailed(String),
    /// Recovery closed a turn.
    Recovered(String),
    /// Informational status.
    Info(String),
}

/// The two expansion switches that change a completed entry's rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Toggles {
    /// Show every reasoning line instead of the first.
    pub(crate) reasoning: bool,
    /// Show every tool-output line instead of the first three.
    pub(crate) output: bool,
}

/// Rendered entries, keyed by their identity and the facts that can change them.
#[derive(Default)]
pub(crate) struct Renderer {
    cache: BTreeMap<EntryId, Cached>,
    parses: u64,
}

struct Cached {
    key: u64,
    rows: Vec<Line<'static>>,
}

impl Renderer {
    /// How many entries were rendered from scratch. A hook for the cache test.
    pub(crate) fn parses(&self) -> u64 {
        self.parses
    }

    /// Project the durable branch and the live partial response into rows.
    pub(crate) fn project(
        &mut self,
        view: &RuntimeView,
        toggles: Toggles,
        notices: &[Notice],
        durations: &BTreeMap<TurnId, std::time::Duration>,
    ) -> Vec<Line<'static>> {
        let mut rows = Vec::new();
        let ancestry = match view.snapshot.ancestry(view.snapshot.head()) {
            Ok(ancestry) => ancestry,
            Err(_) => return rows,
        };
        let branch: BTreeSet<EntryId> = ancestry
            .iter()
            .map(|stored| stored.entry.id.clone())
            .collect();
        let (outcomes, approvals) = audit_rows(view, &branch);
        let calls = call_index(&ancestry);
        self.cache.retain(|id, _| branch.contains(id));

        let mut emitted: BTreeSet<TurnId> = BTreeSet::new();
        // An excluded turn stays in history, in this transcript and in the report;
        // it is only kept out of the model's request (decision 7). Dimming and one
        // tag row say so, while the text stays readable.
        let modes = super::turn_modes(&ancestry);
        let mut tagged: BTreeSet<TurnId> = BTreeSet::new();
        for (position, stored) in ancestry.iter().enumerate() {
            let mode = stored
                .entry
                .turn_id
                .as_ref()
                .and_then(|turn| modes.get(turn))
                .copied()
                .unwrap_or_default();
            let start = rows.len();
            if mode == ContextMode::Excluded
                && stored
                    .entry
                    .turn_id
                    .as_ref()
                    .is_some_and(|turn| tagged.insert(turn.clone()))
            {
                rows.push(Line::styled(
                    "── excluded turn (kept in history; never sent to the model) ──",
                    dim_style(),
                ));
            }
            let key = entry_key(
                &stored.entry.payload,
                toggles,
                &approvals,
                &calls,
                stored
                    .entry
                    .turn_id
                    .as_ref()
                    .and_then(|turn| durations.get(turn))
                    .copied(),
            );
            let cached = self
                .cache
                .get(&stored.entry.id)
                .filter(|cached| cached.key == key);
            let mut entry_rows = match cached {
                Some(cached) => cached.rows.clone(),
                None => {
                    self.parses += 1;
                    let rendered = render_entry(
                        &stored.entry.payload,
                        toggles,
                        &approvals,
                        &calls,
                        stored
                            .entry
                            .turn_id
                            .as_ref()
                            .and_then(|turn| durations.get(turn))
                            .copied(),
                    );
                    self.cache.insert(
                        stored.entry.id.clone(),
                        Cached {
                            key,
                            rows: rendered.clone(),
                        },
                    );
                    rendered
                }
            };
            rows.append(&mut entry_rows);
            let last_in_commit = ancestry
                .get(position + 1)
                .is_none_or(|next| next.sequence != stored.sequence);
            if last_in_commit {
                for (turn_id, finish) in &outcomes {
                    if finish.2 == stored.sequence && !emitted.contains(turn_id) {
                        push_outcome(&mut rows, finish.0, finish.1.as_deref());
                        emitted.insert(turn_id.clone());
                    }
                }
            }
            if mode == ContextMode::Excluded {
                dim(&mut rows[start..]);
            }
        }
        // A turn may finish in a commit that carries no entry (cancellation or a
        // failure before any response). Show those outcomes after the branch.
        for (turn_id, finish) in &outcomes {
            if emitted.insert(turn_id.clone()) {
                push_outcome(&mut rows, finish.0, finish.1.as_deref());
            }
        }

        if let Some(partial) = &view.partial {
            let start = rows.len();
            rows.push(assistant_label(
                &partial.model,
                partial.usage.as_ref(),
                None,
            ));
            for block in partial.blocks.values() {
                match block {
                    PartialBlock::Text(text) => rows.extend(markdown::rows(text)),
                    PartialBlock::ReasoningSummary(text) => {
                        rows.extend(reasoning_rows(text, toggles.reasoning));
                    }
                    PartialBlock::ProviderState { schema, bytes } => {
                        rows.push(provider_state(Some(schema.as_str()), *bytes as u64, None))
                    }
                    PartialBlock::ToolCall {
                        name,
                        arguments,
                        ready,
                        ..
                    } => rows.extend(tools::partial(name, arguments, *ready)),
                }
            }
            gutter(&mut rows[start..], Role::Assistant);
            rows.push(Line::default());
        } else if view.phase == harness_core::runtime::Phase::AwaitingResolution
            && !view.awaiting.is_empty()
        {
            rows.push(Line::styled(
                "waiting for a statement about an uncertain invocation",
                error_style(),
            ));
            rows.push(Line::default());
        }

        for notice in notices {
            match notice {
                Notice::Diagnostic(text) => {
                    rows.push(Line::styled(format!("diagnostic: {text}"), dim_style()))
                }
                Notice::Fault(text) => rows.push(Line::styled(text.clone(), error_style())),
                Notice::CompactionFailed(code) => rows.push(Line::styled(
                    format!("compaction failed: {}", bounded(code, 80)),
                    error_style(),
                )),
                Notice::Recovered(text) => rows.push(Line::styled(text.clone(), dim_style())),
                Notice::Info(text) => rows.push(Line::styled(text.clone(), dim_style())),
            }
        }
        if view.phase == harness_core::runtime::Phase::Faulted
            && !notices.iter().any(|n| matches!(n, Notice::Fault(_)))
        {
            rows.push(Line::styled("fault: runtime stopped", error_style()));
        }
        if rows.is_empty() {
            rows.push(Line::styled(
                "no messages yet — type a prompt and press Enter",
                dim_style(),
            ));
        }
        rows
    }
}

/// Durable facts a header needs, gathered from the branch's tool results.
struct Calls {
    names: BTreeMap<CallId, (String, Map<String, Value>)>,
    facts: BTreeMap<CallId, Facts>,
}

#[derive(Clone)]
struct Facts {
    status: ToolStatus,
    code: Option<String>,
    exit_code: Option<i32>,
}

fn call_index(ancestry: &[&harness_core::session::StoredEntry]) -> Calls {
    let mut calls = Calls {
        names: BTreeMap::new(),
        facts: BTreeMap::new(),
    };
    for stored in ancestry {
        match &stored.entry.payload {
            EntryPayload::AssistantResponse(response) => {
                for block in &response.content {
                    if let ContentBlock::ToolCall {
                        call_id,
                        name,
                        arguments,
                        ..
                    } = block
                    {
                        calls
                            .names
                            .insert(call_id.clone(), (name.clone(), arguments.clone()));
                    }
                }
            }
            EntryPayload::ToolResult(result) => {
                calls.facts.insert(
                    result.result.call.call_id.clone(),
                    Facts {
                        status: result.result.status,
                        code: result.result.metadata.code.clone(),
                        exit_code: result.result.metadata.exit_code,
                    },
                );
            }
            _ => {}
        }
    }
    calls
}

/// An entry's cache key: its switches plus the facts its own calls can show.
fn entry_key(
    payload: &EntryPayload,
    toggles: Toggles,
    approvals: &Approvals,
    calls: &Calls,
    duration: Option<std::time::Duration>,
) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |value: u64| {
        hash ^= value;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    };
    mix(u64::from(toggles.reasoning) | (u64::from(toggles.output) << 1));
    if let Some(duration) = duration {
        mix(duration.as_millis() as u64 + 1);
    }
    match payload {
        EntryPayload::AssistantResponse(response) => {
            for block in &response.content {
                let ContentBlock::ToolCall { call_id, .. } = block else {
                    continue;
                };
                if let Some((_, decision)) = approvals.get(call_id) {
                    mix(match decision {
                        Some(Decision::Allow) => 2,
                        Some(Decision::Deny) => 3,
                        None => 1,
                    });
                }
                if let Some(facts) = calls.facts.get(call_id) {
                    mix(facts.status as u64 + 4);
                    mix(facts.exit_code.unwrap_or(i32::MIN) as u64);
                    for byte in facts.code.as_deref().unwrap_or_default().bytes() {
                        mix(u64::from(byte));
                    }
                }
            }
        }
        EntryPayload::ToolResult(result) => {
            mix(result.result.status as u64 + 4);
        }
        _ => {}
    }
    hash
}

fn render_entry(
    payload: &EntryPayload,
    toggles: Toggles,
    approvals: &Approvals,
    calls: &Calls,
    duration: Option<std::time::Duration>,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    match payload {
        EntryPayload::UserMessage(message) => {
            let start = rows.len();
            rows.push(Line::styled("you", Theme::default().style(Role::User)));
            for block in &message.content {
                user_block(&mut rows, block);
            }
            gutter(&mut rows[start..], Role::User);
            rows.push(Line::default());
        }
        EntryPayload::AssistantResponse(response) => {
            let start = rows.len();
            rows.push(assistant_label(
                &response.model,
                response.usage.as_ref(),
                duration,
            ));
            if let Some(error) = &response.error {
                rows.push(Line::styled(
                    format!(
                        "error {}: {}",
                        model_error_code(error.code),
                        bounded(&error.message, 240)
                    ),
                    error_style(),
                ));
            }
            for block in &response.content {
                match block {
                    ContentBlock::Text { text } => rows.extend(markdown::rows(text)),
                    ContentBlock::ReasoningSummary { text } => {
                        rows.extend(reasoning_rows(text, toggles.reasoning));
                    }
                    ContentBlock::ToolCall {
                        call_id,
                        name,
                        arguments,
                        ..
                    } => {
                        let facts = calls.facts.get(call_id);
                        rows.extend(tools::call(
                            name,
                            arguments,
                            tools::Facts {
                                status: facts.map(|facts| facts.status),
                                code: facts.and_then(|facts| facts.code.as_deref()),
                                exit_code: facts.and_then(|facts| facts.exit_code),
                            },
                        ));
                        approval_rows(&mut rows, approvals, call_id);
                    }
                    ContentBlock::ProviderState {
                        owner,
                        schema,
                        artifact,
                    } => rows.push(provider_state(
                        Some(schema.as_str()),
                        artifact.bytes,
                        Some(owner),
                    )),
                    other => media(&mut rows, other),
                }
            }
            gutter(&mut rows[start..], Role::Assistant);
            rows.push(Line::default());
        }
        EntryPayload::ToolResult(result) => {
            output(&mut rows, result, toggles, calls);
        }
        EntryPayload::ModelChanged(changed) => {
            rows.push(Line::styled(
                format!(
                    "model switched: {} (portability: {})",
                    model_name(&changed.model),
                    portability(changed.portability)
                ),
                accent_style(),
            ));
            rows.push(Line::default());
        }
        EntryPayload::ContextControl(control) => {
            let action = match control.mode {
                ContextMode::Pinned => "Pinned",
                ContextMode::Excluded => "Excluded",
                ContextMode::Default => "Reset",
            };
            rows.push(Line::styled(
                format!("{action} turn {}", turn_label(&control.turn_id)),
                accent_style(),
            ));
            rows.push(Line::default());
        }
        EntryPayload::Compaction(compaction) => {
            rows.push(Line::styled(
                "── compaction summary ──────────────────────",
                accent_style(),
            ));
            let model = compaction
                .model
                .as_ref()
                .map(model_name)
                .unwrap_or_else(|| "no model".into());
            rows.push(Line::styled(
                format!(
                    "  {} · {} entries covered · {}",
                    bounded(&compaction.strategy, 40),
                    compaction.covered_entry_ids.len(),
                    model
                ),
                dim_style(),
            ));
            for text in compaction.summary.lines() {
                rows.push(Line::raw(format!("  {}", bounded(text, 200))));
            }
            rows.push(Line::styled(
                "────────────────────────────────────────────",
                accent_style(),
            ));
            rows.push(Line::default());
        }
    }
    rows
}

type Outcomes = BTreeMap<TurnId, (Option<TurnOutcome>, Option<String>, u64)>;
type Approvals = BTreeMap<CallId, (Option<ApprovalId>, Option<Decision>)>;

fn audit_rows(view: &RuntimeView, branch: &BTreeSet<EntryId>) -> (Outcomes, Approvals) {
    let mut starts = BTreeSet::new();
    let mut outcomes = BTreeMap::new();
    let mut approvals = Approvals::new();
    for (sequence, _, event) in view.snapshot.events() {
        match event {
            JournalEvent::TurnStarted(started) if branch.contains(&started.user_entry_id) => {
                starts.insert(started.turn_id.clone());
            }
            JournalEvent::TurnFinished(finished) if starts.contains(&finished.turn_id) => {
                outcomes.insert(
                    finished.turn_id.clone(),
                    (Some(finished.outcome), finished.code.clone(), *sequence),
                );
            }
            JournalEvent::ApprovalRequested(requested) => {
                approvals
                    .entry(requested.call.call_id.clone())
                    .or_default()
                    .0 = Some(requested.approval_id.clone());
            }
            JournalEvent::AuthorizationDecided(decided) => {
                approvals.entry(decided.call.call_id.clone()).or_default().1 =
                    Some(decided.decision);
            }
            _ => {}
        }
    }
    (outcomes, approvals)
}

fn push_outcome(rows: &mut Vec<Line<'static>>, outcome: Option<TurnOutcome>, code: Option<&str>) {
    if outcome.unwrap_or(TurnOutcome::Completed) == TurnOutcome::Completed {
        return;
    }
    while rows.last().is_some_and(|row| row.width() == 0) {
        rows.pop();
    }
    let mut row = outcome_row(outcome, code);
    let role = match outcome {
        Some(TurnOutcome::Cancelled) => Role::Warning,
        _ => Role::Error,
    };
    row.spans
        .insert(0, Span::styled("▌ ", Theme::default().style(role)));
    rows.push(row);
    rows.push(Line::default());
}

fn assistant_label(
    model: &ModelRef,
    usage: Option<&harness_core::session::Usage>,
    duration: Option<std::time::Duration>,
) -> Line<'static> {
    let mut meta = bounded(&model.model, 48);
    if let Some(duration) = duration {
        meta.push_str(&format!(" · {:.1}s", duration.as_secs_f64()));
    }
    if let Some(usage) = usage {
        let count = |n: Option<u64>| n.map_or_else(|| "?".into(), |n| n.to_string());
        meta.push_str(&format!(
            " · {} in / {} out",
            count(usage.input_tokens),
            count(usage.output_tokens)
        ));
    }
    Line::from(vec![
        Span::styled("assistant", label_style()),
        Span::styled(meta, dim_style()),
    ])
}

/// Width-independent gutter spans; the renderer repeats them on wrapped rows.
fn gutter(rows: &mut [Line<'static>], role: Role) {
    for row in rows.iter_mut().filter(|row| row.width() > 0) {
        let at = usize::from(
            row.spans
                .first()
                .is_some_and(|span| span.content.starts_with('\u{0}')),
        );
        row.spans
            .insert(at, Span::styled("▌ ", Theme::default().style(role)));
    }
}

fn outcome_row(outcome: Option<TurnOutcome>, code: Option<&str>) -> Line<'static> {
    let outcome = outcome.unwrap_or(TurnOutcome::Completed);
    let label = match outcome {
        TurnOutcome::Completed => "turn: completed",
        TurnOutcome::Cancelled => "turn: cancelled",
        TurnOutcome::Interrupted => "turn: interrupted",
        TurnOutcome::Failed => "turn: failed",
    };
    let text = match code {
        Some(code) => format!("{label} ({})", bounded(code, 80)),
        None => label.to_owned(),
    };
    Line::styled(text, error_style())
}

/// One dim line under the call: a recorded approval or a pending one.
fn approval_rows(rows: &mut Vec<Line<'static>>, approvals: &Approvals, call: &CallId) {
    let Some((_, decided)) = approvals.get(call) else {
        return;
    };
    let text = match decided {
        Some(Decision::Allow) => "approval: allowed",
        Some(Decision::Deny) => "approval: denied",
        None => "approval: requested",
    };
    rows.push(Line::styled(text, dim_style()));
}

/// Dim rows an excluded turn owns. Wrapping keeps span styles and the renderer
/// patches them over the line style, so both are patched: the turn is visibly
/// dimmed without hiding or truncating one character of it.
fn dim(rows: &mut [Line<'static>]) {
    for row in rows.iter_mut() {
        row.style = row.style.patch(dim_style());
        for span in &mut row.spans {
            span.style = span.style.patch(dim_style());
        }
    }
}

fn user_block(rows: &mut Vec<Line<'static>>, block: &ContentBlock) {
    match block {
        ContentBlock::Text { text } => rows.extend(markdown::user_rows(text)),
        other => media(rows, other),
    }
}

fn media(rows: &mut Vec<Line<'static>>, block: &ContentBlock) {
    match block {
        ContentBlock::Image { mime_type, .. } => rows.push(Line::styled(
            format!("[image {}]", bounded(mime_type, 60)),
            dim_style(),
        )),
        ContentBlock::Audio { mime_type, .. } => rows.push(Line::styled(
            format!("[audio {}]", bounded(mime_type, 60)),
            dim_style(),
        )),
        ContentBlock::File {
            mime_type, name, ..
        } => rows.push(Line::styled(
            format!("[file {} {}]", bounded(name, 60), bounded(mime_type, 60)),
            dim_style(),
        )),
        ContentBlock::ReasoningSummary { text } => rows.extend(reasoning_rows(text, false)),
        // Tool calls, native state and text have their own renderers.
        ContentBlock::Text { .. }
        | ContentBlock::ToolCall { .. }
        | ContentBlock::ProviderState { .. } => {}
    }
}

fn provider_state(schema: Option<&str>, bytes: u64, owner: Option<&ModelRef>) -> Line<'static> {
    let mut text = format!(
        "provider state: {} ({bytes} bytes, not shown",
        bounded(schema.unwrap_or("schema"), 80)
    );
    if let Some(owner) = owner {
        text.push_str(&format!(", owner {}", model_name(owner)));
    }
    text.push(')');
    Line::styled(text, dim_style())
}

/// Collapse a reasoning summary to one dim italic line unless expanded.
fn reasoning_rows(text: &str, expanded: bool) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    let italic = dim_style().add_modifier(ratatui::style::Modifier::ITALIC);
    let gutter_span = Span::styled("│ ", dim_style());
    if expanded {
        return text
            .lines()
            .map(|text| Line::from(vec![gutter_span.clone(), Span::styled(line(text), italic)]))
            .collect();
    }
    let first = text.lines().next().unwrap_or_default();
    let more = text.lines().count().saturating_sub(1);
    let suffix = if more > 0 {
        format!(" … (+{more} lines · Ctrl-R)")
    } else {
        " (Ctrl-R)".into()
    };
    vec![Line::from(vec![
        gutter_span,
        Span::styled(format!("reasoning: {}", bounded(first, 160)), italic),
        Span::styled(suffix, dim_style()),
    ])]
}

/// A tool result's body: the first three rows, or all of them when expanded.
fn output(
    rows: &mut Vec<Line<'static>>,
    result: &harness_core::session::ToolResultEntry,
    toggles: Toggles,
    calls: &Calls,
) {
    let mut block = Vec::new();
    let mut lines = Vec::new();
    for content in &result.result.content {
        match content {
            ContentBlock::Text { text } => {
                for text in text.lines() {
                    lines.push(text.to_owned());
                }
            }
            other => media(&mut block, other),
        }
    }
    if !calls.names.contains_key(&result.result.call.call_id) {
        // A result whose call is not on this branch still names its own status.
        let mark = tools::Mark::of(Some(result.result.status));
        block.push(Line::from(vec![
            Span::styled(mark.glyph(), mark.style()),
            Span::styled(
                format!(" tool result: {}", tool_status(result.result.status)),
                dim_style(),
            ),
        ]));
    }
    let total = lines.len();
    let shown = if toggles.output { total } else { total.min(3) };
    for text in lines.iter().take(shown) {
        block.push(tools::output_row(text));
    }
    if total > shown {
        block.push(tools::note(format!(
            "… +{} lines (Ctrl-E for full output)",
            total - shown
        )));
    }
    if result.result.metadata.output_truncated {
        block.push(tools::note("output truncated by the tool"));
    }
    if !result.result.metadata.artifacts.is_empty() {
        block.push(tools::note(format!(
            "artifacts captured: {}",
            result.result.metadata.artifacts.len()
        )));
    }
    gutter(&mut block, Role::Assistant);
    rows.append(&mut block);
    rows.push(Line::default());
}

fn tool_status(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Succeeded => "succeeded",
        ToolStatus::Failed => "failed",
        ToolStatus::InvalidArguments => "invalid arguments",
        ToolStatus::Denied => "denied",
        ToolStatus::TimedOut => "timed out",
        ToolStatus::Cancelled => "cancelled",
        ToolStatus::InfrastructureFailure => "infrastructure failure",
        ToolStatus::Uncertain => "uncertain",
    }
}

fn model_error_code(code: harness_core::session::ModelErrorCode) -> &'static str {
    use harness_core::session::ModelErrorCode;
    match code {
        ModelErrorCode::InvalidRequest => "invalid_request",
        ModelErrorCode::UnsupportedCapability => "unsupported_capability",
        ModelErrorCode::Authentication => "authentication",
        ModelErrorCode::RateLimit => "rate_limit",
        ModelErrorCode::Transport => "transport",
        ModelErrorCode::ProviderFailure => "provider_failure",
        ModelErrorCode::MalformedStream => "malformed_stream",
        ModelErrorCode::LimitExceeded => "limit_exceeded",
        ModelErrorCode::ContextOverflow => "context_overflow",
    }
}

fn portability(portability: Portability) -> &'static str {
    match portability {
        Portability::Reject => "reject",
        Portability::OmitOptional => "omit optional",
        Portability::ConvertSupported => "convert supported",
    }
}

fn model_name(model: &ModelRef) -> String {
    format!(
        "{}/{}",
        bounded(&model.provider_instance, 40),
        bounded(&model.model, 60)
    )
}

/// The last eight characters of a turn's identity, enough to tell turns apart
/// in one line without a full UUID.
fn turn_label(turn: &TurnId) -> String {
    turn.to_string()
        .chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn label_style() -> Style {
    Theme::default().style(Role::Assistant)
}

fn dim_style() -> Style {
    Theme::default().style(Role::Dim)
}

fn error_style() -> Style {
    Theme::default().style(Role::Error)
}

fn accent_style() -> Style {
    Theme::default().style(Role::Accent)
}
