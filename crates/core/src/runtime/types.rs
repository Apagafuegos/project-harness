//! Typed inputs, effects and events of the pure runtime core.

use std::{fmt, time::Duration};

use crate::context::compaction::{
    CompactionError, CompactionInput, CompactionPreview, NotEligible,
};
use crate::context::{ContextConfig, ContextError, Flags, PreviewReport, Totals};
use crate::model::ModelInput;
use crate::session::{
    ArtifactRef, CallId, CallRef, CommandId, Commit, CommitAck, ContentBlock, Digest, EntryId,
    Inspection, InvocationId, ModelRef, ResponseId, SessionSnapshot, StoreError, Timestamp,
    ToolStatus, TurnId, TurnOutcome, Usage,
};

pub use crate::model::{ProviderEvent, ProviderOutcome};
pub use crate::tools::RecoveryHint;

use super::view::Phase;
use crate::session::{ApprovalId, Decision, ResultCode};
use crate::tools::{AuthorizedOperation, ExecutionOutcome, Plan, Refusal, ValidatedOperation};

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
    /// Identifies a planning effect; stale replies are ignored.
    PlanToken,
    /// Identifies an execution effect; stale replies are ignored.
    ExecutionToken,
    /// Identifies an advisory inspection; stale replies are ignored.
    HintToken,
    /// Identifies one compaction generation; driver answers are epoch scoped.
    CompactionToken,
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
    /// Maximum elapsed time per inference, including setup; no deadline by default.
    pub inference_deadline: Option<Duration>,
    /// Model limits, budget deductions and estimator of the context builder.
    /// The default catalog is empty, and a turn whose model has no context
    /// limit fails with `context_budget_unknown`.
    pub context: ContextConfig,
    /// Absolute elapsed limit for summary generation, independently cancellable.
    pub compaction_deadline: Duration,
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
            inference_deadline: None,
            context: ContextConfig::default(),
            compaction_deadline: Duration::from_secs(300),
        }
    }
}

/// Everything the core reacts to. One input yields one batch of effects.
#[derive(Clone, Debug)]
pub enum Input {
    /// Driver-only summary generation completed; stale tokens are ignored.
    CompactionProposed {
        /// Fresh local generation identity.
        token: CompactionToken,
        /// Complete preview or coded failure, never partial data.
        result: Result<Box<CompactionPreview>, CompactionError>,
    },
    /// Announce the current phase and epoch after attaching a replacement runtime.
    PublishView,
    /// The driver attached this runtime to an opened, created, reopened or
    /// repaired journal. If the snapshot needs recovery, plan it: close an
    /// interrupted turn, or record uncertainty and wait for the user. A
    /// runtime with nothing to recover emits nothing.
    Start,
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
    /// Planning completed against the driver's trusted workspace view.
    Planned {
        /// Fresh identity of the planning effect.
        token: PlanToken,
        /// Immutable plan, or a nonexecuted refusal.
        result: Result<Plan, Refusal>,
    },
    /// The executor finished and cleaned up, including after cancellation.
    ToolFinished {
        /// Fresh identity of the execution effect.
        token: ExecutionToken,
        /// The executor's observation, not a promise of rollback.
        outcome: ExecutionOutcome,
    },
    /// Advisory inspection completed; never persisted.
    Hints {
        /// Fresh inspection identity.
        token: HintToken,
        /// Current-file evidence, not execution outcomes.
        hints: Vec<(InvocationId, RecoveryHint)>,
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
    /// Generate an ephemeral summary preview while idle.
    PreviewCompaction,
    /// Commit an unused, still-bound preview exactly as proposed.
    CommitCompaction {
        /// Ephemeral identity returned by preview generation.
        preview_id: EntryId,
    },
    /// Cancel the outstanding generation without publishing its result.
    CancelCompaction,
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
    /// Answer the one durable pending approval. Single-use and digest-bound.
    SubmitApproval {
        /// Pending ticket identity.
        approval_id: ApprovalId,
        /// Digest displayed in the approval request.
        plan_digest: Digest,
        /// Allow or deny this call.
        decision: Decision,
        /// Grant the narrowest session allowance for write, edit, or bash.
        remember: bool,
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
        /// Explicit omission policy; callers should default to `Reject`.
        policy: crate::session::Portability,
    },
    /// State what happened to an invocation whose outcome is unknown. Only
    /// accepted while the runtime awaits resolution.
    ResolveUncertain {
        /// The invocation listed by `ResolutionRequired`.
        invocation_id: InvocationId,
        /// The user's statement.
        resolution: UserResolution,
    },
}

impl Command {
    /// Switch with the safe default: refuse any portability loss.
    pub fn switch_model(model: ModelRef) -> Self {
        Self::SwitchModel {
            model,
            policy: crate::session::Portability::Reject,
        }
    }
}

/// The user's statement about an uncertain invocation. It becomes the call's
/// tool result; the tool is never run again.
#[derive(Clone, Debug, PartialEq)]
pub enum UserResolution {
    /// The user checked what happened. Recorded with `user_observed`
    /// provenance and this status, which cannot be `uncertain`.
    Observed {
        /// What the user found.
        status: ToolStatus,
        /// The user's account, bounded by `max_user_bytes`; not empty.
        text: String,
    },
    /// The user accepts not knowing. Recorded with status `uncertain` and
    /// `user_acknowledged_unknown` provenance.
    AcknowledgedUnknown,
}

/// An invocation that may or may not have run, as a client shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct UncertainInvocation {
    /// Invocation identity, named by `ResolveUncertain`.
    pub invocation_id: InvocationId,
    /// The turn that authorized it.
    pub turn_id: TurnId,
    /// The proposed call.
    pub call: CallRef,
    /// Tool name from the proposal.
    pub tool: String,
    /// Digest of the authorized plan.
    pub plan_digest: Digest,
    /// Record time of the execution intent.
    pub intent_at: Timestamp,
    /// Ephemeral current-file evidence, never a journal fact.
    pub hint: Option<RecoveryHint>,
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
    /// The journal ends in a torn tail. Nothing was repaired; the inspection
    /// says what `repair_tail` would quarantine.
    #[error("torn journal tail: {0}")]
    TornTail(TornTail),
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

/// The inspection of a journal whose last record was never fully written:
/// valid bytes, file bytes, fingerprint and the valid prefix's snapshot. Pass
/// its fingerprint and file size to `repair_tail` once the user confirms.
#[derive(Clone, Debug)]
pub struct TornTail(pub Box<Inspection>);

impl TornTail {
    /// The full inspection.
    pub fn inspection(&self) -> &Inspection {
        &self.0
    }
    /// Bytes that repair would quarantine.
    pub fn tail_bytes(&self) -> u64 {
        self.0.file_bytes - self.0.valid_bytes
    }
}

/// Inspections compare by their numbers and the valid prefix's identity.
impl PartialEq for TornTail {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (&self.0, &other.0);
        let prefix = |i: &Inspection| {
            i.snapshot
                .as_ref()
                .map(|s| (s.session_id().clone(), s.sequence(), s.checksum().clone()))
        };
        a.valid_bytes == b.valid_bytes
            && a.file_bytes == b.file_bytes
            && a.fingerprint == b.fingerprint
            && a.diagnostic == b.diagnostic
            && prefix(a) == prefix(b)
    }
}
impl Eq for TornTail {}

impl fmt::Display for TornTail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} of {} bytes are valid; fingerprint {}",
            self.0.valid_bytes, self.0.file_bytes, self.0.fingerprint
        )
    }
}

/// Why `repair_tail` refused or failed. Nothing is repaired automatically.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RepairError {
    /// The journal's fingerprint or size differs from the inspected values:
    /// the file changed since inspection. Inspect again.
    #[error("stale_repair")]
    StaleRepair,
    /// No torn tail to repair: a complete corrupt record, a missing header or
    /// a valid journal. Nothing is skipped or rewritten.
    #[error("tail_not_repairable")]
    NotRepairable,
    /// Another engine holds the session.
    #[error("session_locked")]
    Locked,
    /// The handle is not faulted, so its journal is open and has no torn tail.
    #[error("not_faulted")]
    NotFaulted,
    /// Repair succeeded or was not needed, but opening or I/O failed.
    #[error(transparent)]
    Persist(#[from] PersistError),
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
    /// Generate through RollingSummary in the driver, never in the core.
    ProposeCompaction {
        /// Fresh generation identity.
        token: CompactionToken,
        /// Immutable source and read-only artifact capability.
        input: CompactionInput,
    },
    /// Cancel and drop an outstanding summary generation.
    CancelCompaction {
        /// Generation to dispose of.
        token: CompactionToken,
    },
    /// Inspect authorized plan artifacts without delaying recovery commits.
    InspectUncertain {
        /// Fresh advisory job identity.
        token: HintToken,
        /// Invocation and authorized plan artifact.
        invocations: Vec<(InvocationId, ArtifactRef)>,
    },
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
        /// Publish and sync these bytes before appending, in the same worker job.
        artifacts: Vec<Vec<u8>>,
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
    /// Read workspace facts and build a plan using the current policy.
    PlanCall {
        /// Token echoed in `Input::Planned`.
        token: PlanToken,
        /// Durable proposal identity.
        call: CallRef,
        /// Resolved built-in name.
        tool: String,
        /// Opaque validated and defaulted arguments.
        operation: ValidatedOperation,
    },
    /// Run only after the execution intent has been acknowledged.
    Execute {
        /// Token echoed in `Input::ToolFinished`.
        token: ExecutionToken,
        /// Opaque authority constructed only by the core.
        operation: AuthorizedOperation,
    },
    /// Request cancellation, then wait for `ToolFinished` before finishing.
    CancelExecution {
        /// The currently executing effect.
        token: ExecutionToken,
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
    /// The preview is memory-only and grants no write authority.
    CompactionPreviewed {
        /// Single-use runtime-local identity (fresh across restarts).
        preview_id: EntryId,
        /// Before/after reports and inspection diff.
        preview: Box<CompactionPreview>,
    },
    /// The exact proposed summary entry is durable.
    CompactionCommitted {
        /// Consumed preview identity.
        preview_id: EntryId,
        /// Storage acknowledgement.
        ack: CommitAck,
    },
    /// The running generation was cancelled; no preview will result.
    CompactionCancelled,
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
    /// The user's resolution and `recovery_resolved` are durable.
    Resolved {
        /// The resolved invocation.
        invocation_id: InvocationId,
        /// The commit holding them.
        ack: CommitAck,
    },
    /// The approval decision and its consequences are durable.
    ApprovalSubmitted {
        /// The consumed ticket.
        approval_id: ApprovalId,
        /// Commit holding the decision.
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
        /// Classified target preview, with metadata only for every affected part.
        preview: Box<PreviewReport>,
    },
}

/// Why a command was refused. Refused commands change nothing.
#[derive(Clone, Debug, PartialEq)]
pub enum Rejection {
    /// `stale_preview`: no write occurred; names the differing binding.
    StalePreview {
        /// head, journal_sequence, session_model, source_fingerprint,
        /// policy_fingerprint, or preview_id (unknown/consumed).
        field: &'static str,
    },
    /// Compaction refused or failed with its closed code/reason.
    Compaction(CompactionError),
    /// `context_budget_exceeded`: compaction cannot make the next request fit.
    ContextBudgetExceeded {
        /// Eligibility reason, when no span can help.
        reason: Option<NotEligible>,
    },
    /// The bounded command channel is full; submission never waits for room.
    Overloaded,
    /// A turn is active or a commit is in flight. Commands are never queued.
    Busy,
    /// Automatic recovery is planned or in flight. Retry when it ends.
    RecoveryRequired,
    /// An uncertain invocation waits for the user; send `ResolveUncertain`.
    ResolutionRequired,
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
    /// `portability_loss`: the target cannot accept the projection under this policy.
    PortabilityLoss(Box<PreviewReport>),
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
    /// An ephemeral preview, including automatic preparation.
    CompactionPreviewed {
        /// Fresh single-use identity.
        preview_id: EntryId,
        /// Whether the idle trigger requested it.
        automatic: bool,
        /// Reports and exact changes.
        preview: Box<CompactionPreview>,
    },
    /// Compaction became durable, with the original inspection reports.
    CompactionCommitted {
        /// Consumed preview identity.
        preview_id: EntryId,
        /// Original preview and diff.
        preview: Box<CompactionPreview>,
    },
    /// One trigger attempt failed; no automatic retry is scheduled.
    CompactionFailed {
        /// Stable generation or budget failure code.
        code: &'static str,
    },
    /// A model switch is durable; never contains opaque payload bytes.
    ModelSwitched {
        /// Classified target preview.
        preview: Box<PreviewReport>,
    },
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
    /// Native block metadata only. Payload bytes remain private to the core.
    ProviderState {
        /// Stream identity.
        inference: InferenceId,
        /// Fresh response-local block index.
        block_index: u32,
        /// Provider schema, not display text.
        schema: String,
        /// Opaque payload length, not payload bytes.
        bytes: usize,
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
    /// Recovery closed a turn that a restart interrupted. Nothing was run,
    /// retried or invented: calls that never started were answered as
    /// cancelled and the turn finished as interrupted.
    Recovered {
        /// The closed turn.
        turn_id: TurnId,
        /// Calls answered as cancelled because they never started.
        answered: Vec<CallRef>,
        /// Why, such as `process_restart`.
        code: String,
    },
    /// Advisory evidence, never persisted; resolved invocations are ignored.
    RecoveryHints {
        /// Current-file evidence keyed by invocation.
        hints: Vec<(InvocationId, RecoveryHint)>,
    },
    /// Invocations whose outcome is unknown wait for `ResolveUncertain`. The
    /// list replaces the view's; it is the whole set still waiting.
    ResolutionRequired {
        /// Each unresolved invocation.
        invocations: Vec<UncertainInvocation>,
    },
    /// A durable ticket; exposed in resync views until resolved.
    ApprovalRequested {
        /// Single-use ticket identity.
        approval_id: ApprovalId,
        /// Proposal identity.
        call: CallRef,
        /// Digest of exactly `plan`.
        plan_digest: Digest,
        /// Canonical plan bytes displayed to the client.
        plan: Vec<u8>,
        /// Five-minute deadline.
        expires_at: Timestamp,
    },
    /// The resolution is durable. This clears the pending ticket.
    ApprovalResolved {
        /// Consumed or revoked ticket.
        approval_id: ApprovalId,
        /// Allowed, denied, expired, invalidated, or cancelled.
        outcome: ApprovalOutcome,
    },
    /// The intent is durable; dispatch follows this event.
    ToolStarted {
        /// Proposal identity.
        call: CallRef,
        /// Invocation from the durable intent.
        invocation_id: InvocationId,
    },
    /// The result is durable, including a nonexecuted refusal.
    ToolFinished {
        /// Proposal identity.
        call: CallRef,
        /// Recorded result status.
        status: ToolStatus,
        /// Machine-readable result code, absent exactly for success.
        code: Option<ResultCode>,
    },
    /// Non-fatal observation.
    Diagnostic(RuntimeDiagnostic),
    /// The runtime stopped. Every later command is rejected with `Faulted`.
    Fault(FaultCause),
}

/// Durable approval displayed by a client, including after resync.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingApproval {
    /// Single-use identity.
    pub approval_id: ApprovalId,
    /// Proposal identity.
    pub call: CallRef,
    /// Digest of exactly `plan`.
    pub plan_digest: Digest,
    /// Canonical approved plan bytes.
    pub plan: Vec<u8>,
    /// Five minutes after request creation.
    pub expires_at: Timestamp,
}

/// Durable outcome of an approval lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// User approved; dispatch follows intent acknowledgement.
    Allowed,
    /// User refused.
    Denied,
    /// Five-minute deadline elapsed.
    Expired,
    /// Session policy changed.
    Invalidated,
    /// Enclosing turn was cancelled.
    Cancelled,
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
