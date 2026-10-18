//! Resync views and the client reducer shared by the core and its clients.

use std::collections::BTreeMap;

use crate::session::{CallId, ModelRef, ResponseId, RuntimeState, SessionSnapshot, TurnId, Usage};

use super::types::{EventKind, InferenceId, RuntimeEpoch, RuntimeEvent};

/// Operational phase as a client sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The journaled operational state of the published snapshot.
    Durable(RuntimeState),
    /// The snapshot has unfinished work; every command is rejected until
    /// recovery is implemented.
    RecoveryRequired,
    /// Persistence failed; only a new runtime over a reopened store continues.
    Faulted,
}

/// One block of the in-flight response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PartialBlock {
    /// Accumulated text.
    Text(String),
    /// Accumulated displayable reasoning summary.
    ReasoningSummary(String),
    /// A proposed call. Never executable while partial.
    ToolCall {
        /// Engine-issued call identity.
        call_id: CallId,
        /// Tool name.
        name: String,
        /// Native call identity.
        provider_call_id: Option<String>,
        /// Concatenated argument fragments, possibly incomplete JSON.
        arguments: String,
        /// Whether the arguments formed a complete JSON object.
        ready: bool,
    },
}

/// The live response of the current inference, which no snapshot contains.
/// Bounded by `RuntimeConfig::max_partial_bytes`.
#[derive(Clone, Debug, PartialEq)]
pub struct PartialResponse {
    /// Stream identity.
    pub inference: InferenceId,
    /// Turn it belongs to.
    pub turn_id: TurnId,
    /// Identity the assistant entry will carry.
    pub response_id: ResponseId,
    /// Model in use.
    pub model: ModelRef,
    /// Blocks by response-local index, in content order.
    pub blocks: BTreeMap<u32, PartialBlock>,
    /// Latest cumulative usage.
    pub usage: Option<Usage>,
}

/// A coherent resync point: the published snapshot plus live state, reflecting
/// exactly the events with `seq <= event_seq`.
#[derive(Clone, Debug)]
pub struct RuntimeView {
    /// Runtime incarnation.
    pub epoch: RuntimeEpoch,
    /// Last event reflected; 0 before the first event.
    pub event_seq: u64,
    /// Last published snapshot.
    pub snapshot: SessionSnapshot,
    /// Current phase.
    pub phase: Phase,
    /// Durable active turn.
    pub active_turn: Option<TurnId>,
    /// In-flight response, if an inference is running or being persisted.
    pub partial: Option<PartialResponse>,
}

/// Snapshots compare by identity: the exact-byte checksum of their last record
/// covers the whole hash chain.
impl PartialEq for RuntimeView {
    fn eq(&self, other: &Self) -> bool {
        self.epoch == other.epoch
            && self.event_seq == other.event_seq
            && self.snapshot.session_id() == other.snapshot.session_id()
            && self.snapshot.sequence() == other.snapshot.sequence()
            && self.snapshot.checksum() == other.snapshot.checksum()
            && self.phase == other.phase
            && self.active_turn == other.active_turn
            && self.partial == other.partial
    }
}

/// What `ClientView::apply` did with an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The event was next and is now reflected.
    Applied,
    /// The view already reflects it.
    Duplicate,
    /// An event is missing; take a new view. The view is unchanged.
    NeedsResync,
}

/// A client's state, rebuilt from a `RuntimeView` and kept current by events.
/// The runtime maintains its own view with this same reducer.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientView(RuntimeView);

impl From<RuntimeView> for ClientView {
    fn from(view: RuntimeView) -> Self {
        Self(view)
    }
}

impl ClientView {
    /// The reflected state.
    pub fn view(&self) -> &RuntimeView {
        &self.0
    }

    /// Apply the next event. Older events are ignored; a gap requires resync.
    pub fn apply(&mut self, event: &RuntimeEvent) -> ApplyOutcome {
        let view = &mut self.0;
        if event.epoch != view.epoch {
            return ApplyOutcome::NeedsResync;
        }
        if event.seq <= view.event_seq {
            return ApplyOutcome::Duplicate;
        }
        if event.seq != view.event_seq + 1 {
            return ApplyOutcome::NeedsResync;
        }
        view.event_seq = event.seq;
        match &event.kind {
            EventKind::Committed { snapshot, .. } => view.snapshot = snapshot.clone(),
            EventKind::PhaseChanged { phase, active_turn } => {
                view.phase = *phase;
                view.active_turn = active_turn.clone();
            }
            EventKind::InferenceStarted {
                inference,
                turn_id,
                response_id,
                model,
            } => {
                view.partial = Some(PartialResponse {
                    inference: *inference,
                    turn_id: turn_id.clone(),
                    response_id: response_id.clone(),
                    model: model.clone(),
                    blocks: BTreeMap::new(),
                    usage: None,
                })
            }
            EventKind::TextDelta {
                inference,
                block_index,
                text,
            } => {
                if let Some(partial) = partial(view, inference)
                    && let PartialBlock::Text(existing) = partial
                        .blocks
                        .entry(*block_index)
                        .or_insert_with(|| PartialBlock::Text(String::new()))
                {
                    existing.push_str(text);
                }
            }
            EventKind::ReasoningSummaryDelta {
                inference,
                block_index,
                text,
            } => {
                if let Some(partial) = partial(view, inference)
                    && let PartialBlock::ReasoningSummary(existing) = partial
                        .blocks
                        .entry(*block_index)
                        .or_insert_with(|| PartialBlock::ReasoningSummary(String::new()))
                {
                    existing.push_str(text);
                }
            }
            EventKind::ToolCallStarted {
                inference,
                block_index,
                call_id,
                name,
                provider_call_id,
            } => {
                if let Some(partial) = partial(view, inference) {
                    partial.blocks.insert(
                        *block_index,
                        PartialBlock::ToolCall {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            provider_call_id: provider_call_id.clone(),
                            arguments: String::new(),
                            ready: false,
                        },
                    );
                }
            }
            EventKind::ToolArgumentsDelta {
                inference,
                block_index,
                fragment,
            } => {
                if let Some(partial) = partial(view, inference)
                    && let Some(PartialBlock::ToolCall { arguments, .. }) =
                        partial.blocks.get_mut(block_index)
                {
                    arguments.push_str(fragment);
                }
            }
            EventKind::ToolCallReady {
                inference,
                block_index,
            } => {
                if let Some(partial) = partial(view, inference)
                    && let Some(PartialBlock::ToolCall { ready, .. }) =
                        partial.blocks.get_mut(block_index)
                {
                    *ready = true;
                }
            }
            EventKind::UsageUpdated { inference, usage } => {
                if let Some(partial) = partial(view, inference) {
                    partial.usage = Some(usage.clone());
                }
            }
            EventKind::PartialCleared { inference } => {
                if partial(view, inference).is_some() {
                    view.partial = None;
                }
            }
            EventKind::TurnFinished { .. } | EventKind::Diagnostic(_) | EventKind::Fault(_) => {}
        }
        ApplyOutcome::Applied
    }
}

/// The partial response of stream `inference`, if it is the live one.
fn partial<'a>(
    view: &'a mut RuntimeView,
    inference: &InferenceId,
) -> Option<&'a mut PartialResponse> {
    view.partial
        .as_mut()
        .filter(|partial| partial.inference == *inference)
}
