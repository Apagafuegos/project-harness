//! The pure runtime state machine: one input in, effects out, nothing else.

use serde_json::json;

use crate::session::{
    AssistantResponse, CallRef, CommandId, Commit, CommitAck, ContentBlock, Digest, Entry, EntryId,
    EntryPayload, HeadChange, HeadReason, JournalEvent, MAX_COUNTER, ModelChanged, ModelError,
    ModelErrorCode, ModelRef, Portability, Provenance, ResponseOutcome, RuntimeState,
    SessionSnapshot, StateChanged, ToolMetadata, ToolResult, ToolResultEntry, ToolStatus,
    TurnFinished, TurnId, TurnOutcome, TurnStarted, UserMessage, codec, store,
};

use super::{
    env::Env,
    types::*,
    view::{ClientView, PartialBlock, PartialResponse, Phase, RuntimeView},
};
use crate::session::FinishReason;

/// JSON overhead charged per response block, so many tiny blocks cannot grow
/// a response commit past the journal's line limit.
const BLOCK_OVERHEAD: usize = 160;
/// Text of the runtime result answering each call while tools are unavailable.
const TOOLS_UNAVAILABLE: &str = "tools_unavailable: this runtime executes no tools";
/// Text of the runtime result answering each call a cancellation left unstarted.
const NOT_STARTED: &str = "cancelled: the turn was cancelled before this call started";

/// The pure agent runtime. `step` performs no I/O, reads no clock and creates
/// no identity except through `Env`; everything it does is in the effects.
#[derive(Debug)]
pub struct Runtime {
    config: RuntimeConfig,
    mode: Mode,
    /// Published snapshot, phase and partial response, changed only by
    /// applying emitted events, so `view()` always matches the event stream.
    view: ClientView,
    out: Vec<Effect>,
    next_token: u64,
    next_inference: u64,
    next_timer: u64,
    in_flight: Option<InFlight>,
    verifying: Option<Verification>,
    inference: Option<Inference>,
    /// Inferences started by the active turn; storage does not count them.
    inferences: u32,
    cancel: Option<CancelRequest>,
    stale_events: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Ready,
    RecoveryRequired,
    Faulted,
}

/// The single commit awaiting its answer.
#[derive(Debug)]
struct InFlight {
    token: CommitToken,
    waiters: Vec<(RequestId, Waiter)>,
    opening: Option<Opening>,
    finished: Option<TurnFinished>,
    ends_inference: bool,
}

#[derive(Debug)]
struct Opening {
    command_id: CommandId,
    fingerprint: Digest,
    turn_id: TurnId,
}

#[derive(Debug)]
enum Waiter {
    TurnStarted(TurnId),
    HeadSelected,
    ModelSwitched,
}

/// A media opening command reserves the same exclusive slot as a commit.
#[derive(Debug)]
struct Verification {
    token: CommitToken,
    command_id: CommandId,
    fingerprint: Digest,
    content: Vec<ContentBlock>,
    waiters: Vec<RequestId>,
}

/// A requested cancellation, answered once it is durable.
#[derive(Debug)]
struct CancelRequest {
    turn_id: TurnId,
    waiters: Vec<RequestId>,
}

/// Runtime-only facts about the current stream. Its content lives in the view.
#[derive(Debug)]
struct Inference {
    id: InferenceId,
    started: bool,
    bytes: usize,
    status: Stream,
    /// The armed cancellation grace timer.
    timer: Option<TimerId>,
}

#[derive(Debug)]
enum Stream {
    Open,
    /// `CancelInference` was emitted; waiting for the stream to end or grace.
    Cancelling,
    Ended(End),
}

/// A copyable summary of `Stream`, for decisions that then borrow `self`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Open,
    Cancelling,
    Ended,
}
impl Stream {
    fn status(&self) -> Status {
        match self {
            Self::Open => Status::Open,
            Self::Cancelling => Status::Cancelling,
            Self::Ended(_) => Status::Ended,
        }
    }
}

#[derive(Debug)]
enum End {
    /// A valid `Finished` event.
    Finished {
        outcome: ProviderOutcome,
        finish_reason: Option<FinishReason>,
        error: Option<ModelError>,
    },
    /// Closed without `Finished`.
    Eof,
    /// Exceeded `max_partial_bytes`.
    TooLarge,
    /// Broke the provider protocol.
    Malformed(&'static str),
    /// Ended or timed out after a requested cancellation.
    Cancelled,
}

/// How a finished stream is recorded.
struct Recorded {
    outcome: ResponseOutcome,
    finish_reason: Option<FinishReason>,
    error: Option<ModelError>,
    eligible: bool,
    processing: bool,
    finished: Option<(TurnOutcome, Option<&'static str>)>,
}

impl Runtime {
    /// Wrap a published snapshot. Emits nothing; the first input produces the
    /// first effects. A snapshot with unfinished work starts `RecoveryRequired`.
    pub fn new(snapshot: SessionSnapshot, config: RuntimeConfig, env: &mut impl Env) -> Self {
        let mode = if snapshot.requires_recovery() || snapshot.state() != RuntimeState::Idle {
            Mode::RecoveryRequired
        } else {
            Mode::Ready
        };
        let view = RuntimeView {
            epoch: env.runtime_epoch(),
            event_seq: 0,
            phase: phase(mode, &snapshot),
            active_turn: snapshot.active_turn().cloned(),
            snapshot,
            partial: None,
        };
        Self {
            config,
            mode,
            view: view.into(),
            out: Vec::new(),
            next_token: 0,
            next_inference: 0,
            next_timer: 0,
            in_flight: None,
            verifying: None,
            inference: None,
            inferences: 0,
            cancel: None,
            stale_events: 0,
        }
    }

    /// The resync view: published snapshot plus live state, as of `event_seq`.
    pub fn view(&self) -> RuntimeView {
        self.view.view().clone()
    }

    /// The last snapshot published by a `Persisted` input.
    pub fn snapshot(&self) -> &SessionSnapshot {
        &self.view.view().snapshot
    }

    /// This runtime incarnation's epoch.
    pub fn epoch(&self) -> &RuntimeEpoch {
        &self.view.view().epoch
    }

    /// The current phase.
    pub fn phase(&self) -> Phase {
        self.view.view().phase
    }

    /// The configured limits.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// Process one input. Returns every effect it causes, in order.
    pub fn step(&mut self, input: Input, env: &mut impl Env) -> Vec<Effect> {
        let env: &mut dyn Env = env;
        match input {
            Input::PublishView => self.emit(EventKind::PhaseChanged {
                phase: self.phase(),
                active_turn: self.snapshot().active_turn().cloned(),
            }),
            Input::ArtifactsVerified { token, missing } => {
                self.artifacts_verified(token, missing.is_empty(), env)
            }
            Input::StorageFailed { error } if self.mode != Mode::Faulted => {
                self.fault(FaultCause::Persist(error))
            }
            Input::StorageFailed { .. } => {}
            Input::DriverInvariant { code } if self.mode != Mode::Faulted => {
                self.fault(FaultCause::Internal(code))
            }
            Input::DriverInvariant { .. } => {}
            Input::Command { request, command } => self.command(request, command, env),
            Input::Persisted {
                token,
                ack,
                snapshot,
            } => self.persisted(token, ack, snapshot, env),
            Input::PersistFailed { token, error } => {
                if self.mode != Mode::Faulted {
                    let cause = match &self.in_flight {
                        Some(in_flight) if in_flight.token == token => FaultCause::Persist(error),
                        _ if self.verifying.as_ref().is_some_and(|v| v.token == token) => {
                            FaultCause::Persist(error)
                        }
                        _ => FaultCause::UnexpectedAnswer(token),
                    };
                    self.fault(cause);
                }
            }
            Input::Provider { inference, event } => self.provider(inference, event, env),
            Input::ProviderEnded { inference } => self.provider_ended(inference, env),
            Input::TimerFired { timer } => self.timer(timer, env),
        }
        std::mem::take(&mut self.out)
    }

    // ----- commands -------------------------------------------------------

    fn command(&mut self, request: RequestId, command: Command, env: &mut dyn Env) {
        let result = match command {
            Command::StartTurn {
                command_id,
                content,
            } => self.start_turn(request, command_id, content, env),
            Command::Cancel { turn_id } => self.cancel(request, turn_id, env),
            Command::SelectHead { head } => self.select_head(request, head, env),
            Command::SwitchModel { model } => self.switch_model(request, model, env),
        };
        if let Some(result) = result {
            self.reply(request, result);
        }
    }

    /// Common refusals of commands that need an idle, healthy session.
    fn idle_check(&self) -> Result<(), Rejection> {
        match self.mode {
            Mode::Faulted => Err(Rejection::Faulted),
            Mode::RecoveryRequired => Err(Rejection::RecoveryRequired),
            Mode::Ready
                if self.in_flight.is_some()
                    || self.verifying.is_some()
                    || self.snapshot().active_turn().is_some() =>
            {
                Err(Rejection::Busy)
            }
            Mode::Ready => Ok(()),
        }
    }

    fn start_turn(
        &mut self,
        request: RequestId,
        command_id: CommandId,
        content: Vec<ContentBlock>,
        env: &mut dyn Env,
    ) -> Option<Result<Accepted, Rejection>> {
        if self.mode == Mode::Faulted {
            return Some(Err(Rejection::Faulted));
        }
        let command = json!({ "kind": "start_turn", "content": content });
        let Ok(normalized) = store::normalized_command(&command) else {
            return Some(Err(Rejection::Invalid("content_too_large")));
        };
        let fingerprint = Digest::of(&normalized);
        // Deduplicate before any busy check: a retry learns the original outcome.
        if let Some(receipt) = self.snapshot().command_receipt(&command_id) {
            let turn_id = receipt.commit.events.iter().find_map(|event| match event {
                JournalEvent::TurnStarted(start) => Some(start.turn_id.clone()),
                _ => None,
            });
            return Some(match turn_id {
                Some(turn_id) if receipt.fingerprint == fingerprint => Ok(Accepted::TurnStarted {
                    turn_id,
                    ack: receipt.acknowledgement.clone(),
                }),
                _ => Err(Rejection::StaleCommand),
            });
        }
        if let Some(in_flight) = &mut self.in_flight
            && let Some(opening) = &in_flight.opening
            && opening.command_id == command_id
        {
            if opening.fingerprint != fingerprint {
                return Some(Err(Rejection::StaleCommand));
            }
            let turn_id = opening.turn_id.clone();
            in_flight
                .waiters
                .push((request, Waiter::TurnStarted(turn_id)));
            return None;
        }
        if let Some(verifying) = &mut self.verifying
            && verifying.command_id == command_id
        {
            if verifying.fingerprint != fingerprint {
                return Some(Err(Rejection::StaleCommand));
            }
            verifying.waiters.push(request);
            return None;
        }
        if let Err(rejection) = self.idle_check() {
            return Some(Err(rejection));
        }
        if content.is_empty() {
            return Some(Err(Rejection::Invalid("empty_content")));
        }
        if !content.iter().all(|block| {
            matches!(
                block,
                ContentBlock::Text { .. }
                    | ContentBlock::Image { .. }
                    | ContentBlock::Audio { .. }
                    | ContentBlock::File { .. }
            )
        }) {
            return Some(Err(Rejection::Invalid("unsupported_content")));
        }
        if content.iter().any(|block| match block {
            ContentBlock::Image { mime_type, .. } | ContentBlock::Audio { mime_type, .. } => {
                mime_type.is_empty()
            }
            ContentBlock::File {
                mime_type, name, ..
            } => mime_type.is_empty() || name.is_empty(),
            _ => false,
        }) {
            return Some(Err(Rejection::Invalid("invalid_media")));
        }
        if normalized.len() > self.config.max_user_bytes {
            return Some(Err(Rejection::Invalid("content_too_large")));
        }
        let snapshot = self.snapshot().clone();
        if snapshot.has_identity(command_id.as_str()) {
            return Some(Err(Rejection::Invalid("command_id_in_use")));
        }
        if !matches!(snapshot.0.pending(snapshot.head()), Ok(pending) if pending.is_empty()) {
            return Some(Err(Rejection::Invalid("head_inside_tool_exchange")));
        }
        let refs: Vec<_> = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Image { artifact, .. }
                | ContentBlock::Audio { artifact, .. }
                | ContentBlock::File { artifact, .. } => Some(artifact.clone()),
                _ => None,
            })
            .collect();
        if !refs.is_empty() {
            self.next_token += 1;
            let token = CommitToken(self.next_token);
            self.verifying = Some(Verification {
                token,
                command_id,
                fingerprint,
                content,
                waiters: vec![request],
            });
            self.out.push(Effect::VerifyArtifacts { token, refs });
        } else {
            self.open_turn(command_id, content, fingerprint, vec![request], env);
        }
        None
    }

    fn artifacts_verified(&mut self, token: CommitToken, valid: bool, env: &mut dyn Env) {
        if self.mode == Mode::Faulted {
            return;
        }
        if self.verifying.as_ref().is_none_or(|v| v.token != token) {
            return self.fault(FaultCause::UnexpectedAnswer(token));
        }
        let verification = self.verifying.take().expect("checked above");
        if valid {
            self.open_turn(
                verification.command_id,
                verification.content,
                verification.fingerprint,
                verification.waiters,
                env,
            );
        } else {
            for request in verification.waiters {
                self.reply(request, Err(Rejection::Invalid("artifact_missing")));
            }
        }
    }

    fn open_turn(
        &mut self,
        command_id: CommandId,
        content: Vec<ContentBlock>,
        fingerprint: Digest,
        waiters: Vec<RequestId>,
        env: &mut dyn Env,
    ) {
        let snapshot = self.snapshot().clone();
        let command = json!({ "kind": "start_turn", "content": content });
        let turn_id = env.turn_id();
        let entries = chain(
            &snapshot,
            vec![(
                Some(turn_id.clone()),
                EntryPayload::UserMessage(UserMessage { content }),
            )],
            env,
        );
        let events = vec![
            JournalEvent::TurnStarted(TurnStarted {
                turn_id: turn_id.clone(),
                user_entry_id: entries[0].id.clone(),
                configuration: snapshot.configuration().clone(),
            }),
            transition(&turn_id, RuntimeState::Idle, RuntimeState::Preparing),
        ];
        self.persist(
            advancing(&snapshot, entries, events),
            InFlight {
                token: CommitToken(0),
                waiters: waiters
                    .into_iter()
                    .map(|request| (request, Waiter::TurnStarted(turn_id.clone())))
                    .collect(),
                opening: Some(Opening {
                    command_id: command_id.clone(),
                    fingerprint,
                    turn_id,
                }),
                finished: None,
                ends_inference: false,
            },
            Some((command_id, command)),
            env,
        );
    }

    fn cancel(
        &mut self,
        request: RequestId,
        turn_id: TurnId,
        env: &mut dyn Env,
    ) -> Option<Result<Accepted, Rejection>> {
        match self.mode {
            Mode::Faulted => return Some(Err(Rejection::Faulted)),
            Mode::RecoveryRequired => return Some(Err(Rejection::RecoveryRequired)),
            Mode::Ready => {}
        }
        if self.snapshot().active_turn() != Some(&turn_id) {
            return Some(Err(Rejection::NotActive));
        }
        self.cancel
            .get_or_insert_with(|| CancelRequest {
                turn_id,
                waiters: Vec::new(),
            })
            .waiters
            .push(request);
        // Stop the stream at once; durable records follow when nothing is in flight.
        if let Some(inference) = &mut self.inference
            && matches!(inference.status, Stream::Open)
        {
            inference.status = Stream::Cancelling;
            let id = inference.id;
            self.next_timer += 1;
            let timer = TimerId(self.next_timer);
            inference.timer = Some(timer);
            self.out.push(Effect::CancelInference { inference: id });
            self.out.push(Effect::SetTimer {
                timer,
                duration: self.config.cancel_grace,
            });
        }
        self.advance(env);
        None
    }

    fn select_head(
        &mut self,
        request: RequestId,
        head: Option<EntryId>,
        env: &mut dyn Env,
    ) -> Option<Result<Accepted, Rejection>> {
        if let Err(rejection) = self.idle_check() {
            return Some(Err(rejection));
        }
        let snapshot = self.snapshot().clone();
        if head.as_ref() == snapshot.head() {
            return Some(Err(Rejection::Invalid("head_unchanged")));
        }
        match snapshot.0.pending(head.as_ref()) {
            Ok([]) => {}
            Ok(_) => return Some(Err(Rejection::Invalid("head_inside_tool_exchange"))),
            Err(_) => return Some(Err(Rejection::Invalid("unknown_head"))),
        }
        let commit = Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: snapshot.head().cloned(),
                to: head,
                reason: HeadReason::Navigate,
            },
            entries: Vec::new(),
            events: Vec::new(),
        };
        self.persist(commit, waiting(request, Waiter::HeadSelected), None, env);
        None
    }

    fn switch_model(
        &mut self,
        request: RequestId,
        model: ModelRef,
        env: &mut dyn Env,
    ) -> Option<Result<Accepted, Rejection>> {
        if let Err(rejection) = self.idle_check() {
            return Some(Err(rejection));
        }
        if model.provider_instance.is_empty() || model.model.is_empty() {
            return Some(Err(Rejection::Invalid("empty_model")));
        }
        let snapshot = self.snapshot().clone();
        if !matches!(snapshot.0.pending(snapshot.head()), Ok(pending) if pending.is_empty()) {
            return Some(Err(Rejection::Invalid("head_inside_tool_exchange")));
        }
        // Portability previews arrive with the context builder; until then the
        // default policy is recorded.
        let entries = chain(
            &snapshot,
            vec![(
                None,
                EntryPayload::ModelChanged(ModelChanged {
                    model,
                    portability: Portability::Reject,
                }),
            )],
            env,
        );
        self.persist(
            advancing(&snapshot, entries, Vec::new()),
            waiting(request, Waiter::ModelSwitched),
            None,
            env,
        );
        None
    }

    // ----- storage answers ------------------------------------------------

    fn persisted(
        &mut self,
        token: CommitToken,
        ack: CommitAck,
        snapshot: SessionSnapshot,
        env: &mut dyn Env,
    ) {
        if self.mode == Mode::Faulted {
            return;
        }
        if self.in_flight.as_ref().is_none_or(|f| f.token != token) {
            return self.fault(FaultCause::UnexpectedAnswer(token));
        }
        let previous = self.snapshot();
        if snapshot.session_id() != previous.session_id()
            || Some(ack.sequence) != previous.sequence().checked_add(1)
            || snapshot.sequence() != ack.sequence
            || snapshot.checksum() != &ack.checksum
            || snapshot.head() != ack.head.as_ref()
        {
            return self.fault(FaultCause::Internal("acknowledgement_mismatch"));
        }
        let in_flight = self.in_flight.take().expect("checked above");
        self.emit(EventKind::Committed {
            ack: ack.clone(),
            snapshot,
        });
        if let Some(finished) = in_flight.finished {
            self.inferences = 0;
            self.emit(EventKind::TurnFinished {
                turn_id: finished.turn_id,
                outcome: finished.outcome,
                code: finished.code,
            });
        }
        if in_flight.ends_inference
            && let Some(inference) = self.inference.take()
        {
            if let Some(timer) = inference.timer {
                self.out.push(Effect::CancelTimer { timer });
            }
            self.emit(EventKind::PartialCleared {
                inference: inference.id,
            });
        }
        self.sync_phase();
        for (request, waiter) in in_flight.waiters {
            let accepted = match waiter {
                Waiter::TurnStarted(turn_id) => Accepted::TurnStarted {
                    turn_id,
                    ack: ack.clone(),
                },
                Waiter::HeadSelected => Accepted::HeadSelected { ack: ack.clone() },
                Waiter::ModelSwitched => Accepted::ModelSwitched { ack: ack.clone() },
            };
            self.reply(request, Ok(accepted));
        }
        self.resolve_cancel(&ack);
        self.advance(env);
    }

    /// Answer cancellation requests once storage shows their outcome.
    fn resolve_cancel(&mut self, ack: &CommitAck) {
        let Some(cancel) = &self.cancel else { return };
        let snapshot = self.snapshot();
        let result = if snapshot.active_turn() == Some(&cancel.turn_id) {
            if snapshot.state() != RuntimeState::Cancelling {
                return;
            }
            Ok(())
        } else if snapshot.turn(&cancel.turn_id).and_then(|turn| turn.outcome)
            == Some(TurnOutcome::Cancelled)
        {
            Ok(())
        } else {
            Err(Rejection::NotActive)
        };
        let cancel = self.cancel.take().expect("checked above");
        for request in cancel.waiters {
            let result = result.map(|()| Accepted::Cancelling {
                turn_id: cancel.turn_id.clone(),
                ack: ack.clone(),
            });
            self.reply(request, result);
        }
    }

    /// Stop for good: no retries, no further effects except rejections.
    fn fault(&mut self, cause: FaultCause) {
        self.mode = Mode::Faulted;
        if let Some(verification) = self.verifying.take() {
            for request in verification.waiters {
                self.reply(request, Err(Rejection::Faulted));
            }
        }
        if let Some(inference) = self.inference.take() {
            if !matches!(inference.status, Stream::Ended(_)) {
                self.out.push(Effect::ReleaseInference {
                    inference: inference.id,
                });
            }
            if let Some(timer) = inference.timer {
                self.out.push(Effect::CancelTimer { timer });
            }
            if self.view.view().partial.is_some() {
                self.emit(EventKind::PartialCleared {
                    inference: inference.id,
                });
            }
        }
        self.emit(EventKind::Fault(cause));
        self.sync_phase();
        let waiters = self.in_flight.take().map(|f| f.waiters).unwrap_or_default();
        for (request, _) in waiters {
            self.reply(request, Err(Rejection::Faulted));
        }
        for request in self.cancel.take().map(|c| c.waiters).unwrap_or_default() {
            self.reply(request, Err(Rejection::Faulted));
        }
    }

    // ----- provider stream ------------------------------------------------

    fn stale(&mut self, inference: InferenceId) {
        self.stale_events += 1;
        self.emit(EventKind::Diagnostic(
            RuntimeDiagnostic::StaleProviderEvent {
                inference,
                dropped: self.stale_events,
            },
        ));
    }

    fn provider(&mut self, id: InferenceId, event: ProviderEvent, env: &mut dyn Env) {
        if self.mode == Mode::Faulted {
            return;
        }
        let finished = matches!(event, ProviderEvent::Finished { .. });
        match self.status_of(id) {
            Some(Status::Open) => self.stream_event(id, event, env),
            Some(Status::Cancelling) if finished => self.cancel_ended(env),
            _ => self.stale(id),
        }
    }

    /// Status of the stream `id`, if it is the current one.
    fn status_of(&self, id: InferenceId) -> Option<Status> {
        self.inference
            .as_ref()
            .filter(|inference| inference.id == id)
            .map(|inference| inference.status.status())
    }

    fn stream_event(&mut self, id: InferenceId, event: ProviderEvent, env: &mut dyn Env) {
        let max = self.config.max_partial_bytes;
        let inference = self.inference.as_mut().expect("current inference");
        let partial = self
            .view
            .view()
            .partial
            .as_ref()
            .expect("partial exists while streaming");
        let block = |index: &u32| partial.blocks.get(index);
        // Validate first; the view changes only through the emitted event.
        let (kind, cost) = match event {
            ProviderEvent::Started => {
                if inference.started {
                    return self.malformed("duplicate_start", env);
                }
                inference.started = true;
                return;
            }
            _ if !inference.started && !matches!(event, ProviderEvent::Finished { .. }) => {
                return self.malformed("content_before_start", env);
            }
            ProviderEvent::TextDelta { block_index, text } => {
                let new = match block(&block_index) {
                    None => true,
                    Some(PartialBlock::Text(_)) => false,
                    Some(_) => return self.malformed("block_kind_mismatch", env),
                };
                let cost = json_len(&text) + if new { BLOCK_OVERHEAD } else { 0 };
                (
                    EventKind::TextDelta {
                        inference: id,
                        block_index,
                        text,
                    },
                    cost,
                )
            }
            ProviderEvent::ReasoningSummaryDelta { block_index, text } => {
                let new = match block(&block_index) {
                    None => true,
                    Some(PartialBlock::ReasoningSummary(_)) => false,
                    Some(_) => return self.malformed("block_kind_mismatch", env),
                };
                let cost = json_len(&text) + if new { BLOCK_OVERHEAD } else { 0 };
                (
                    EventKind::ReasoningSummaryDelta {
                        inference: id,
                        block_index,
                        text,
                    },
                    cost,
                )
            }
            ProviderEvent::ToolCallStarted {
                block_index,
                name,
                provider_call_id,
            } => {
                if block(&block_index).is_some() {
                    return self.malformed("duplicate_block", env);
                }
                if name.is_empty() {
                    return self.malformed("empty_tool_name", env);
                }
                let cost = BLOCK_OVERHEAD
                    + json_len(&name)
                    + provider_call_id.as_deref().map_or(0, json_len);
                // Charge before issuing an identity, so an oversized call gets none.
                if inference.bytes + cost > max {
                    return self.too_large(env);
                }
                (
                    EventKind::ToolCallStarted {
                        inference: id,
                        block_index,
                        call_id: env.call_id(),
                        name,
                        provider_call_id,
                    },
                    cost,
                )
            }
            ProviderEvent::ToolArgumentsDelta {
                block_index,
                fragment,
            } => {
                match block(&block_index) {
                    Some(PartialBlock::ToolCall { ready: false, .. }) => {}
                    Some(PartialBlock::ToolCall { ready: true, .. }) => {
                        return self.malformed("arguments_after_ready", env);
                    }
                    _ => return self.malformed("unknown_tool_call", env),
                }
                let cost = fragment.len();
                (
                    EventKind::ToolArgumentsDelta {
                        inference: id,
                        block_index,
                        fragment,
                    },
                    cost,
                )
            }
            ProviderEvent::ToolCallReady { block_index } => {
                let arguments = match block(&block_index) {
                    Some(PartialBlock::ToolCall {
                        ready: false,
                        arguments,
                        ..
                    }) => arguments,
                    Some(PartialBlock::ToolCall { ready: true, .. }) => {
                        return self.malformed("duplicate_ready", env);
                    }
                    _ => return self.malformed("unknown_tool_call", env),
                };
                let Some(object) = arguments_object(arguments) else {
                    return self.malformed("invalid_tool_arguments", env);
                };
                // Reserialization can be longer than the fragments; charge it.
                let Ok(serialized) = codec::bounded_json(&object) else {
                    return self.too_large(env);
                };
                let cost = serialized.len().saturating_sub(arguments.len());
                (
                    EventKind::ToolCallReady {
                        inference: id,
                        block_index,
                    },
                    cost,
                )
            }
            ProviderEvent::UsageUpdated { usage } => {
                let counts = [
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cached_input_tokens,
                    usage.reasoning_tokens,
                ];
                if counts.iter().flatten().any(|count| *count > MAX_COUNTER) {
                    return self.malformed("usage_out_of_range", env);
                }
                (
                    EventKind::UsageUpdated {
                        inference: id,
                        usage,
                    },
                    0,
                )
            }
            ProviderEvent::Finished {
                outcome,
                finish_reason,
                error,
            } => return self.on_finished(outcome, finish_reason, error, env),
        };
        if inference.bytes + cost > max {
            return self.too_large(env);
        }
        inference.bytes += cost;
        self.emit(kind);
    }

    fn on_finished(
        &mut self,
        outcome: ProviderOutcome,
        finish_reason: Option<FinishReason>,
        error: Option<ModelError>,
        env: &mut dyn Env,
    ) {
        let max = self.config.max_partial_bytes;
        let inference = self.inference.as_ref().expect("current inference");
        let partial = self.view.view().partial.as_ref().expect("partial");
        let calls: Vec<bool> = partial
            .blocks
            .values()
            .filter_map(|block| match block {
                PartialBlock::ToolCall { ready, .. } => Some(*ready),
                _ => None,
            })
            .collect();
        let reason = match (outcome, finish_reason, &error) {
            (ProviderOutcome::Completed, _, _) if !inference.started => {
                Some("completed_before_start")
            }
            (ProviderOutcome::Completed, Some(FinishReason::ToolCalls), None) => {
                (calls.is_empty() || calls.contains(&false)).then_some("incomplete_tool_calls")
            }
            (
                ProviderOutcome::Completed,
                Some(FinishReason::Stop | FinishReason::Refusal),
                None,
            ) => (!calls.is_empty()).then_some("calls_without_tool_finish"),
            (ProviderOutcome::Completed, Some(FinishReason::OutputLimit), None) => None,
            (ProviderOutcome::Completed, _, _) => Some("invalid_completed_finish"),
            (ProviderOutcome::Failed, None, Some(error)) => {
                let cost =
                    json_len(&error.message) + error.provider_code.as_deref().map_or(0, json_len);
                if error.message.is_empty()
                    || error.provider_code.as_ref().is_some_and(String::is_empty)
                    || error.retry_after_ms.is_some_and(|ms| ms > MAX_COUNTER)
                {
                    Some("invalid_error")
                } else if inference.bytes + cost > max {
                    Some("oversized_error")
                } else {
                    None
                }
            }
            (ProviderOutcome::Failed, _, _) => Some("invalid_failed_finish"),
            (ProviderOutcome::Cancelled, None, None) => None,
            (ProviderOutcome::Cancelled, _, _) => Some("invalid_cancelled_finish"),
        };
        if let Some(reason) = reason {
            return self.malformed(reason, env);
        }
        self.end(
            End::Finished {
                outcome,
                finish_reason,
                error,
            },
            env,
        );
    }

    fn provider_ended(&mut self, id: InferenceId, env: &mut dyn Env) {
        if self.mode == Mode::Faulted {
            return;
        }
        match self.status_of(id) {
            Some(Status::Open) => self.end(End::Eof, env),
            Some(Status::Cancelling) => self.cancel_ended(env),
            // The normal close after `Finished`, or of a stream already recorded.
            Some(Status::Ended) | None => {}
        }
    }

    fn timer(&mut self, timer: TimerId, env: &mut dyn Env) {
        if self.mode == Mode::Faulted {
            return;
        }
        if let Some(inference) = &mut self.inference
            && inference.timer == Some(timer)
        {
            inference.timer = None;
            self.end(End::Cancelled, env);
        } else {
            self.emit(EventKind::Diagnostic(RuntimeDiagnostic::StaleTimer {
                timer,
            }));
        }
    }

    fn cancel_ended(&mut self, env: &mut dyn Env) {
        let inference = self.inference.as_mut().expect("current inference");
        if let Some(timer) = inference.timer.take() {
            self.out.push(Effect::CancelTimer { timer });
        }
        self.end(End::Cancelled, env);
    }

    fn malformed(&mut self, reason: &'static str, env: &mut dyn Env) {
        let id = self.inference.as_ref().expect("current inference").id;
        self.emit(EventKind::Diagnostic(RuntimeDiagnostic::MalformedStream {
            inference: id,
            reason,
        }));
        self.end(End::Malformed(reason), env);
    }

    fn too_large(&mut self, env: &mut dyn Env) {
        self.end(End::TooLarge, env);
    }

    /// The core stops listening: release the stream, then decide what follows.
    fn end(&mut self, end: End, env: &mut dyn Env) {
        let inference = self.inference.as_mut().expect("current inference");
        inference.status = Stream::Ended(end);
        let id = inference.id;
        self.out.push(Effect::ReleaseInference { inference: id });
        self.advance(env);
    }

    // ----- turn progression -----------------------------------------------

    /// Issue the next durable step of the active turn, if nothing is in flight.
    /// Every decision reads the published snapshot; only what storage cannot
    /// know (the stream, the cancel request, the inference count) is local.
    fn advance(&mut self, env: &mut dyn Env) {
        if self.mode != Mode::Ready || self.in_flight.is_some() || self.verifying.is_some() {
            return;
        }
        let snapshot = self.snapshot().clone();
        let Some(turn) = snapshot.active_turn().cloned() else {
            return;
        };
        let cancel = self.cancel.is_some();
        let limit = self.inferences >= self.config.max_inferences_per_turn;
        let stream = self.inference.as_ref().map(|i| i.status.status());
        use RuntimeState::*;
        match snapshot.state() {
            Preparing if cancel => {
                self.finish(&snapshot, &turn, Some(Preparing), cancelled(&turn), env)
            }
            Preparing if limit => self.finish(
                &snapshot,
                &turn,
                None,
                turn_finished(&turn, TurnOutcome::Failed, Some("inference_limit")),
                env,
            ),
            Preparing => {
                let commit = audit(&snapshot, vec![transition(&turn, Preparing, Inferring)]);
                self.persist(commit, idle_flight(), None, env);
            }
            Inferring => match stream {
                None if cancel => {
                    self.finish(&snapshot, &turn, Some(Inferring), cancelled(&turn), env)
                }
                None => self.start_inference(&snapshot, turn, env),
                Some(Status::Cancelling) if cancel => {
                    let commit = audit(&snapshot, vec![transition(&turn, Inferring, Cancelling)]);
                    self.persist(commit, idle_flight(), None, env);
                }
                Some(Status::Open | Status::Cancelling) => {}
                Some(Status::Ended) => self.record_response(&snapshot, turn, env),
            },
            Cancelling => match stream {
                None => self.finish(&snapshot, &turn, None, cancelled(&turn), env),
                Some(Status::Open | Status::Cancelling) => {}
                Some(Status::Ended) => self.record_response(&snapshot, turn, env),
            },
            Processing => self.answer_calls(&snapshot, turn, cancel, limit, env),
            Continuing => {
                let commit = audit(&snapshot, vec![transition(&turn, Continuing, Preparing)]);
                self.persist(commit, idle_flight(), None, env);
            }
            Idle | Authorizing | Executing | Recovering => {
                self.fault(FaultCause::Internal("unexpected_runtime_state"))
            }
        }
    }

    /// Terminate the turn with no response, from `from` through `Cancelling`
    /// when cancelling.
    fn finish(
        &mut self,
        snapshot: &SessionSnapshot,
        turn: &TurnId,
        cancelling_from: Option<RuntimeState>,
        finished: TurnFinished,
        env: &mut dyn Env,
    ) {
        let mut events = Vec::new();
        if let Some(from) = cancelling_from {
            events.push(transition(turn, from, RuntimeState::Cancelling));
        }
        events.push(JournalEvent::TurnFinished(finished.clone()));
        self.persist(
            audit(snapshot, events),
            InFlight {
                finished: Some(finished),
                ..idle_flight()
            },
            None,
            env,
        );
    }

    fn start_inference(&mut self, snapshot: &SessionSnapshot, turn_id: TurnId, env: &mut dyn Env) {
        let Some(head) = snapshot.head().cloned() else {
            return self.fault(FaultCause::Internal("inference_without_head"));
        };
        let Ok(model) = snapshot.model_at(Some(&head)).cloned() else {
            return self.fault(FaultCause::Internal("inference_head_unknown"));
        };
        self.next_inference += 1;
        let inference = InferenceId(self.next_inference);
        let response_id = env.response_id();
        self.inferences += 1;
        self.inference = Some(Inference {
            id: inference,
            started: false,
            bytes: 0,
            status: Stream::Open,
            timer: None,
        });
        self.emit(EventKind::InferenceStarted {
            inference,
            turn_id: turn_id.clone(),
            response_id: response_id.clone(),
            model: model.clone(),
        });
        self.out.push(Effect::StartInference {
            inference,
            turn_id,
            head,
            model,
            response_id,
        });
    }

    /// Commit 3: the assistant entry plus whatever its outcome implies.
    fn record_response(&mut self, snapshot: &SessionSnapshot, turn: TurnId, env: &mut dyn Env) {
        let Some(Inference {
            status: Stream::Ended(end),
            ..
        }) = &self.inference
        else {
            return self.fault(FaultCause::Internal("response_without_end"));
        };
        let durable = snapshot.state();
        // A requested cancellation records the partial response as cancelled,
        // whatever the stream did after `CancelInference`.
        let requested = durable == RuntimeState::Cancelling || matches!(end, End::Cancelled);
        let recorded = if requested {
            Recorded::partial(ResponseOutcome::Cancelled, TurnOutcome::Cancelled, None)
        } else {
            Recorded::of(end)
        };
        let Some(partial) = self.view.view().partial.clone() else {
            return self.fault(FaultCause::Internal("response_without_partial"));
        };
        let Some(content) = content(&partial, recorded.eligible) else {
            return self.fault(FaultCause::Internal("eligible_call_unparseable"));
        };
        let entries = chain(
            snapshot,
            vec![(
                Some(turn.clone()),
                EntryPayload::AssistantResponse(AssistantResponse {
                    response_id: partial.response_id,
                    model: partial.model,
                    outcome: recorded.outcome,
                    finish_reason: recorded.finish_reason,
                    content,
                    usage: partial.usage,
                    continuation: None,
                    error: recorded.error,
                }),
            )],
            env,
        );
        let mut events = Vec::new();
        if requested && durable == RuntimeState::Inferring {
            events.push(transition(&turn, durable, RuntimeState::Cancelling));
        }
        if recorded.processing {
            events.push(transition(
                &turn,
                RuntimeState::Inferring,
                RuntimeState::Processing,
            ));
        }
        let finished = recorded
            .finished
            .map(|(outcome, code)| turn_finished(&turn, outcome, code));
        events.extend(finished.clone().map(JournalEvent::TurnFinished));
        self.persist(
            advancing(snapshot, entries, events),
            InFlight {
                finished,
                ends_inference: true,
                ..idle_flight()
            },
            None,
            env,
        );
    }

    /// Commit 4: answer every pending call in branch order without executing
    /// anything, then continue, stop at the inference limit, or cancel.
    fn answer_calls(
        &mut self,
        snapshot: &SessionSnapshot,
        turn: TurnId,
        cancel: bool,
        limit: bool,
        env: &mut dyn Env,
    ) {
        let pending: Vec<CallRef> = match snapshot.0.pending(snapshot.head()) {
            Ok(pending) if !pending.is_empty() => pending.to_vec(),
            _ => return self.fault(FaultCause::Internal("processing_without_calls")),
        };
        let (status, text) = if cancel {
            (ToolStatus::Cancelled, NOT_STARTED)
        } else {
            (ToolStatus::Denied, TOOLS_UNAVAILABLE)
        };
        let results = pending
            .into_iter()
            .map(|call| {
                (
                    Some(turn.clone()),
                    EntryPayload::ToolResult(ToolResultEntry {
                        result: ToolResult {
                            call,
                            invocation_id: None,
                            status,
                            content: vec![ContentBlock::Text { text: text.into() }],
                            metadata: ToolMetadata {
                                exit_code: None,
                                duration_ms: None,
                                output_truncated: false,
                                provenance: Provenance::Runtime,
                                artifacts: Vec::new(),
                            },
                        },
                    }),
                )
            })
            .collect();
        let entries = chain(snapshot, results, env);
        use RuntimeState::*;
        let (events, finished) = if cancel {
            let finished = cancelled(&turn);
            (
                vec![transition(&turn, Processing, Cancelling)],
                Some(finished),
            )
        } else if limit {
            let finished = turn_finished(&turn, TurnOutcome::Failed, Some("inference_limit"));
            (
                vec![transition(&turn, Processing, Continuing)],
                Some(finished),
            )
        } else {
            (
                vec![
                    transition(&turn, Processing, Continuing),
                    transition(&turn, Continuing, Preparing),
                ],
                None,
            )
        };
        let mut events = events;
        events.extend(finished.clone().map(JournalEvent::TurnFinished));
        self.persist(
            advancing(snapshot, entries, events),
            InFlight {
                finished,
                ..idle_flight()
            },
            None,
            env,
        );
    }

    // ----- primitives -----------------------------------------------------

    /// The only place a `Persist` is produced, so at most one is in flight.
    fn persist(
        &mut self,
        commit: Commit,
        mut in_flight: InFlight,
        command: Option<(CommandId, serde_json::Value)>,
        env: &mut dyn Env,
    ) {
        if self.in_flight.is_some() {
            return self.fault(FaultCause::Internal("second_commit_in_flight"));
        }
        self.next_token += 1;
        let token = CommitToken(self.next_token);
        in_flight.token = token;
        self.in_flight = Some(in_flight);
        self.out.push(Effect::Persist {
            token,
            commit,
            command,
            at: env.now(),
        });
    }

    fn emit(&mut self, kind: EventKind) {
        let event = RuntimeEvent {
            epoch: self.view.view().epoch.clone(),
            seq: self.view.view().event_seq + 1,
            kind,
        };
        self.view.apply(&event);
        self.out.push(Effect::Emit(event));
    }

    fn reply(&mut self, request: RequestId, result: Result<Accepted, Rejection>) {
        self.out.push(Effect::Reply { request, result });
    }

    fn sync_phase(&mut self) {
        let snapshot = self.snapshot();
        let phase = phase(self.mode, snapshot);
        let active_turn = snapshot.active_turn().cloned();
        let view = self.view.view();
        if view.phase != phase || view.active_turn != active_turn {
            self.emit(EventKind::PhaseChanged { phase, active_turn });
        }
    }
}

impl Recorded {
    /// A response that is not a completed answer and ends the turn.
    fn partial(outcome: ResponseOutcome, turn: TurnOutcome, code: Option<&'static str>) -> Self {
        Self {
            outcome,
            finish_reason: None,
            error: None,
            eligible: false,
            processing: false,
            finished: Some((turn, code)),
        }
    }

    fn of(end: &End) -> Self {
        let completed = |finish_reason, eligible, finished| Self {
            outcome: ResponseOutcome::Completed,
            finish_reason: Some(finish_reason),
            error: None,
            eligible,
            processing: true,
            finished,
        };
        match end {
            End::Finished {
                outcome: ProviderOutcome::Completed,
                finish_reason: Some(reason),
                ..
            } => match reason {
                FinishReason::Stop => {
                    completed(*reason, false, Some((TurnOutcome::Completed, None)))
                }
                FinishReason::ToolCalls => completed(*reason, true, None),
                FinishReason::Refusal => {
                    completed(*reason, false, Some((TurnOutcome::Completed, None)))
                }
                FinishReason::OutputLimit => completed(
                    *reason,
                    false,
                    Some((TurnOutcome::Failed, Some("output_limit"))),
                ),
            },
            End::Finished {
                outcome: ProviderOutcome::Failed,
                error: Some(error),
                ..
            } => Self {
                error: Some(error.clone()),
                ..Self::partial(
                    ResponseOutcome::Failed,
                    TurnOutcome::Failed,
                    Some(error_code(error.code)),
                )
            },
            End::Finished {
                outcome: ProviderOutcome::Cancelled,
                ..
            } => Self::partial(
                ResponseOutcome::Cancelled,
                TurnOutcome::Cancelled,
                Some("provider_cancelled"),
            ),
            End::Eof => Self::partial(
                ResponseOutcome::Interrupted,
                TurnOutcome::Failed,
                Some("provider_eof"),
            ),
            End::TooLarge => Self::partial(
                ResponseOutcome::Interrupted,
                TurnOutcome::Failed,
                Some("response_too_large"),
            ),
            End::Cancelled => {
                Self::partial(ResponseOutcome::Cancelled, TurnOutcome::Cancelled, None)
            }
            End::Malformed(reason) => Self::malformed(reason),
            // `on_finished` admits no other `Finished`.
            End::Finished { .. } => Self::malformed("invalid_finish"),
        }
    }

    fn malformed(reason: &'static str) -> Self {
        Self {
            error: Some(ModelError {
                code: ModelErrorCode::MalformedStream,
                message: reason.into(),
                provider_code: None,
                retry_after_ms: None,
            }),
            ..Self::partial(
                ResponseOutcome::Failed,
                TurnOutcome::Failed,
                Some("malformed_stream"),
            )
        }
    }
}

fn phase(mode: Mode, snapshot: &SessionSnapshot) -> Phase {
    match mode {
        Mode::Ready => Phase::Durable(snapshot.state()),
        Mode::RecoveryRequired => Phase::RecoveryRequired,
        Mode::Faulted => Phase::Faulted,
    }
}

fn idle_flight() -> InFlight {
    InFlight {
        token: CommitToken(0),
        waiters: Vec::new(),
        opening: None,
        finished: None,
        ends_inference: false,
    }
}

fn waiting(request: RequestId, waiter: Waiter) -> InFlight {
    InFlight {
        waiters: vec![(request, waiter)],
        ..idle_flight()
    }
}

/// New entries forming one chain from the current head.
fn chain(
    snapshot: &SessionSnapshot,
    payloads: Vec<(Option<TurnId>, EntryPayload)>,
    env: &mut dyn Env,
) -> Vec<Entry> {
    let mut parent = snapshot.head().cloned();
    payloads
        .into_iter()
        .map(|(turn_id, payload)| {
            let id = env.entry_id();
            Entry {
                id: id.clone(),
                parent_id: parent.replace(id),
                turn_id,
                payload,
            }
        })
        .collect()
}

/// A commit appending `entries` (nonempty) and advancing the head to the last.
fn advancing(snapshot: &SessionSnapshot, entries: Vec<Entry>, events: Vec<JournalEvent>) -> Commit {
    Commit {
        command_id: None,
        command_fingerprint: None,
        head: HeadChange {
            from: snapshot.head().cloned(),
            to: entries.last().map(|entry| entry.id.clone()),
            reason: HeadReason::Advance,
        },
        entries,
        events,
    }
}

/// An audit-only commit that retains the head.
fn audit(snapshot: &SessionSnapshot, events: Vec<JournalEvent>) -> Commit {
    Commit {
        command_id: None,
        command_fingerprint: None,
        head: HeadChange {
            from: snapshot.head().cloned(),
            to: snapshot.head().cloned(),
            reason: HeadReason::Retain,
        },
        entries: Vec::new(),
        events,
    }
}

fn transition(turn: &TurnId, from: RuntimeState, to: RuntimeState) -> JournalEvent {
    JournalEvent::StateChanged(StateChanged {
        turn_id: turn.clone(),
        from,
        to,
    })
}

fn turn_finished(turn: &TurnId, outcome: TurnOutcome, code: Option<&str>) -> TurnFinished {
    TurnFinished {
        turn_id: turn.clone(),
        outcome,
        code: code.map(str::to_owned),
    }
}

fn cancelled(turn: &TurnId) -> TurnFinished {
    turn_finished(turn, TurnOutcome::Cancelled, None)
}

/// Persisted content of a partial response. Tool calls appear only in an
/// eligible response, where every call is ready; partial calls are dropped.
fn content(partial: &PartialResponse, eligible: bool) -> Option<Vec<ContentBlock>> {
    let mut content = Vec::with_capacity(partial.blocks.len());
    for block in partial.blocks.values() {
        content.push(match block {
            PartialBlock::Text(text) => ContentBlock::Text { text: text.clone() },
            PartialBlock::ReasoningSummary(text) => {
                ContentBlock::ReasoningSummary { text: text.clone() }
            }
            PartialBlock::ToolCall { .. } if !eligible => continue,
            PartialBlock::ToolCall {
                call_id,
                name,
                provider_call_id,
                arguments,
                ready,
            } => {
                if !ready {
                    return None;
                }
                ContentBlock::ToolCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments_object(arguments)?,
                    provider_call_id: provider_call_id.clone(),
                }
            }
        });
    }
    Some(content)
}

/// Parse assembled arguments as one JSON object, rejecting duplicate keys as
/// storage does for every journal line.
fn arguments_object(arguments: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    match codec::unique_json(arguments.as_bytes()) {
        Ok(serde_json::Value::Object(object)) => Some(object),
        _ => None,
    }
}

/// Length of `text` once escaped as a JSON string body by `serde_json`.
fn json_len(text: &str) -> usize {
    text.chars()
        .map(|c| match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            c if (c as u32) < 0x20 => 6,
            c => c.len_utf8(),
        })
        .sum()
}

fn error_code(code: ModelErrorCode) -> &'static str {
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
