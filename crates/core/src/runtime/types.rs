//! Typed inputs, effects and events of the pure runtime core.

use std::{fmt, time::Duration};

use crate::context::{ContextConfig, ContextError, Flags, Totals};
use crate::model::ModelInput;
use crate::session::{
    ArtifactRef, CallId, CommandId, Commit, CommitAck, ContentBlock, Digest, EntryId, ModelRef,
    ResponseId, SessionSnapshot, StoreError, Timestamp, TurnId, TurnOutcome, Usage,
};

pub use crate::model::{ProviderEvent, ProviderOutcome};

use super::view::Phase;

macro_rules! local_id {
    ($($(#[$doc:meta])* $name:ident),+ $(,)?) => {$(
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u64);
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
        }
    )+};
}

local_id!(
    /// Client-chosen correlation for one command; answered by exactly one `Reply`.
    RequestId,
    /// Identifies the one commit in flight. Issued by the runtime, never journaled.
    CommitToken,
    /// Identifies one provider stream of this runtime. Never journaled.
    InferenceId,
    /// Identifies one driver timer of this runtime. Never journaled.
    TimerId,
);

/// UUIDv7 identity of one runtime incarnation, never journaled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeEpoch(EntryId);

impl RuntimeEpoch {
    /// Generate an epoch through the same UUIDv7 source as journal identities.
    pub fn new() -> Self {
        Self(EntryId::new())
    }
    /// Canonical UUIDv7 representation.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}
impl Default for RuntimeEpoch {
    fn default() -> Self {
        Self::new()
    }
}
impl TryFrom<String> for RuntimeEpoch {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Ok(Self(value.try_into()?))
    }
}

/// Limits that bound one runtime's work. They are not persisted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// Inferences one turn may start before it fails with `inference_limit`.
    pub max_inferences_per_turn: u32,
    /// Bound on a response's inline content, measured as JSON-escaped bytes of
    /// its text, reasoning summaries, tool names, provider call IDs and argument
    /// fragments. Keep it well below the 8 MiB journal line limit.
    pub max_partial_bytes: usize,
    /// Bound on a `StartTurn`'s content, measured as its compact JSON. Larger
    /// commands are rejected rather than risking a commit storage must refuse.
    pub max_user_bytes: usize,
    /// How long a cancelled stream may take to confirm before the runtime
    /// records the partial response without it.
    pub cancel_grace: Duration,
    /// How long an open stream may go without any event. The timer restarts
    /// after every event; on expiry the stream is released and the response
    /// recorded as interrupted, failing the turn with `provider_stalled`.
    pub provider_idle_timeout: Duration,
    /// Model limits, budget deductions and estimator of the context builder.
    pub context: ContextConfig,
}

impl Default for RuntimeConfig {
    /// RFC-001 defaults: 32 inferences and 4 MiB of inline response content.
    /// The idle timeout is generous because reasoning models can be silent
    /// for minutes before their first visible event.
    fn default() -> Self {
        Self {
            max_inferences_per_turn: 32,
            max_partial_bytes: 4 * 1024 * 1024,
            max_user_bytes: 1024 * 1024,
            cancel_grace: Duration::from_secs(2),
            provider_idle_timeout: Duration::from_secs(300),
            context: ContextConfig::default(),
        }
    }
}

/// Everything the core reacts to. One input yields one batch of effects.
#[derive(Clone, Debug)]
pub enum Input {
    /// Announce the current phase and epoch after attaching a replacement runtime.
    PublishView,
    /// Artifact preflight completed without changing the journal.
    ArtifactsVerified {
        /// Token of the preflight in flight.
        token: CommitToken,
        /// Missing, unsafe or invalid references; empty means all verified.
        missing: Vec<ArtifactRef>,
    },
    /// Worker exit or an artifact operation failed outside a pending commit.
    StorageFailed {
        /// Infrastructure failure; never retried.
        error: PersistError,
    },
    /// The driver could not perform an effect without breaking one of its own
    /// invariants. Faults with `FaultCause::Internal(code)`.
    DriverInvariant {
        /// Stable code naming the invariant.
        code: &'static str,
    },

    /// A client command, through the driver.
    Command {
        /// Correlation for the single reply.
        request: RequestId,
        /// The command.
        command: Command,
    },
    /// The storage worker synced the commit with this token.
    Persisted {
        /// Token of the commit in flight.
        token: CommitToken,
        /// Storage's acknowledgement.
        ack: CommitAck,
        /// Storage's snapshot after the commit.
        snapshot: SessionSnapshot,
    },
    /// The storage worker could not persist the commit with this token.
    PersistFailed {
        /// Token of the commit in flight.
        token: CommitToken,
        /// Why storage failed.
        error: PersistError,
    },
    /// One event of a provider stream.
    Provider {
        /// The stream it belongs to.
        inference: InferenceId,
        /// The event.
        event: ProviderEvent,
    },
    /// The provider stream closed, with or without `Finished`.
    ProviderEnded {
        /// The stream that closed.
        inference: InferenceId,
    },
    /// A timer set by `Effect::SetTimer` expired.
    TimerFired {
        /// The timer.
        timer: TimerId,
    },
}

/// Client commands in scope for Milestone 2.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Start a turn. `command_id` deduplicates retries through the journal.
    StartTurn {
        /// Journal deduplication key; never reused as the turn's identity.
        command_id: CommandId,
        /// Text or media content. Media is verified before the opening commit.
        content: Vec<ContentBlock>,
    },
    /// Cancel the active turn.
    Cancel {
        /// The turn the client believes is active.
        turn_id: TurnId,
    },
    /// Durably select another conversation head while idle.
    SelectHead {
        /// Target entry, or `None` for the empty root.
        head: Option<EntryId>,
    },
    /// Durably switch the model at the current head while idle.
    SwitchModel {
        /// The model for later inferences on this branch.
        model: ModelRef,
    },
}

/// Why a commit was not persisted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PersistError {
    /// The dedicated worker panicked or dropped a reply. Never retry.
    #[error("storage_worker_lost")]
    WorkerLost,
    /// An earlier I/O failure poisoned this worker until reopen.
    #[error("writer_poisoned")]
    Poisoned,
    /// Storage/open/upload was rejected with a stable code. Rejection of a
    /// core-generated append is a core bug.
    #[error("storage rejected: {code}")]
    Rejected {
        /// Storage's code, such as `illegal_state_transition`.
        code: String,
    },
    /// I/O failed; the writer is poisoned or its result is unknown.
    #[error("storage I/O: {message}")]
    Io {
        /// Operating-system description, without journal content.
        message: String,
    },
}

impl PersistError {
    /// Classify a storage error for the runtime.
    pub fn from_store(error: &StoreError) -> Self {
        let code = match error {
            StoreError::Io(error) => {
                return Self::Io {
                    message: error.to_string(),
                };
            }
            StoreError::Poisoned => {
                return Self::Poisoned;
            }
            StoreError::Invalid(code) => (*code).to_owned(),
            StoreError::Journal(diagnostic) => diagnostic.code.clone(),
            StoreError::Locked => "session_locked".into(),
            StoreError::StaleHead => "stale_head".into(),
            StoreError::StaleCommand => "stale_command".into(),
            StoreError::StaleRepair => "stale_repair".into(),
            StoreError::NotRepairable => "tail_not_repairable".into(),
            StoreError::UnsafePath => "unsafe_storage_object".into(),
            StoreError::Artifact => "artifact_missing_or_invalid".into(),
            StoreError::Limit => "storage_limit_exceeded".into(),
        };
        Self::Rejected { code }
    }
}

/// Side effects the driver performs. The core does nothing else.
#[derive(Clone, Debug)]
pub enum Effect {
    /// Verify media before building the opening commit.
    VerifyArtifacts {
        /// Token echoed by `ArtifactsVerified` (or `PersistFailed`).
        token: CommitToken,
        /// References to check through the worker.
        refs: Vec<ArtifactRef>,
    },

    /// Append one transaction, then answer `Persisted` or `PersistFailed`.
    Persist {
        /// Token for the answer.
        token: CommitToken,
        /// The transaction. Its command fields are bound by the worker.
        commit: Commit,
        /// Command to bind with `JournalStore::bind_command` before appending.
        command: Option<(CommandId, serde_json::Value)>,
        /// Record timestamp, from `Env::now`.
        at: Timestamp,
    },
    /// Begin a provider stream.
    StartInference {
        /// Stream identity for its events.
        inference: InferenceId,
        /// The active turn.
        turn_id: TurnId,
        /// Head whose ancestry was projected.
        head: EntryId,
        /// The projection built in `Preparing`, for the model at that head.
        input: ModelInput,
        /// The projection's fingerprint.
        fingerprint: Digest,
        /// Identity the assistant entry will carry.
        response_id: ResponseId,
    },
    /// Ask a stream to stop by cancelling its token. Keep delivering its events
    /// and its end: the core waits for a confirmation, ignoring other events,
    /// until `ReleaseInference`.
    CancelInference {
        /// The stream.
        inference: InferenceId,
    },
    /// Stop listening and drop the stream, which also cancels it. Exactly once
    /// per `StartInference`, when the core stops listening for any reason.
    ReleaseInference {
        /// The stream.
        inference: InferenceId,
    },
    /// Start a driver timer.
    SetTimer {
        /// Identity for `TimerFired`.
        timer: TimerId,
        /// Delay.
        duration: Duration,
    },
    /// Stop a driver timer; a late firing is ignored.
    CancelTimer {
        /// The timer.
        timer: TimerId,
    },
    /// Broadcast an event to subscribers.
    Emit(RuntimeEvent),
    /// Answer a command. Exactly one per `Input::Command`.
    Reply {
        /// The command's correlation.
        request: RequestId,
        /// Its outcome.
        result: Result<Accepted, Rejection>,
    },
}

/// A durably accepted command.
#[derive(Clone, Debug, PartialEq)]
pub enum Accepted {
    /// The opening commit is durable; a deduplicated retry returns the original.
    TurnStarted {
        /// Identity of the turn, distinct from the command ID.
        turn_id: TurnId,
        /// Acknowledgement of the opening commit.
        ack: CommitAck,
    },
    /// The turn is durably cancelling or finished as cancelled.
    Cancelling {
        /// The cancelled turn.
        turn_id: TurnId,
        /// The commit that made it so.
        ack: CommitAck,
    },
    /// The navigation commit is durable.
    HeadSelected {
        /// Its acknowledgement.
        ack: CommitAck,
    },
    /// The `model_changed` entry is durable.
    ModelSwitched {
        /// Its acknowledgement.
        ack: CommitAck,
    },
}

/// Why a command was refused. Refused commands change nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// The bounded command channel is full; submission never waits for room.
    Overloaded,
    /// A turn is active or a commit is in flight. Commands are never queued.
    Busy,
    /// The session needs recovery, which this runtime does not perform yet.
    RecoveryRequired,
    /// A persistence failure stopped this runtime; reopen the store.
    Faulted,
    /// The driver is draining for shutdown; the command never reached the core.
    ShuttingDown,
    /// The named turn is not the active turn.
    NotActive,
    /// The command ID was accepted earlier for a different command.
    StaleCommand,
    /// The command itself is invalid, with a stable code.
    Invalid(&'static str),
}

/// One broadcast event. `seq` starts at 1 per runtime and has no gaps.
#[derive(Clone, Debug)]
pub struct RuntimeEvent {
    /// Runtime incarnation; compare before sequence numbers.
    pub epoch: RuntimeEpoch,
    /// Position in this runtime's event stream, unrelated to journal sequence.
    pub seq: u64,
    /// Payload.
    pub kind: EventKind,
}

/// Event payloads. Durable facts follow their commit's acknowledgement; stream
/// deltas are live observations that are never persisted individually.
#[derive(Clone, Debug)]
pub enum EventKind {
    /// A commit is durable. One per `Persisted`.
    Committed {
        /// Storage's acknowledgement.
        ack: CommitAck,
        /// The newly published snapshot.
        snapshot: SessionSnapshot,
    },
    /// The phase or active turn changed.
    PhaseChanged {
        /// New phase.
        phase: Phase,
        /// Durable active turn.
        active_turn: Option<TurnId>,
    },
    /// A turn's terminal outcome is durable. Exactly once per turn.
    TurnFinished {
        /// The turn.
        turn_id: TurnId,
        /// Its outcome.
        outcome: TurnOutcome,
        /// Its stable code, if any.
        code: Option<String>,
    },
    /// The turn's context was built in `Preparing`. The full report is
    /// available through `SessionHandle::inspect_context`.
    ContextBuilt {
        /// The active turn.
        turn_id: TurnId,
        /// Head whose branch was projected.
        head: EntryId,
        /// The projection's fingerprint.
        fingerprint: Digest,
        /// Estimated tokens by category, and opaque bytes.
        totals: Totals,
        /// Overflow fails the turn with `context_overflow` before any inference.
        flags: Flags,
    },
    /// A provider stream started; a partial response now exists.
    InferenceStarted {
        /// Stream identity.
        inference: InferenceId,
        /// The active turn.
        turn_id: TurnId,
        /// Identity the assistant entry will carry.
        response_id: ResponseId,
        /// Model in use.
        model: ModelRef,
    },
    /// Live text.
    TextDelta {
        /// Stream identity.
        inference: InferenceId,
        /// Response-local block.
        block_index: u32,
        /// Appended text.
        text: String,
    },
    /// Live reasoning summary text.
    ReasoningSummaryDelta {
        /// Stream identity.
        inference: InferenceId,
        /// Response-local block.
        block_index: u32,
        /// Appended text.
        text: String,
    },
    /// A tool-call block opened.
    ToolCallStarted {
        /// Stream identity.
        inference: InferenceId,
        /// Response-local block.
        block_index: u32,
        /// Engine-issued call identity.
        call_id: CallId,
        /// Tool name.
        name: String,
        /// Native call identity.
        provider_call_id: Option<String>,
    },
    /// A tool argument fragment.
    ToolArgumentsDelta {
        /// Stream identity.
        inference: InferenceId,
        /// Response-local block.
        block_index: u32,
        /// Appended fragment.
        fragment: String,
    },
    /// A call's arguments are a complete JSON object. Not executable by itself.
    ToolCallReady {
        /// Stream identity.
        inference: InferenceId,
        /// Response-local block.
        block_index: u32,
    },
    /// Cumulative usage replaced.
    UsageUpdated {
        /// Stream identity.
        inference: InferenceId,
        /// Usage snapshot.
        usage: Usage,
    },
    /// The partial response is gone: durable, or abandoned by a fault.
    PartialCleared {
        /// Stream identity.
        inference: InferenceId,
    },
    /// Non-fatal observation.
    Diagnostic(RuntimeDiagnostic),
    /// The runtime stopped. Every later command is rejected with `Faulted`.
    Fault(FaultCause),
}

/// Non-fatal runtime observations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeDiagnostic {
    /// A provider event for a stream that is not current was dropped.
    StaleProviderEvent {
        /// The stream it named.
        inference: InferenceId,
        /// Total dropped by this runtime so far.
        dropped: u64,
    },
    /// A timer that is no longer armed fired.
    StaleTimer {
        /// The timer.
        timer: TimerId,
    },
    /// The current stream broke the provider protocol; its response will be
    /// recorded as failed with `malformed_stream`.
    MalformedStream {
        /// The stream.
        inference: InferenceId,
        /// Stable reason.
        reason: &'static str,
    },
}

/// Why `SessionHandle::inspect_context` returned no report.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InspectError {
    /// The builder refused, for example for an unknown head.
    #[error(transparent)]
    Context(#[from] ContextError),
    /// The driver has exited.
    #[error("session_closed")]
    Closed,
}

/// Why the runtime faulted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FaultCause {
    /// Storage did not persist the commit in flight.
    Persist(PersistError),
    /// An answer did not match the commit in flight.
    UnexpectedAnswer(CommitToken),
    /// An internal invariant failed; the core stops rather than guess.
    Internal(&'static str),
}
