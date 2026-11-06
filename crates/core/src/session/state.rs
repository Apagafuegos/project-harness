//! Deterministic, staged semantic replay. No executor or provider is called here.

use super::{error::Result, *};
use imbl::{OrdMap, OrdSet, Vector};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// An immutable tree entry and its exact journal provenance.
#[derive(Clone, Debug)]
pub struct StoredEntry {
    /// Validated immutable entry.
    pub entry: Entry,
    /// Journal sequence containing the entry.
    pub sequence: u64,
    /// Exact-byte checksum of its containing record.
    pub record_checksum: Digest,
    /// Index within that commit's entry array.
    pub entry_index: usize,
}

/// A durable command receipt, reconstructed without an in-memory reply cache.
#[derive(Clone, Debug)]
pub struct CommandReceipt {
    /// Digest of the stored normalized submitted command.
    pub fingerprint: Digest,
    /// Original durable acceptance, including its original head.
    pub acknowledgement: CommitAck,
    /// Accepted entries/events identify associated turns, approvals and recovery.
    pub commit: Commit,
}

/// One known turn and its durable outcome.
#[derive(Clone, Debug)]
pub struct TurnInfo {
    /// Initial user entry.
    pub user_entry_id: EntryId,
    /// None while unfinished; terminal outcomes are retained across navigation.
    pub outcome: Option<TurnOutcome>,
}

/// A durable undo whose terminal result (or resolution) is not committed yet.
#[derive(Clone, Debug)]
pub struct UndoInProgress {
    /// The exact intent, including every target and the anchor head.
    pub intent: UndoIntent,
    /// Whether an interrupted undo has been recorded as uncertain.
    pub uncertain: bool,
    /// Journal sequence of the intent.
    pub sequence: u64,
}

/// An execution intent that has no durable result; opening never retries it.
#[derive(Clone, Debug)]
pub struct UnresolvedInvocation {
    /// Persisted intent and its binding. Its `effects` are `None` when it was
    /// recorded before v2: the effects are unknown, never assumed read-only.
    pub intent: ExecutionIntent,
    /// Sequence at which dispatch first became possible.
    pub sequence: u64,
    /// Record time of the intent.
    pub at: Timestamp,
    /// Whether an explicit uncertainty event has been recorded.
    pub uncertainty_recorded: bool,
}

/// A read-only point-in-time view sharing unchanged history across commits.
/// Cloning it is cheap and never exposes staged changes.
#[derive(Clone, Debug)]
pub struct SessionSnapshot(pub(crate) Arc<State>);
impl SessionSnapshot {
    /// Immutable session identity.
    pub fn session_id(&self) -> &SessionId {
        &self.0.session_id
    }
    /// Last accepted record sequence.
    pub fn sequence(&self) -> u64 {
        self.0.sequence
    }
    /// Last accepted exact-byte record checksum.
    pub fn checksum(&self) -> &Digest {
        &self.0.checksum
    }
    /// Durably selected head, independent of physical entry order.
    pub fn head(&self) -> Option<&EntryId> {
        self.0.head.as_ref()
    }
    /// Initial session configuration and workspace.
    pub fn header(&self) -> &SessionHeader {
        &self.0.header
    }
    /// Time the session header record was written, its creation time.
    pub fn created_at(&self) -> &Timestamp {
        &self.0.created_at
    }
    /// Current session-wide configuration, unaffected by navigation.
    pub fn configuration(&self) -> &ArtifactRef {
        &self.0.configuration
    }
    /// Current session-wide authorization policy revision.
    pub fn policy_revision(&self) -> &str {
        &self.0.policy_revision
    }
    /// Last committed operational state; opening does not synthesize audit events.
    pub fn state(&self) -> RuntimeState {
        self.0.runtime_state
    }
    /// Current unfinished turn, if any.
    pub fn active_turn(&self) -> Option<&TurnId> {
        self.0.active_turn.as_ref()
    }
    /// Look up an immutable entry on any branch.
    pub fn entry(&self, id: &EntryId) -> Option<&StoredEntry> {
        self.entry_arc(id).map(Arc::as_ref)
    }
    pub(crate) fn entry_arc(&self, id: &EntryId) -> Option<&Arc<StoredEntry>> {
        self.0.entries.get(id)
    }
    /// All branches, in stable identity order. Use ancestry for conversation order.
    pub fn entries(
        &self,
    ) -> impl ExactSizeIterator<Item = (&EntryId, &StoredEntry)> + DoubleEndedIterator {
        self.0
            .entries
            .iter()
            .map(|(id, entry)| (id, entry.as_ref()))
    }
    /// Look up a known turn, including one on a sibling branch.
    pub fn turn(&self, id: &TurnId) -> Option<&TurnInfo> {
        self.0.turns.get(id)
    }
    /// Known turn outcomes in stable identity order, including sibling branches.
    pub fn turns(
        &self,
    ) -> impl ExactSizeIterator<Item = (&TurnId, &TurnInfo)> + DoubleEndedIterator {
        self.0.turns.iter()
    }
    /// Globally ordered audit events `(record sequence, event index, event)`.
    pub fn events(
        &self,
    ) -> impl ExactSizeIterator<Item = &(u64, usize, JournalEvent)> + DoubleEndedIterator {
        self.0.events.iter().map(Arc::as_ref)
    }
    /// Persisted command acceptance, including after an acknowledgement was lost.
    pub fn command_receipt(&self, id: &CommandId) -> Option<&CommandReceipt> {
        self.0.commands.get(id).map(Arc::as_ref)
    }
    /// Return ancestors in conversation order, rejecting unknown heads.
    pub fn ancestry(&self, head: Option<&EntryId>) -> Result<Vec<&StoredEntry>> {
        self.0.ancestry(head)
    }
    /// Branch-derived model selection at the supplied head.
    pub fn model_at(&self, head: Option<&EntryId>) -> Result<&ModelRef> {
        self.0.model_at(head)
    }
    /// All intents lacking a committed result, across every branch.
    pub fn unresolved_invocations(&self) -> Vec<UnresolvedInvocation> {
        self.0
            .unresolved
            .iter()
            .map(|id| &self.0.invocations[id])
            .filter(|v| v.result.is_none())
            .map(|v| UnresolvedInvocation {
                intent: v.intent.clone(),
                sequence: v.sequence,
                at: v.at.clone(),
                uncertainty_recorded: v.uncertain,
            })
            .collect()
    }
    /// Whether any journal identity (session, record, entry, turn, response,
    /// call, invocation, approval, decision or command) already uses `id`.
    /// Storage rejects a commit that reuses one with `duplicate_id`.
    pub(crate) fn has_identity(&self, id: &str) -> bool {
        self.0.ids.contains(id)
    }
    /// A reopened engine must explicitly terminate unfinished work and resolve
    /// uncertain effects before accepting new work. No tool may be replayed.
    pub fn requires_recovery(&self) -> bool {
        self.0.active_turn.is_some() || self.0.has_unresolved()
    }
    /// The pending undo, if one has been recorded and not resolved.
    pub fn unresolved_undo(&self) -> Option<UndoInProgress> {
        self.0.undo.as_ref().map(|undo| UndoInProgress {
            intent: undo.intent.clone(),
            uncertain: undo.uncertain,
            sequence: undo.sequence,
        })
    }
    /// Which turn an undo on this head would select, and what it would restore.
    /// Read-only: nothing is recorded, and the selection is recomputed from
    /// durable facts every time.
    pub fn select_undo(&self) -> Result<UndoSelection> {
        self.0.undo_selection(self.0.head.as_ref())
    }
    /// Why a `context_control` for `turn` with `mode` at `head` would be
    /// invalid, as its stable code. Storage and the runtime share this check,
    /// so a command and a hand-written record never disagree.
    pub(crate) fn control_error(
        &self,
        head: Option<&EntryId>,
        turn: &TurnId,
        mode: ContextMode,
    ) -> std::result::Result<(), &'static str> {
        self.0.control_error(head, turn, mode)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Invocation {
    intent: ExecutionIntent,
    sequence: u64,
    at: Timestamp,
    uncertain: bool,
    result: Option<EntryId>,
    recovery_resolved: bool,
}

/// One durably recorded undo, from its intent to its terminal note.
#[derive(Clone, Debug)]
pub(crate) struct PendingUndo {
    pub(crate) intent: UndoIntent,
    pub(crate) sequence: u64,
    pub(crate) uncertain: bool,
}
impl Invocation {
    /// The captured pre-image binding, if this is a trusted write/edit.
    pub(crate) fn image(&self) -> Option<&FileImage> {
        self.intent.file_image.as_ref()
    }
    /// The turn that authorized it.
    pub(crate) fn turn(&self) -> &TurnId {
        &self.intent.turn_id
    }
    /// Whether a result for it is durably recorded.
    pub(crate) fn has_result(&self) -> bool {
        self.result.is_some()
    }
    /// Lacks a result, or is uncertain without an explicit recovery resolution.
    fn unresolved(&self) -> bool {
        self.result.is_none() || (self.uncertain && !self.recovery_resolved)
    }
}
/// Facts that depend only on an entry's ancestry. Entries are immutable, so
/// these are computed once from the parent's facts when the entry is inserted.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BranchFacts {
    /// Calls still awaiting results after this entry, in required order.
    pending: Arc<[CallRef]>,
    /// Model in effect at this entry.
    model: ModelRef,
    /// Effective non-default mode of each controlled turn after this entry.
    controls: OrdMap<TurnId, ContextMode>,
    /// Turns an ancestral undo note already consumed.
    consumed: Arc<BTreeSet<TurnId>>,
}
/// Internal ordered key for a call; the public `CallRef` is not `Ord`.
type CallKey = (EntryId, CallId);
fn key(call: &CallRef) -> CallKey {
    (call.response_entry_id.clone(), call.call_id.clone())
}
#[derive(Clone, Debug)]
struct Approval {
    request: ApprovalRequested,
    used: bool,
}
#[derive(Clone, Debug)]
struct Authorization {
    decision: AuthorizationDecided,
    used: bool,
}
#[derive(Clone, Debug)]
pub(crate) struct State {
    pub session_id: SessionId,
    pub header: SessionHeader,
    /// Time of the header record, which is the session's creation time.
    pub created_at: Timestamp,
    pub sequence: u64,
    pub checksum: Digest,
    /// Version of the last accepted record; versions never decrease.
    pub version: u32,
    pub head: Option<EntryId>,
    pub configuration: ArtifactRef,
    pub policy_revision: String,
    pub runtime_state: RuntimeState,
    pub active_turn: Option<TurnId>,
    pub entries: OrdMap<EntryId, Arc<StoredEntry>>,
    pub turns: OrdMap<TurnId, TurnInfo>,
    pub events: Vector<Arc<(u64, usize, JournalEvent)>>,
    pub commands: OrdMap<CommandId, Arc<CommandReceipt>>,
    pub invocations: OrdMap<InvocationId, Invocation>,
    /// The one undo that has an intent but no terminal result.
    pub(crate) undo: Option<PendingUndo>,
    ids: OrdSet<String>,
    approvals: OrdMap<ApprovalId, Approval>,
    decisions: OrdMap<DecisionId, Authorization>,
    inference_response: Option<EntryId>,
    // Derived state below is staged with the records it indexes and rebuilt by
    // replay, never persisted. It keeps a commit's validation cost independent
    // of history length.
    /// Branch facts of every entry in `entries`.
    pub(crate) branch: OrdMap<EntryId, Arc<BranchFacts>>,
    /// Branch facts at the null root.
    root: Arc<BranchFacts>,
    /// Invocations for which `Invocation::unresolved` holds.
    unresolved: OrdSet<InvocationId>,
    /// The only invocation of each call that has one.
    invocation_by_call: OrdMap<CallKey, InvocationId>,
    /// The only unused approval of each call that has one.
    unused_approvals: OrdMap<CallKey, ApprovalId>,
    /// Decisions not yet used by an intent or the end of their turn.
    unused_decisions: OrdSet<DecisionId>,
}

fn require(condition: bool, code: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(StoreError::Invalid(code))
    }
}
impl State {
    pub fn start(record: &Record, checksum: Digest) -> Result<Self> {
        let RecordPayload::SessionCreated(header) = &record.payload else {
            return Err(StoreError::Invalid("missing_header"));
        };
        require(
            record.seq == 0 && record.previous_checksum.is_none(),
            "invalid_header_sequence",
        )?;
        let mut state = Self {
            session_id: record.session_id.clone(),
            header: header.clone(),
            created_at: record.at.clone(),
            sequence: 0,
            checksum,
            version: record.version,
            head: None,
            configuration: header.configuration.clone(),
            policy_revision: header.policy_revision.clone(),
            runtime_state: RuntimeState::Idle,
            active_turn: None,
            entries: OrdMap::new(),
            turns: OrdMap::new(),
            events: Vector::new(),
            commands: OrdMap::new(),
            invocations: OrdMap::new(),
            undo: None,
            ids: OrdSet::new(),
            approvals: OrdMap::new(),
            decisions: OrdMap::new(),
            inference_response: None,
            branch: OrdMap::new(),
            root: Arc::new(BranchFacts {
                pending: Arc::from([]),
                model: header.initial_model.clone(),
                controls: OrdMap::new(),
                consumed: Arc::new(BTreeSet::new()),
            }),
            unresolved: OrdSet::new(),
            invocation_by_call: OrdMap::new(),
            unused_approvals: OrdMap::new(),
            unused_decisions: OrdSet::new(),
        };
        state.unique(record.session_id.as_str())?;
        state.unique(record.record_id.as_str())?;
        Ok(state)
    }
    fn unique(&mut self, id: &str) -> Result<()> {
        require(self.ids.insert(id.to_owned()).is_none(), "duplicate_id")
    }
    fn has_unresolved(&self) -> bool {
        !self.unresolved.is_empty() || self.undo.is_some()
    }
    /// Recompute one invocation's membership of `unresolved` from its record.
    /// Call after every change to that record.
    fn track(&mut self, id: &InvocationId) {
        if self.invocations.get(id).is_some_and(Invocation::unresolved) {
            self.unresolved.insert(id.clone());
        } else {
            self.unresolved.remove(id);
        }
    }
    fn idle(&self) -> Result<()> {
        require(
            self.active_turn.is_none()
                && self.runtime_state == RuntimeState::Idle
                && !self.has_unresolved(),
            "session_not_idle",
        )
    }
    fn active(&self, turn: &TurnId) -> Result<()> {
        require(self.active_turn.as_ref() == Some(turn), "wrong_active_turn")
    }
    fn ancestry(&self, head: Option<&EntryId>) -> Result<Vec<&StoredEntry>> {
        let mut next = head;
        let mut entries = Vec::new();
        while let Some(id) = next {
            let stored = self
                .entries
                .get(id)
                .ok_or(StoreError::Invalid("unknown_head_or_parent"))?;
            entries.push(stored.as_ref());
            next = stored.entry.parent_id.as_ref();
        }
        entries.reverse();
        Ok(entries)
    }
    fn facts(&self, head: Option<&EntryId>) -> Result<&Arc<BranchFacts>> {
        match head {
            None => Ok(&self.root),
            Some(id) => self
                .branch
                .get(id)
                .ok_or(StoreError::Invalid("unknown_head_or_parent")),
        }
    }
    fn model_at(&self, head: Option<&EntryId>) -> Result<&ModelRef> {
        Ok(&self.facts(head)?.model)
    }
    pub(crate) fn pending(&self, head: Option<&EntryId>) -> Result<&[CallRef]> {
        Ok(&self.facts(head)?.pending)
    }
    fn call(&self, call: &CallRef, sequence: u64, turn: &TurnId) -> Result<()> {
        let response = self
            .entries
            .get(&call.response_entry_id)
            .ok_or(StoreError::Invalid("unknown_response"))?;
        require(
            response.sequence < sequence && response.entry.turn_id.as_ref() == Some(turn),
            "proposal_not_prior_durable",
        )?;
        let EntryPayload::AssistantResponse(payload) = &response.entry.payload else {
            return Err(StoreError::Invalid("call_not_response"));
        };
        require(
            payload.outcome == ResponseOutcome::Completed
                && payload.finish_reason == Some(FinishReason::ToolCalls),
            "ineligible_proposal",
        )?;
        require(payload.content.iter().any(|block| matches!(block, ContentBlock::ToolCall {call_id, ..} if call_id == &call.call_id)), "unknown_call")?;
        Ok(())
    }
    fn next_call(&self, call: &CallRef, sequence: u64, turn: &TurnId) -> Result<()> {
        self.call(call, sequence, turn)?;
        require(
            self.pending(self.head.as_ref())?.first() == Some(call),
            "call_not_next_on_branch",
        )
    }

    pub fn apply(&mut self, record: &Record, checksum: Digest) -> Result<()> {
        self.apply_record(record, checksum)?;
        #[cfg(test)]
        if ORACLE.get() {
            self.check_derived(record);
        }
        Ok(())
    }
    fn apply_record(&mut self, record: &Record, checksum: Digest) -> Result<()> {
        require(
            record.seq == self.sequence + 1
                && record.session_id == self.session_id
                && record.previous_checksum.as_ref() == Some(&self.checksum),
            "broken_record_chain",
        )?;
        require(record.version >= self.version, "version_regression")?;
        self.unique(record.record_id.as_str())?;
        let RecordPayload::Commit(commit) = &record.payload else {
            return Err(StoreError::Invalid("repeated_header"));
        };
        if commit.head.from != self.head {
            return Err(StoreError::StaleHead);
        }
        require(
            !commit.entries.is_empty()
                || !commit.events.is_empty()
                || commit.head.from != commit.head.to,
            "empty_commit",
        )?;
        require(
            commit.command_id.is_some() == commit.command_fingerprint.is_some(),
            "command_binding",
        )?;
        if let Some(id) = &commit.command_id {
            self.unique(id.as_str())?;
        }
        match commit.head.reason {
            HeadReason::Advance => {
                require(!commit.entries.is_empty(), "advance_without_entries")?;
                let mut parent = commit.head.from.as_ref();
                for entry in &commit.entries {
                    require(entry.parent_id.as_ref() == parent, "nonlinear_advance")?;
                    parent = Some(&entry.id);
                }
                require(parent == commit.head.to.as_ref(), "invalid_advance_head")?;
            }
            HeadReason::Navigate => {
                self.idle()?;
                require(commit.entries.is_empty(), "navigation_has_entries")?;
                require(
                    self.pending(commit.head.to.as_ref())?.is_empty(),
                    "head_inside_tool_exchange",
                )?;
            }
            HeadReason::Retain => require(
                commit.entries.is_empty() && commit.head.to == self.head,
                "invalid_retained_head",
            )?,
        }
        // Undo records have exact commit shapes and a terminal pairing that
        // must be validated from the whole commit before any entry is inserted.
        self.undo_shape(commit, record.seq)?;
        let starts: Vec<_> = commit
            .events
            .iter()
            .filter_map(|e| {
                if let JournalEvent::TurnStarted(v) = e {
                    Some(v)
                } else {
                    None
                }
            })
            .collect();
        require(starts.len() <= 1, "multiple_turns")?;
        for start in &starts {
            self.idle()?;
            self.unique(start.turn_id.as_str())?;
            require(
                start.configuration == self.configuration,
                "turn_configuration_mismatch",
            )?;
            self.turns.insert(
                start.turn_id.clone(),
                TurnInfo {
                    user_entry_id: start.user_entry_id.clone(),
                    outcome: None,
                },
            );
        }
        for (index, entry) in commit.entries.iter().enumerate() {
            self.unique(entry.id.as_str())?;
            if let Some(parent) = &entry.parent_id {
                require(self.entries.contains_key(parent), "unknown_parent")?;
            }
            self.entry(entry, record, &starts)?;
            let facts = step(self.facts(entry.parent_id.as_ref())?, entry)?;
            self.branch.insert(entry.id.clone(), facts);
            self.entries.insert(
                entry.id.clone(),
                Arc::new(StoredEntry {
                    entry: entry.clone(),
                    sequence: record.seq,
                    record_checksum: checksum.clone(),
                    entry_index: index,
                }),
            );
        }
        self.head = commit.head.to.clone();
        // Entries and events are atomic, but event order remains semantically significant.
        for (index, event) in commit.events.iter().enumerate() {
            self.event(event, record, &commit.events[..index])?;
            self.events
                .push_back(Arc::new((record.seq, index, event.clone())));
        }
        // Every new entry's branch rules were checked by `step`; this guards
        // that the selected head exists.
        self.facts(self.head.as_ref())?;
        require(
            self.active_turn.is_none() || self.runtime_state != RuntimeState::Idle,
            "unfinished_idle_turn",
        )?;
        for entry in &commit.entries {
            if let EntryPayload::ToolResult(result) = &entry.payload
                && matches!(
                    result.result.metadata.provenance,
                    Provenance::UserObserved | Provenance::UserAcknowledgedUnknown
                )
            {
                require(commit.events.iter().any(|e| matches!(e, JournalEvent::RecoveryResolved(r) if r.result_entry_id == entry.id)), "recovery_result_without_resolution")?;
            }
        }
        self.sequence = record.seq;
        self.checksum = checksum.clone();
        self.version = record.version;
        if let (Some(id), Some(fingerprint)) = (&commit.command_id, &commit.command_fingerprint) {
            self.commands.insert(
                id.clone(),
                Arc::new(CommandReceipt {
                    fingerprint: fingerprint.clone(),
                    acknowledgement: CommitAck {
                        sequence: record.seq,
                        checksum,
                        head: self.head.clone(),
                    },
                    commit: commit.clone(),
                }),
            );
        }
        Ok(())
    }

    fn entry(&mut self, entry: &Entry, record: &Record, starts: &[&TurnStarted]) -> Result<()> {
        let sequence = record.seq;
        match &entry.payload {
            EntryPayload::UserMessage(message) => {
                require(
                    starts.iter().any(|s| {
                        Some(&s.turn_id) == entry.turn_id.as_ref() && s.user_entry_id == entry.id
                    }),
                    "user_without_turn_start",
                )?;
                require(
                    self.pending(entry.parent_id.as_ref())?.is_empty(),
                    "head_inside_tool_exchange",
                )?;
                valid_content(&message.content)?;
            }
            EntryPayload::AssistantResponse(response) => {
                let turn = entry
                    .turn_id
                    .as_ref()
                    .ok_or(StoreError::Invalid("response_without_turn"))?;
                self.active(turn)?;
                require(
                    matches!(
                        self.runtime_state,
                        RuntimeState::Inferring
                            | RuntimeState::Cancelling
                            | RuntimeState::Recovering
                    ),
                    "response_outside_inference",
                )?;
                require(
                    self.pending(entry.parent_id.as_ref())?.is_empty(),
                    "inference_inside_tool_exchange",
                )?;
                require(
                    response.model == *self.model_at(entry.parent_id.as_ref())?,
                    "response_model_mismatch",
                )?;
                require(
                    self.inference_response.is_none(),
                    "duplicate_inference_response",
                )?;
                self.unique(response.response_id.as_str())?;
                let mut calls = 0;
                for block in &response.content {
                    if let ContentBlock::ToolCall { call_id, .. } = block {
                        self.unique(call_id.as_str())?;
                        calls += 1;
                    }
                    if let ContentBlock::ProviderState { owner, .. } = block {
                        require(owner == &response.model, "provider_state_owner")?;
                    }
                }
                match response.outcome {
                    ResponseOutcome::Completed => {
                        require(
                            response.finish_reason.is_some() && response.error.is_none(),
                            "invalid_completed_response",
                        )?;
                        require(
                            (response.finish_reason == Some(FinishReason::ToolCalls))
                                == (calls > 0),
                            "inconsistent_tool_finish",
                        )?;
                    }
                    ResponseOutcome::Failed => require(
                        response.error.is_some()
                            && response.finish_reason.is_none()
                            && response.continuation.is_none(),
                        "invalid_failed_response",
                    )?,
                    ResponseOutcome::Cancelled | ResponseOutcome::Interrupted => require(
                        response.error.is_none()
                            && response.finish_reason.is_none()
                            && response.continuation.is_none(),
                        "invalid_partial_response",
                    )?,
                }
                if let Some(continuation) = &response.continuation {
                    require(continuation.owner == response.model, "continuation_owner")?;
                }
                self.inference_response = Some(entry.id.clone());
            }
            EntryPayload::ToolResult(result) => {
                let result = &result.result;
                let turn = entry
                    .turn_id
                    .as_ref()
                    .ok_or(StoreError::Invalid("result_without_turn"))?;
                self.call(&result.call, sequence, turn)?;
                require(
                    self.pending(entry.parent_id.as_ref())?.first() == Some(&result.call),
                    "orphan_duplicate_or_reordered_result",
                )?;
                valid_content(&result.content)?;
                if record.version >= 2 {
                    result_code(result)?;
                }
                let recovering = matches!(
                    result.metadata.provenance,
                    Provenance::UserObserved | Provenance::UserAcknowledgedUnknown
                );
                let restart_read = result.invocation_id.is_some()
                    && result.metadata.provenance == Provenance::Runtime;
                if !recovering && !restart_read {
                    self.active(turn)?;
                } else {
                    require(
                        self.turns
                            .get(turn)
                            .is_some_and(|t| t.outcome == Some(TurnOutcome::Interrupted))
                            || self.runtime_state == RuntimeState::Recovering,
                        "not_recovering",
                    )?;
                }
                if let Some(id) = &result.invocation_id {
                    let invocation = self
                        .invocations
                        .get_mut(id)
                        .ok_or(StoreError::Invalid("unknown_invocation"))?;
                    require(
                        invocation.sequence < sequence
                            && invocation.intent.call == result.call
                            && invocation.intent.turn_id == *turn
                            && invocation.result.is_none(),
                        "invalid_invocation_result",
                    )?;
                    require(
                        if restart_read {
                            true // The complete D9 exception is checked below.
                        } else if recovering {
                            invocation.uncertain
                        } else {
                            !invocation.uncertain
                        },
                        "uncertain_result_requires_recovery",
                    )?;
                    if record.version >= 4 {
                        let captured = invocation.intent.file_image.is_some();
                        require(
                            result.metadata.file_effect.is_some() == captured,
                            "undo_image_binding",
                        )?;
                        // Only a user's explicit statement classifies a
                        // captured mutation, and never as `unknown` while the
                        // user claims to have observed the outcome.
                        if result.metadata.provenance == Provenance::UserObserved && captured {
                            require(
                                matches!(
                                    result.metadata.file_effect,
                                    Some(FileEffect::Applied | FileEffect::NotApplied)
                                ),
                                "undo_image_binding",
                            )?;
                        }
                        if result.metadata.provenance == Provenance::UserAcknowledgedUnknown
                            && captured
                        {
                            require(
                                result.metadata.file_effect == Some(FileEffect::Unknown),
                                "undo_image_binding",
                            )?;
                        }
                        if record.version >= 5
                            && result.metadata.provenance == Provenance::Executor
                            && invocation
                                .image()
                                .is_some_and(|image| !image.additional_targets.is_empty())
                            && result.metadata.file_effect != Some(FileEffect::NotApplied)
                        {
                            require(!result.metadata.file_paths.is_empty(), "undo_image_binding")?;
                        }
                        if record.version < 5 {
                            require(result.metadata.file_paths.is_empty(), "undo_image_binding")?;
                        } else if !result.metadata.file_paths.is_empty() {
                            let image = invocation
                                .image()
                                .ok_or(StoreError::Invalid("undo_image_binding"))?;
                            let targets: Vec<_> = image.targets().collect();
                            require(
                                targets.len() == result.metadata.file_paths.len()
                                    && targets.iter().zip(&result.metadata.file_paths).all(
                                        |(target, effect)| target.path == effect.path.as_str(),
                                    )
                                    && result
                                        .metadata
                                        .file_paths
                                        .windows(2)
                                        .all(|pair| pair[0].path < pair[1].path),
                                "undo_image_binding",
                            )?;
                            let aggregate = if result
                                .metadata
                                .file_paths
                                .iter()
                                .any(|entry| entry.effect == FileEffect::Unknown)
                            {
                                FileEffect::Unknown
                            } else if result
                                .metadata
                                .file_paths
                                .iter()
                                .any(|entry| entry.effect == FileEffect::Applied)
                            {
                                FileEffect::Applied
                            } else {
                                FileEffect::NotApplied
                            };
                            require(
                                result.metadata.file_effect == Some(aggregate),
                                "undo_image_binding",
                            )?;
                        }
                    }
                    require(
                        if result.status == ToolStatus::Uncertain {
                            result.metadata.provenance == Provenance::UserAcknowledgedUnknown
                        } else {
                            result.metadata.provenance != Provenance::UserAcknowledgedUnknown
                        },
                        "invalid_unknown_result",
                    )?;
                    require(
                        recovering
                            || (restart_read
                                && !invocation.uncertain
                                && invocation.intent.effects.as_ref().is_some_and(|effects| {
                                    effects.as_slice() == [EffectClass::Read]
                                })
                                && result.status == ToolStatus::Cancelled
                                && result.metadata.code.as_deref()
                                    == Some(ResultCode::ProcessRestart.as_str()))
                            || (result.metadata.provenance == Provenance::Executor
                                && matches!(
                                    result.status,
                                    ToolStatus::Succeeded
                                        | ToolStatus::Failed
                                        | ToolStatus::TimedOut
                                        | ToolStatus::Cancelled
                                        | ToolStatus::InfrastructureFailure
                                )),
                        "invalid_executed_provenance",
                    )?;
                    invocation.result = Some(entry.id.clone());
                    self.track(id);
                } else {
                    if record.version >= 4 {
                        require(result.metadata.file_effect.is_none(), "undo_image_binding")?;
                    }
                    require(
                        result.metadata.provenance == Provenance::Runtime
                            && matches!(
                                result.status,
                                ToolStatus::InvalidArguments
                                    | ToolStatus::Denied
                                    | ToolStatus::Cancelled
                                    | ToolStatus::InfrastructureFailure
                                    | ToolStatus::TimedOut
                            ),
                        "invalid_nonexecuted_result",
                    )?;
                    require(
                        !self
                            .invocation_by_call
                            .get(&key(&result.call))
                            .is_some_and(|id| self.invocations[id].result.is_none()),
                        "result_omits_intent",
                    )?;
                }
            }
            EntryPayload::ModelChanged(_) => {
                self.idle()?;
                require(
                    entry.turn_id.is_none() && starts.is_empty(),
                    "model_change_inside_turn",
                )?;
            }
            EntryPayload::Compaction(compaction) => {
                self.idle()?;
                require(
                    entry.turn_id.is_none() && starts.is_empty(),
                    "compaction_inside_turn",
                )?;
                self.compaction(entry, compaction)?;
            }
            EntryPayload::ContextControl(control) => {
                self.idle()?;
                require(
                    entry.turn_id.is_none() && starts.is_empty(),
                    "context_control_inside_turn",
                )?;
                if let Err(code) =
                    self.control_error(entry.parent_id.as_ref(), &control.turn_id, control.mode)
                {
                    return Err(StoreError::Invalid(code));
                }
            }
            EntryPayload::UndoNote(note) => {
                // A note is never dialogue: it has no turn, and it is the only
                // entry of its commit, appended at the intent's anchor head.
                let pending = self
                    .undo
                    .as_ref()
                    .ok_or(StoreError::Invalid("undo_result_binding"))?;
                require(
                    entry.turn_id.is_none() && starts.is_empty(),
                    "undo_commit_shape",
                )?;
                require(
                    entry.parent_id.as_ref() == pending.intent.anchor_head.as_ref()
                        && pending.intent.undo_id == note.undo_id,
                    "undo_binding",
                )?;
                require(
                    self.pending(entry.parent_id.as_ref())?.is_empty(),
                    "head_inside_tool_exchange",
                )?;
            }
        }
        Ok(())
    }

    fn event(
        &mut self,
        event: &JournalEvent,
        record: &Record,
        prior: &[JournalEvent],
    ) -> Result<()> {
        match event {
            JournalEvent::TurnStarted(start) => {
                require(
                    self.active_turn.is_none()
                        && self.runtime_state == RuntimeState::Idle
                        && !self.has_unresolved(),
                    "concurrent_turn",
                )?;
                let user = self
                    .entries
                    .get(&start.user_entry_id)
                    .ok_or(StoreError::Invalid("missing_user_entry"))?;
                require(
                    user.sequence == record.seq
                        && user.entry.turn_id.as_ref() == Some(&start.turn_id)
                        && matches!(user.entry.payload, EntryPayload::UserMessage(_)),
                    "invalid_turn_start",
                )?;
                self.active_turn = Some(start.turn_id.clone());
            }
            JournalEvent::StateChanged(change) => {
                require(
                    self.active_turn.as_ref() == Some(&change.turn_id)
                        || (change.to == RuntimeState::Recovering
                            && self.has_unresolved()
                            && self.turns.contains_key(&change.turn_id)),
                    "state_without_turn",
                )?;
                require(
                    change.from == self.runtime_state && legal_transition(change.from, change.to),
                    "illegal_state_transition",
                )?;
                if change.to == RuntimeState::Inferring {
                    require(
                        !self.has_unresolved() && self.pending(self.head.as_ref())?.is_empty(),
                        "inference_before_results",
                    )?;
                    self.inference_response = None;
                }
                if change.to == RuntimeState::Processing && change.from == RuntimeState::Inferring {
                    require(
                        self.inference_response.as_ref() == self.head.as_ref(),
                        "processing_without_current_response",
                    )?;
                    require(
                        self.head
                            .as_ref()
                            .and_then(|id| self.entries.get(id))
                            .is_some_and(|v| {
                                matches!(v.entry.payload, EntryPayload::AssistantResponse(_))
                                    && v.entry.turn_id.as_ref() == Some(&change.turn_id)
                            }),
                        "processing_without_response",
                    )?;
                }
                if change.to == RuntimeState::Executing {
                    require(
                        self.unresolved.iter().any(|id| {
                            let v = &self.invocations[id];
                            v.result.is_none() && v.intent.turn_id == change.turn_id
                        }),
                        "executing_without_intent",
                    )?;
                }
                if change.from == RuntimeState::Executing && change.to == RuntimeState::Continuing {
                    require(!self.has_unresolved(), "continuing_without_result")?;
                }
                self.runtime_state = change.to;
            }
            JournalEvent::ApprovalRequested(request) => {
                self.active(&request.turn_id)?;
                require(
                    self.runtime_state == RuntimeState::Authorizing,
                    "approval_outside_authorizing",
                )?;
                self.next_call(&request.call, record.seq, &request.turn_id)?;
                self.unique(request.approval_id.as_str())?;
                require(
                    request.plan_digest == request.plan.sha256
                        && request.policy_revision == self.policy_revision
                        && request.expires_at.instant() > record.at.instant(),
                    "invalid_approval_binding",
                )?;
                require(
                    !self.unused_approvals.contains_key(&key(&request.call)),
                    "duplicate_pending_approval",
                )?;
                self.unused_approvals
                    .insert(key(&request.call), request.approval_id.clone());
                self.approvals.insert(
                    request.approval_id.clone(),
                    Approval {
                        request: request.clone(),
                        used: false,
                    },
                );
            }
            JournalEvent::AuthorizationDecided(decision) => {
                self.active(&decision.turn_id)?;
                require(
                    self.runtime_state == RuntimeState::Authorizing,
                    "authorization_outside_authorizing",
                )?;
                self.next_call(&decision.call, record.seq, &decision.turn_id)?;
                self.unique(decision.decision_id.as_str())?;
                require(
                    decision.plan_digest == decision.plan.sha256
                        && decision.policy_revision == self.policy_revision,
                    "authorization_binding",
                )?;
                if let Some(id) = &decision.approval_id {
                    let approval = self
                        .approvals
                        .get_mut(id)
                        .ok_or(StoreError::Invalid("unknown_approval"))?;
                    require(
                        !approval.used
                            && approval.request.call == decision.call
                            && approval.request.turn_id == decision.turn_id
                            && approval.request.plan_digest == decision.plan_digest
                            && approval.request.policy_revision == decision.policy_revision,
                        "approval_binding_mismatch",
                    )?;
                    if decision.decision == Decision::Allow {
                        require(
                            record.at.instant() < approval.request.expires_at.instant(),
                            "expired_approval",
                        )?;
                    }
                    approval.used = true;
                    self.unused_approvals.remove(&key(&approval.request.call));
                }
                self.unused_decisions.insert(decision.decision_id.clone());
                self.decisions.insert(
                    decision.decision_id.clone(),
                    Authorization {
                        decision: decision.clone(),
                        used: false,
                    },
                );
            }
            JournalEvent::ExecutionIntent(intent) => {
                // The v2 schema requires effects; a v1 intent has none.
                if let Some(effects) = &intent.effects {
                    valid_effects(intent.backend, effects)?;
                }
                // v4 binds a pre-image capture exactly for trusted write/edit
                // intents, and for nothing else. Earlier versions have none.
                if record.version >= 4 {
                    let mutation = intent.backend == ExecutionBackend::TrustedFs
                        && intent.effects.as_ref().is_some_and(|effects| {
                            effects.contains(EffectClass::FilesystemMutation)
                        });
                    require(
                        intent.file_image.is_some() == mutation,
                        "undo_image_binding",
                    )?;
                }
                self.active(&intent.turn_id)?;
                require(
                    self.runtime_state == RuntimeState::Authorizing && !self.has_unresolved(),
                    "overlapping_execution",
                )?;
                self.next_call(&intent.call, record.seq, &intent.turn_id)?;
                self.unique(intent.invocation_id.as_str())?;
                require(
                    !self.invocation_by_call.contains_key(&key(&intent.call)),
                    "duplicate_call_invocation",
                )?;
                let authorization = self
                    .decisions
                    .get_mut(&intent.decision_id)
                    .ok_or(StoreError::Invalid("unknown_authorization"))?;
                let decision = &authorization.decision;
                require(
                    !authorization.used
                        && decision.decision == Decision::Allow
                        && decision.call == intent.call
                        && decision.turn_id == intent.turn_id
                        && decision.plan_digest == intent.plan_digest
                        && decision.policy_revision == self.policy_revision,
                    "intent_authorization_mismatch",
                )?;
                if let Some(id) = &decision.approval_id {
                    require(
                        record.at.instant() < self.approvals[id].request.expires_at.instant(),
                        "expired_approval",
                    )?;
                }
                authorization.used = true;
                self.unused_decisions.remove(&intent.decision_id);
                self.invocation_by_call
                    .insert(key(&intent.call), intent.invocation_id.clone());
                self.invocations.insert(
                    intent.invocation_id.clone(),
                    Invocation {
                        intent: intent.clone(),
                        sequence: record.seq,
                        at: record.at.clone(),
                        uncertain: false,
                        result: None,
                        recovery_resolved: false,
                    },
                );
                self.track(&intent.invocation_id);
            }
            JournalEvent::ExecutionUncertain(uncertain) => {
                let invocation = self
                    .invocations
                    .get_mut(&uncertain.invocation_id)
                    .ok_or(StoreError::Invalid("unknown_invocation"))?;
                require(
                    invocation.intent.turn_id == uncertain.turn_id
                        && invocation.result.is_none()
                        && !invocation.uncertain,
                    "invalid_uncertainty",
                )?;
                invocation.uncertain = true;
                self.track(&uncertain.invocation_id);
                self.runtime_state = RuntimeState::Recovering;
            }
            JournalEvent::RecoveryResolved(resolution) => {
                require(
                    self.runtime_state == RuntimeState::Recovering,
                    "resolution_outside_recovery",
                )?;
                let invocation = self
                    .invocations
                    .get_mut(&resolution.invocation_id)
                    .ok_or(StoreError::Invalid("unknown_invocation"))?;
                require(
                    invocation.intent.turn_id == resolution.turn_id
                        && invocation.uncertain
                        && !invocation.recovery_resolved
                        && invocation.result.as_ref() == Some(&resolution.result_entry_id),
                    "invalid_recovery_result",
                )?;
                let stored = self
                    .entries
                    .get(&resolution.result_entry_id)
                    .ok_or(StoreError::Invalid("missing_recovery_result"))?;
                let EntryPayload::ToolResult(result) = &stored.entry.payload else {
                    return Err(StoreError::Invalid("recovery_result_not_tool"));
                };
                require(
                    stored.sequence == record.seq
                        && match resolution.resolution {
                            Resolution::Observed => {
                                result.result.metadata.provenance == Provenance::UserObserved
                                    && result.result.status != ToolStatus::Uncertain
                            }
                            Resolution::AcknowledgedUnknown => {
                                result.result.metadata.provenance
                                    == Provenance::UserAcknowledgedUnknown
                                    && result.result.status == ToolStatus::Uncertain
                            }
                        },
                    "recovery_resolution_provenance",
                )?;
                invocation.recovery_resolved = true;
                self.track(&resolution.invocation_id);
                if !self.has_unresolved() && self.active_turn.is_none() {
                    self.runtime_state = RuntimeState::Idle;
                }
            }
            JournalEvent::TurnFinished(finished) => {
                self.active(&finished.turn_id)?;
                let pending = self.pending(self.head.as_ref())?;
                require(
                    pending.iter().all(|call| {
                        self.invocation_by_call
                            .get(&key(call))
                            .is_some_and(|id| self.invocations[id].result.is_none())
                    }),
                    "unfinished_not_started_proposals",
                )?;
                if finished.outcome == TurnOutcome::Completed {
                    require(
                        pending.is_empty()
                            && !self.has_unresolved()
                            && matches!(
                                self.runtime_state,
                                RuntimeState::Processing | RuntimeState::Continuing
                            ),
                        "invalid_completed_turn",
                    )?;
                    require(self.head.as_ref().and_then(|id| self.entries.get(id)).is_some_and(|v| matches!(&v.entry.payload, EntryPayload::AssistantResponse(r) if r.outcome == ResponseOutcome::Completed && matches!(r.finish_reason, Some(FinishReason::Stop | FinishReason::Refusal)))), "completion_without_stop")?;
                }
                if self.has_unresolved() {
                    require(
                        finished.outcome == TurnOutcome::Interrupted,
                        "uncertain_turn_must_interrupt",
                    )?;
                }
                self.turns
                    .get_mut(&finished.turn_id)
                    .ok_or(StoreError::Invalid("unknown_turn"))?
                    .outcome = Some(finished.outcome);
                self.active_turn = None;
                self.inference_response = None;
                self.runtime_state = if self.has_unresolved() {
                    RuntimeState::Recovering
                } else {
                    RuntimeState::Idle
                };
                // Only the active turn can hold unused approvals and decisions.
                for (_, id) in std::mem::take(&mut self.unused_approvals) {
                    self.approvals[&id].used = true;
                }
                for id in std::mem::take(&mut self.unused_decisions) {
                    self.decisions[&id].used = true;
                }
            }
            JournalEvent::ConfigurationChanged(configuration) => {
                // An undo is never the non-idle grant exception: nothing may
                // change the configuration, or its revision, while one waits.
                require(self.undo.is_none(), "configuration_inside_turn")?;
                if self.runtime_state != RuntimeState::Idle {
                    // Decision 16: a remembered grant is atomic with, and
                    // ordered after, the validated allow consuming a ticket.
                    require(
                        configuration.policy_revision != self.policy_revision
                            && prior.iter().any(|event| {
                                matches!(event,
                                    JournalEvent::AuthorizationDecided(decision)
                                    if decision.decision == Decision::Allow
                                        && decision.approval_id.is_some()
                                        && decision.policy_revision == self.policy_revision
                                )
                            }),
                        "configuration_inside_turn",
                    )?;
                } else {
                    self.idle()?;
                }
                self.configuration = configuration.configuration.clone();
                self.policy_revision = configuration.policy_revision.clone();
            }
            JournalEvent::TailRepaired(repair) => {
                require(
                    repair.original_size > repair.valid_bytes
                        && repair.quarantine_artifact.bytes
                            == repair.original_size - repair.valid_bytes,
                    "invalid_tail_repair",
                )?;
            }
            JournalEvent::UndoIntent(intent) => {
                // Idle covers no active turn, no unresolved invocation and no
                // other undo; an undo never fabricates a turn to borrow the
                // ordinary executing states.
                self.idle()?;
                self.unique(intent.undo_id.as_str())?;
                require(
                    intent.anchor_head.as_ref() == self.head.as_ref()
                        && intent.configuration == self.configuration
                        && intent.policy_revision == self.policy_revision,
                    "undo_binding",
                )?;
                self.unique_undo_targets(&intent.targets)?;
                // The intent must be exactly the selection storage derives.
                let selection = self.undo_selection(self.head.as_ref())?;
                let UndoSelection::Candidate {
                    turn_id,
                    targets,
                    omitted_calls,
                } = selection
                else {
                    return Err(StoreError::Invalid("undo_binding"));
                };
                require(
                    turn_id == intent.target_turn_id && targets == intent.targets,
                    "undo_binding",
                )?;
                // Decision 17: a v0.2.0 writer emitted only the target turn's
                // own calls. A journal carrying that list still replays, and no
                // third list is accepted. History is never rewritten.
                let legacy = if omitted_calls == intent.omitted_calls {
                    true
                } else {
                    let ancestry = self.ancestry(self.head.as_ref())?;
                    let invocations = self.invocations_of(&turn_id);
                    self.omitted_calls(&ancestry, &turn_id, &invocations, &[], false)
                        == intent.omitted_calls
                };
                require(legacy, "undo_binding")?;
                for target in &intent.targets {
                    require(
                        target.pre_image.is_some() == target.pre_mode.is_some(),
                        "undo_image_binding",
                    )?;
                    require(
                        target.pre_mode.is_none_or(|mode| mode <= 0o777),
                        "undo_image_binding",
                    )?;
                }
                self.undo = Some(PendingUndo {
                    intent: intent.clone(),
                    sequence: record.seq,
                    uncertain: false,
                });
            }
            JournalEvent::UndoUncertain(uncertain) => {
                let pending = self
                    .undo
                    .as_mut()
                    .ok_or(StoreError::Invalid("undo_recovery_state"))?;
                require(
                    pending.intent.undo_id == uncertain.undo_id && !pending.uncertain,
                    "undo_recovery_state",
                )?;
                require(
                    !uncertain.reason.is_empty()
                        && uncertain.reason.len()
                            <= crate::session::undo::MAX_UNCERTAINTY_REASON_BYTES,
                    "undo_recovery_state",
                )?;
                pending.uncertain = true;
                self.runtime_state = RuntimeState::Recovering;
            }
            JournalEvent::UndoResult(result) => {
                let phase = self
                    .undo
                    .as_ref()
                    .map(|undo| undo.uncertain)
                    .ok_or(StoreError::Invalid("undo_recovery_state"))?;
                match result.outcome {
                    UndoOutcome::Succeeded | UndoOutcome::NotApplied => require(
                        !phase && self.runtime_state == RuntimeState::Idle,
                        "undo_recovery_state",
                    )?,
                    UndoOutcome::Observed | UndoOutcome::AcknowledgedUnknown => require(
                        phase && self.runtime_state == RuntimeState::Recovering,
                        "undo_recovery_state",
                    )?,
                }
                require(
                    self.active_turn.is_none()
                        && self.pending(self.head.as_ref())?.is_empty()
                        && self.unresolved.is_empty(),
                    "undo_commit_shape",
                )?;
                // The shape check already proved the note is this commit's
                // single entry and that it is now the head.
                let note = self
                    .entries
                    .get(&result.note_entry_id)
                    .ok_or(StoreError::Invalid("undo_result_binding"))?;
                require(
                    self.head.as_ref() == Some(&result.note_entry_id)
                        && matches!(note.entry.payload, EntryPayload::UndoNote(_)),
                    "undo_result_binding",
                )?;
                if let Some(undo) = self.undo.take() {
                    require(undo.intent.undo_id == result.undo_id, "undo_result_binding")?;
                }
                self.runtime_state = RuntimeState::Idle;
            }
        }
        Ok(())
    }

    // ----- undo -----------------------------------------------------------

    /// The exact commit shape of an undo record, checked before any entry is
    /// inserted so a terminal note is validated against the intent it resolves.
    fn undo_shape(&self, commit: &Commit, sequence: u64) -> Result<()> {
        let kinds: Vec<&JournalEvent> = commit
            .events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    JournalEvent::UndoIntent(_)
                        | JournalEvent::UndoUncertain(_)
                        | JournalEvent::UndoResult(_)
                )
            })
            .collect();
        let notes: Vec<(&Entry, &UndoNote)> = commit
            .entries
            .iter()
            .filter_map(|entry| match &entry.payload {
                EntryPayload::UndoNote(note) => Some((entry, note)),
                _ => None,
            })
            .collect();
        if kinds.is_empty() {
            // A note is terminal evidence; it is never committed alone, and no
            // other entry may ride along with an undo record.
            require(notes.is_empty(), "undo_commit_shape")?;
            return Ok(());
        }
        require(
            commit.events.len() == 1
                && commit.head.reason != HeadReason::Navigate
                && notes.len() <= 1,
            "undo_commit_shape",
        )?;
        match &commit.events[0] {
            JournalEvent::UndoIntent(_) | JournalEvent::UndoUncertain(_) => require(
                commit.entries.is_empty() && commit.head.reason == HeadReason::Retain,
                "undo_commit_shape",
            ),
            JournalEvent::UndoResult(result) => {
                require(
                    commit.entries.len() == 1
                        && notes.len() == 1
                        && commit.head.reason == HeadReason::Advance
                        && commit.head.from == self.head,
                    "undo_commit_shape",
                )?;
                let (entry, note) = notes
                    .first()
                    .copied()
                    .ok_or(StoreError::Invalid("undo_commit_shape"))?;
                self.undo_terminal(entry, note, result, sequence)?;
                Ok(())
            }
            _ => Err(StoreError::Invalid("undo_commit_shape")),
        }
    }

    /// The exact text and payload binding of a terminal note/result pair.
    fn undo_terminal(
        &self,
        entry: &Entry,
        note: &UndoNote,
        result: &UndoResult,
        sequence: u64,
    ) -> Result<()> {
        let pending = self
            .undo
            .as_ref()
            .ok_or(StoreError::Invalid("undo_result_binding"))?;
        let intent = &pending.intent;
        require(pending.sequence < sequence, "undo_result_binding")?;
        require(
            intent.undo_id == result.undo_id && note.undo_id == result.undo_id,
            "undo_result_binding",
        )?;
        require(
            note.outcome == result.outcome && note.target_turn_id == intent.target_turn_id,
            "undo_result_binding",
        )?;
        require(
            entry.turn_id.is_none() && entry.parent_id.as_ref() == intent.anchor_head.as_ref(),
            "undo_binding",
        )?;
        require(result.note_entry_id == entry.id, "undo_result_binding")?;
        require(
            result.file_outcomes.len() == intent.targets.len()
                && result
                    .file_outcomes
                    .iter()
                    .zip(&intent.targets)
                    .all(|(row, target)| row.path == target.path),
            "undo_result_binding",
        )?;
        require(
            crate::session::undo::consistent(result.outcome, &result.file_outcomes),
            "undo_result_binding",
        )?;
        match result.outcome {
            UndoOutcome::Succeeded | UndoOutcome::NotApplied => {
                require(!pending.uncertain, "undo_recovery_state")?
            }
            UndoOutcome::Observed | UndoOutcome::AcknowledgedUnknown => {
                require(pending.uncertain, "undo_recovery_state")?;
                let note = result
                    .resolution_note
                    .as_deref()
                    .ok_or(StoreError::Invalid("undo_result_binding"))?;
                require(
                    !note.is_empty()
                        && note.len() <= crate::session::undo::MAX_RESOLUTION_NOTE_BYTES,
                    "undo_result_binding",
                )?;
            }
        }
        require(
            matches!(
                result.outcome,
                UndoOutcome::Observed | UndoOutcome::AcknowledgedUnknown
            ) == result.resolution_note.is_some(),
            "undo_result_binding",
        )?;
        let omitted = self
            .omitted_names(&intent.omitted_calls)
            .ok_or(StoreError::Invalid("undo_result_binding"))?;
        require(
            note.text
                == crate::session::undo::note_text(
                    &intent.target_turn_id,
                    &intent.undo_id,
                    result.outcome,
                    &intent.targets,
                    &result.file_outcomes,
                    &omitted,
                    result.resolution_note.as_deref(),
                ),
            "undo_result_binding",
        )
    }

    /// The tool name of every omitted call, in intent order, from its proposal.
    fn omitted_names(&self, calls: &[CallRef]) -> Option<Vec<(CallRef, String)>> {
        calls
            .iter()
            .map(|call| {
                let stored = self.entries.get(&call.response_entry_id)?;
                let EntryPayload::AssistantResponse(response) = &stored.entry.payload else {
                    return None;
                };
                response.content.iter().find_map(|block| match block {
                    ContentBlock::ToolCall { call_id, name, .. } if call_id == &call.call_id => {
                        Some((call.clone(), name.clone()))
                    }
                    _ => None,
                })
            })
            .collect()
    }

    /// Targets are unique by path and sorted bytewise.
    fn unique_undo_targets(&self, targets: &[UndoTarget]) -> Result<()> {
        require(
            targets
                .windows(2)
                .all(|pair| pair[0].path.as_str() < pair[1].path.as_str())
                && targets
                    .iter()
                    .all(|target| !target.invocation_ids.is_empty()),
            "undo_binding",
        )
    }

    /// The newest finished, unconsumed turn with trusted file mutations, and
    /// what undoing it would restore. Storage validates intents against this
    /// same function, so a candidate and its intent cannot diverge.
    pub(crate) fn undo_selection(&self, head: Option<&EntryId>) -> Result<UndoSelection> {
        let ancestry = self.ancestry(head)?;
        let consumed = self.facts(head)?.consumed.clone();
        // Turn order of first appearance on the branch, and the branch
        // position of every entry.
        let mut turns: Vec<TurnId> = Vec::new();
        for stored in &ancestry {
            if let Some(turn) = &stored.entry.turn_id
                && !turns.contains(turn)
            {
                turns.push(turn.clone());
            }
        }
        // Every proposal on the branch, in ancestral proposal/block order.
        let mut proposals: Vec<(CallRef, String, Option<TurnId>)> = Vec::new();
        for stored in &ancestry {
            let EntryPayload::AssistantResponse(response) = &stored.entry.payload else {
                continue;
            };
            for block in &response.content {
                if let ContentBlock::ToolCall { call_id, name, .. } = block {
                    proposals.push((
                        CallRef {
                            response_entry_id: stored.entry.id.clone(),
                            call_id: call_id.clone(),
                        },
                        name.clone(),
                        stored.entry.turn_id.clone(),
                    ));
                }
            }
        }
        let mut skipped: Vec<TurnId> = Vec::new();
        let mut selected = None;
        for turn in turns.iter().rev() {
            if consumed.contains(turn) {
                continue;
            }
            // Finished turns only; an unfinished one is not undoable evidence.
            if !self
                .turns
                .get(turn)
                .is_some_and(|info| info.outcome.is_some())
            {
                selected = Some(Ok(UndoSelection::Incomplete(turn.clone())));
                break;
            }
            let invocations = self.invocations_of(turn);
            let mutations: Vec<&Invocation> = invocations
                .iter()
                .copied()
                .filter(|invocation| is_write_edit(invocation))
                .collect();
            if mutations.is_empty() {
                // An intent whose effects were never recorded might have been
                // a mutation, so its turn is refused rather than skipped.
                if invocations
                    .iter()
                    .any(|invocation| invocation.intent.effects.is_none())
                {
                    selected = Some(Ok(UndoSelection::Legacy(turn.clone())));
                    break;
                }
                skipped.push(turn.clone());
                continue;
            }
            // The newer turns skipped on the way stay in the report (decision
            // 17): their bash calls are not reversed either.
            selected =
                Some(self.undo_candidate(turn, &ancestry, &invocations, &mutations, &skipped));
            break;
        }
        match selected {
            Some(selected) => selected,
            None => {
                // Nothing can be undone; still report the bash calls of every
                // turn that was skipped without being consumed.
                let omitted: Vec<CallRef> = proposals
                    .iter()
                    .filter(|(_, name, at)| {
                        name == "bash"
                            && at.as_ref().is_some_and(|turn| {
                                skipped.contains(turn) && !consumed.contains(turn)
                            })
                    })
                    .map(|(call, _, _)| call.clone())
                    .collect();
                Ok(UndoSelection::Nothing {
                    omitted_calls: omitted,
                })
            }
        }
    }

    /// Intents of one turn, in execution order (at most one per commit).
    fn invocations_of(&self, turn: &TurnId) -> Vec<&Invocation> {
        let mut invocations: Vec<&Invocation> = self
            .invocations
            .values()
            .filter(|invocation| invocation.intent.turn_id == *turn)
            .collect();
        invocations.sort_by_key(|invocation| invocation.sequence);
        invocations
    }

    /// Classify one candidate turn and build its canonical targets.
    fn undo_candidate(
        &self,
        turn: &TurnId,
        ancestry: &[&StoredEntry],
        invocations: &[&Invocation],
        mutations: &[&Invocation],
        skipped: &[TurnId],
    ) -> Result<UndoSelection> {
        // The head must contain every recorded entry of the turn: a head
        // navigated into a turn is not the whole turn.
        let branch: BTreeSet<&EntryId> = ancestry.iter().map(|s| &s.entry.id).collect();
        for (id, stored) in self.entries.iter() {
            if stored.entry.turn_id.as_ref() == Some(turn) && !branch.contains(id) {
                return Ok(UndoSelection::Incomplete(turn.clone()));
            }
        }
        // A trusted mutation recorded before capture existed has no image.
        if mutations
            .iter()
            .any(|invocation| invocation.intent.file_image.is_none())
        {
            return Ok(UndoSelection::Legacy(turn.clone()));
        }
        let mut applied: Vec<(&Invocation, FileImageTarget)> = Vec::new();
        for invocation in mutations {
            let image = invocation
                .intent
                .file_image
                .clone()
                .ok_or(StoreError::Invalid("undo_image_binding"))?;
            let per_path = self.file_paths(invocation);
            if per_path.is_empty() {
                match self.file_effect(invocation) {
                    Some(FileEffect::Applied) => {
                        applied.extend(image.targets().map(|target| {
                            (
                                *invocation,
                                FileImageTarget {
                                    path: target.path.to_owned(),
                                    pre_image: target.pre_image.to_owned(),
                                    pre_mode: target.pre_mode,
                                    post_image: target.post_image.clone(),
                                },
                            )
                        }));
                    }
                    Some(FileEffect::NotApplied) => {}
                    Some(FileEffect::Unknown) | None => {
                        return Ok(UndoSelection::UnknownEffect(turn.clone()));
                    }
                }
            } else {
                let paths: BTreeSet<&str> = image.targets().map(|target| target.path).collect();
                if per_path.len() != paths.len()
                    || per_path
                        .iter()
                        .any(|effect| !paths.contains(effect.path.as_str()))
                {
                    return Ok(UndoSelection::UnknownEffect(turn.clone()));
                }
                for target in image.targets() {
                    match per_path
                        .iter()
                        .find(|effect| effect.path == target.path)
                        .map(|effect| effect.effect)
                    {
                        Some(FileEffect::Applied) => applied.push((
                            *invocation,
                            FileImageTarget {
                                path: target.path.to_owned(),
                                pre_image: target.pre_image.to_owned(),
                                pre_mode: target.pre_mode,
                                post_image: target.post_image.clone(),
                            },
                        )),
                        Some(FileEffect::NotApplied) => {}
                        Some(FileEffect::Unknown) | None => {
                            return Ok(UndoSelection::UnknownEffect(turn.clone()));
                        }
                    }
                }
            }
        }
        let mut targets: Vec<UndoTarget> = Vec::new();
        let mut conflicts: Vec<UndoConflict> = Vec::new();
        let mut by_path: BTreeMap<&str, Vec<(&Invocation, &FileImageTarget)>> = BTreeMap::new();
        for (invocation, image) in &applied {
            by_path
                .entry(image.path.as_str())
                .or_default()
                .push((*invocation, image));
        }
        for (path, chain) in by_path {
            let Some((_, first)) = chain.first().copied() else {
                continue;
            };
            let Some((_, last)) = chain.last().copied() else {
                continue;
            };
            let mode_ok = chain
                .iter()
                .all(|(_, image)| image.pre_image.is_some() == image.pre_mode.is_some());
            let chained = chain.windows(2).all(|pair| {
                pair[1].1.pre_image.as_ref().map(|pre| &pre.sha256) == Some(&pair[0].1.post_image)
            });
            if !mode_ok {
                conflicts.push(UndoConflict {
                    path: path.to_owned(),
                    reason: "image_binding",
                });
                continue;
            }
            if !chained {
                conflicts.push(UndoConflict {
                    path: path.to_owned(),
                    reason: "image_chain",
                });
                continue;
            }
            targets.push(UndoTarget {
                path: path.to_owned(),
                invocation_ids: chain
                    .iter()
                    .map(|(invocation, _)| invocation.intent.invocation_id.clone())
                    .collect(),
                pre_image: first.pre_image.clone(),
                pre_mode: first.pre_mode,
                post_image: last.post_image.clone(),
            });
        }
        if !conflicts.is_empty() {
            return Ok(UndoSelection::Conflict(conflicts));
        }
        Ok(UndoSelection::Candidate {
            turn_id: turn.clone(),
            targets,
            omitted_calls: self.omitted_calls(ancestry, turn, invocations, skipped, true),
        })
    }

    /// The calls an undo does not reverse, in ancestral proposal order and
    /// unique by `CallRef`: every bash proposal of the target turn and, on the
    /// way to it, of the newer unconsumed turns that were skipped (decision
    /// 17); plus executed non-write/edit calls of the target turn declaring a
    /// mutation, process, network or external effect. `skipped_turns` false is
    /// the v0.2.0 shape, accepted only when replaying an existing intent.
    fn omitted_calls(
        &self,
        ancestry: &[&StoredEntry],
        turn: &TurnId,
        invocations: &[&Invocation],
        skipped: &[TurnId],
        skipped_turns: bool,
    ) -> Vec<CallRef> {
        let mut omitted: Vec<CallRef> = Vec::new();
        for (call, name, at) in self.branch_proposals(ancestry) {
            let in_turn = at.as_ref() == Some(turn);
            let skipped_turn = skipped_turns
                && at
                    .as_ref()
                    .is_some_and(|other| other != turn && skipped.contains(other));
            // An executed call outside the trusted write/edit pair that
            // declares a mutation, process, network or external effect.
            let other_effect = in_turn
                && invocations.iter().any(|invocation| {
                    invocation.intent.call == call
                        && !is_write_edit(invocation)
                        && invocation.intent.effects.as_ref().is_some_and(|effects| {
                            effects.as_slice().iter().any(|effect| {
                                matches!(
                                    effect,
                                    EffectClass::FilesystemMutation
                                        | EffectClass::Process
                                        | EffectClass::Network
                                        | EffectClass::ExternalSideEffect
                                )
                            })
                        })
                });
            if other_effect || ((in_turn || skipped_turn) && name == "bash") {
                omitted.push(call);
            }
        }
        omitted
    }

    /// Proposals of the branch, in ancestral proposal/block order.
    fn branch_proposals(
        &self,
        ancestry: &[&StoredEntry],
    ) -> Vec<(CallRef, String, Option<TurnId>)> {
        let mut proposals = Vec::new();
        for stored in ancestry {
            let EntryPayload::AssistantResponse(response) = &stored.entry.payload else {
                continue;
            };
            for block in &response.content {
                if let ContentBlock::ToolCall { call_id, name, .. } = block {
                    proposals.push((
                        CallRef {
                            response_entry_id: stored.entry.id.clone(),
                            call_id: call_id.clone(),
                        },
                        name.clone(),
                        stored.entry.turn_id.clone(),
                    ));
                }
            }
        }
        proposals
    }

    /// The classification recorded with an invocation's result, if any.
    fn file_effect(&self, invocation: &Invocation) -> Option<FileEffect> {
        let id = invocation.result.as_ref()?;
        let stored = self.entries.get(id)?;
        let EntryPayload::ToolResult(result) = &stored.entry.payload else {
            return None;
        };
        result.result.metadata.file_effect
    }

    fn file_paths(&self, invocation: &Invocation) -> &[FilePathEffect] {
        let Some(id) = invocation.result.as_ref() else {
            return &[];
        };
        let Some(stored) = self.entries.get(id) else {
            return &[];
        };
        let EntryPayload::ToolResult(result) = &stored.entry.payload else {
            return &[];
        };
        &result.result.metadata.file_paths
    }

    /// The effective non-default mode of every controlled turn reachable from
    /// `head`, as `Step` computes it. The builder folds the same rule.
    fn controls_at(&self, head: Option<&EntryId>) -> Result<&OrdMap<TurnId, ContextMode>> {
        Ok(&self.facts(head)?.controls)
    }
    /// Entries of `turn` on the ancestry of `head` that an active summary covers.
    /// An active summary is one no other summary on this ancestry supersedes,
    /// exactly as the builder's `Plan` counts them.
    fn covered_by_active_summary(&self, ancestry: &[&StoredEntry], turn: &TurnId) -> bool {
        let superseded: BTreeSet<&EntryId> = ancestry
            .iter()
            .filter_map(|stored| match &stored.entry.payload {
                EntryPayload::Compaction(compaction) => Some(compaction.supersedes.iter()),
                _ => None,
            })
            .flatten()
            .collect();
        let covered: BTreeSet<&EntryId> = ancestry
            .iter()
            .filter_map(|stored| match &stored.entry.payload {
                EntryPayload::Compaction(compaction) if !superseded.contains(&stored.entry.id) => {
                    Some(compaction.covered_entry_ids.iter())
                }
                _ => None,
            })
            .flatten()
            .collect();
        ancestry.iter().any(|stored| {
            stored.entry.turn_id.as_ref() == Some(turn) && covered.contains(&stored.entry.id)
        })
    }

    /// Why a `context_control` would be invalid, as its stable code; `Ok` when
    /// it would be accepted. The runtime calls this before persisting, so its
    /// rejection carries the code storage uses.
    pub(crate) fn control_error(
        &self,
        head: Option<&EntryId>,
        turn: &TurnId,
        mode: ContextMode,
    ) -> std::result::Result<(), &'static str> {
        if !self
            .turns
            .get(turn)
            .is_some_and(|info| info.outcome.is_some())
        {
            return Err("context_control_unfinished_turn");
        }
        let ancestry = self.ancestry(head).map_err(|_| "unknown_head_or_parent")?;
        if !ancestry
            .iter()
            .any(|stored| stored.entry.turn_id.as_ref() == Some(turn))
        {
            return Err("context_control_not_ancestral");
        }
        let effective = self
            .controls_at(head)
            .map_err(|_| "unknown_head_or_parent")?
            .get(turn)
            .copied()
            .unwrap_or_default();
        if effective == mode {
            return Err("context_control_noop");
        }
        // Pinning content a summary already absorbed would promise that the
        // turn is kept verbatim. Excluding it would promise the same: the
        // summary text derived from it stays in the projection. Only a reset
        // is always recorded, because it is how a control is undone.
        if !mode.is_default() && self.covered_by_active_summary(&ancestry, turn) {
            return Err("context_control_covered");
        }
        Ok(())
    }

    fn compaction(&self, entry: &Entry, compaction: &Compaction) -> Result<()> {
        require(
            compaction.source_fingerprint == compaction.source_manifest.sha256
                && compaction.configuration_fingerprint == self.configuration.sha256,
            "compaction_fingerprint",
        )?;
        let ancestry = self.ancestry(entry.parent_id.as_ref())?;
        let ids: Vec<_> = ancestry.iter().map(|v| &v.entry.id).collect();
        // Decision 14 (m6-02) replaced m6-01's plain contiguity check: coverage
        // is an ancestral, ordered subsequence whose gaps are exactly the turns
        // pinned or excluded at this compaction's parent. The builder checks the
        // same rule with `session::coverage`, so the two cannot disagree.
        let facts = self.facts(entry.parent_id.as_ref())?;
        let controls = &facts.controls;
        let controlled = |turn: &TurnId| controls.get(turn).is_some_and(|mode| !mode.is_default());
        coverage::error(&ancestry, &compaction.covered_entry_ids, controlled)
            .map_err(StoreError::Invalid)?;
        // Completed turns in order of first appearance on the branch.
        let mut seen = BTreeSet::new();
        let mut completed_turns = Vec::new();
        for stored in &ancestry {
            if let Some(turn) = &stored.entry.turn_id
                && self
                    .turns
                    .get(turn)
                    .is_some_and(|t| t.outcome == Some(TurnOutcome::Completed))
                && seen.insert(turn)
            {
                completed_turns.push(turn);
            }
        }
        let recent: BTreeSet<_> = completed_turns.iter().rev().take(2).copied().collect();
        for id in &compaction.covered_entry_ids {
            let source = &self.entries[id].entry;
            if let Some(turn) = &source.turn_id {
                require(
                    self.turns
                        .get(turn)
                        .is_some_and(|t| t.outcome == Some(TurnOutcome::Completed))
                        && !recent.contains(turn),
                    "compaction_unfinished_or_recent_turn",
                )?;
                require(
                    ancestry
                        .iter()
                        .filter(|e| e.entry.turn_id.as_ref() == Some(turn))
                        .all(|e| compaction.covered_entry_ids.contains(&e.entry.id)),
                    "compaction_splits_turn",
                )?;
            }
        }
        // A pinned turn is kept verbatim on this ancestry (RFC-001 6.1) and an
        // excluded turn stays excluded (decision 14), so no summary may absorb
        // either. The effective mode at the compaction's parent decides; the
        // same fold already told `coverage` which turns a gap may contain.
        for id in &compaction.covered_entry_ids {
            let Some(turn) = &self.entries[id].entry.turn_id else {
                continue;
            };
            match controls.get(turn) {
                Some(ContextMode::Pinned) => {
                    return Err(StoreError::Invalid("compaction_covers_pinned_turn"));
                }
                Some(ContextMode::Excluded) => {
                    return Err(StoreError::Invalid("compaction_covers_excluded_turn"));
                }
                _ => {}
            }
        }
        for id in &compaction.supersedes {
            require(ids.contains(&id), "superseded_summary_not_ancestral")?;
            let EntryPayload::Compaction(previous) = &self.entries[id].entry.payload else {
                return Err(StoreError::Invalid("supersedes_not_summary"));
            };
            require(
                previous
                    .covered_entry_ids
                    .iter()
                    .all(|id| compaction.covered_entry_ids.contains(id)),
                "supersedes_loses_coverage",
            )?;
        }
        let already_superseded: BTreeSet<_> = ancestry
            .iter()
            .filter_map(|e| {
                if let EntryPayload::Compaction(c) = &e.entry.payload {
                    Some(c.supersedes.iter())
                } else {
                    None
                }
            })
            .flatten()
            .collect();
        for prior in ancestry {
            if let EntryPayload::Compaction(previous) = &prior.entry.payload
                && !already_superseded.contains(&prior.entry.id)
                && previous
                    .covered_entry_ids
                    .iter()
                    .any(|id| compaction.covered_entry_ids.contains(id))
            {
                require(
                    compaction.supersedes.contains(&prior.entry.id),
                    "overlapping_active_summaries",
                )?;
            }
        }
        Ok(())
    }
}

/// One iteration of the branch walk: the facts after `entry`, given its parent's.
/// Unchanged facts share the parent's allocation.
fn step(parent: &Arc<BranchFacts>, entry: &Entry) -> Result<Arc<BranchFacts>> {
    match &entry.payload {
        EntryPayload::AssistantResponse(response) => {
            require(parent.pending.is_empty(), "inference_inside_tool_exchange")?;
            if response.outcome != ResponseOutcome::Completed
                || response.finish_reason != Some(FinishReason::ToolCalls)
            {
                return Ok(parent.clone());
            }
            let pending: Arc<[CallRef]> = response
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { call_id, .. } => Some(CallRef {
                        response_entry_id: entry.id.clone(),
                        call_id: call_id.clone(),
                    }),
                    _ => None,
                })
                .collect();
            if pending.is_empty() {
                return Ok(parent.clone());
            }
            Ok(Arc::new(BranchFacts {
                pending,
                model: parent.model.clone(),
                controls: parent.controls.clone(),
                consumed: parent.consumed.clone(),
            }))
        }
        EntryPayload::ToolResult(result) => {
            require(
                parent.pending.first() == Some(&result.result.call),
                "orphan_duplicate_or_reordered_result",
            )?;
            Ok(Arc::new(BranchFacts {
                pending: Arc::from(&parent.pending[1..]),
                model: parent.model.clone(),
                controls: parent.controls.clone(),
                consumed: parent.consumed.clone(),
            }))
        }
        EntryPayload::UserMessage(_) | EntryPayload::Compaction(_) => {
            require(parent.pending.is_empty(), "head_inside_tool_exchange")?;
            Ok(parent.clone())
        }
        EntryPayload::ContextControl(control) => {
            require(parent.pending.is_empty(), "head_inside_tool_exchange")?;
            let mut controls = parent.controls.clone();
            if control.mode.is_default() {
                controls.remove(&control.turn_id);
            } else {
                controls.insert(control.turn_id.clone(), control.mode);
            }
            Ok(Arc::new(BranchFacts {
                pending: parent.pending.clone(),
                model: parent.model.clone(),
                controls,
                consumed: parent.consumed.clone(),
            }))
        }
        EntryPayload::ModelChanged(change) => {
            require(parent.pending.is_empty(), "head_inside_tool_exchange")?;
            Ok(Arc::new(BranchFacts {
                pending: parent.pending.clone(),
                model: change.model.clone(),
                controls: parent.controls.clone(),
                consumed: parent.consumed.clone(),
            }))
        }
        EntryPayload::UndoNote(note) => {
            require(parent.pending.is_empty(), "head_inside_tool_exchange")?;
            let consumed = if note.outcome.consumes() {
                Arc::new(
                    parent
                        .consumed
                        .iter()
                        .cloned()
                        .chain(std::iter::once(note.target_turn_id.clone()))
                        .collect(),
                )
            } else {
                parent.consumed.clone()
            };
            Ok(Arc::new(BranchFacts {
                pending: parent.pending.clone(),
                model: parent.model.clone(),
                controls: parent.controls.clone(),
                consumed,
            }))
        }
    }
}

/// A v2 result's code: executor and runtime results carry one exactly when
/// they did not succeed; a user's result never does, its provenance says why.
fn result_code(result: &ToolResult) -> Result<()> {
    let code = result.metadata.code.is_some();
    match result.metadata.provenance {
        Provenance::Executor | Provenance::Runtime => {
            if result.status == ToolStatus::Succeeded {
                require(!code, "code_on_success")
            } else {
                require(code, "code_required")
            }
        }
        Provenance::UserObserved | Provenance::UserAcknowledgedUnknown => {
            require(!code, "code_on_user_result")
        }
    }
}

/// One canonical byte form per set, and only effects the backend can have.
/// Whether policy allows them is not a storage question.
fn valid_effects(backend: ExecutionBackend, effects: &EffectSet) -> Result<()> {
    require(effects.is_canonical(), "effects_not_canonical")?;
    require(
        match backend {
            ExecutionBackend::TrustedFs => effects.as_slice().iter().all(|effect| {
                matches!(effect, EffectClass::Read | EffectClass::FilesystemMutation)
            }),
            ExecutionBackend::Bubblewrap => effects.contains(EffectClass::Process),
        },
        "effects_backend_mismatch",
    )
}

fn valid_content(content: &[ContentBlock]) -> Result<()> {
    require(
        content.iter().all(|b| {
            matches!(
                b,
                ContentBlock::Text { .. }
                    | ContentBlock::Image { .. }
                    | ContentBlock::Audio { .. }
                    | ContentBlock::File { .. }
            )
        }),
        "content_role_violation",
    )
}
fn legal_transition(from: RuntimeState, to: RuntimeState) -> bool {
    use RuntimeState::*;
    matches!(
        (from, to),
        (Idle, Preparing)
            | (Preparing, Inferring)
            | (Inferring, Processing)
            | (Processing, Authorizing | Continuing)
            | (Authorizing, Executing | Processing | Continuing)
            | (Executing, Continuing)
            | (Continuing, Processing | Preparing)
    ) || (to == Cancelling && !matches!(from, Idle | Cancelling | Recovering))
        || (to == Recovering && from != Recovering)
}

/// Whether an intent is one of the trusted write/edit mutations undo may
/// revert. An intent whose effects were never recorded is not classified
/// here: its turn is refused rather than treated as read-only.
fn is_write_edit(invocation: &Invocation) -> bool {
    invocation.intent.backend == ExecutionBackend::TrustedFs
        && invocation
            .intent
            .effects
            .as_ref()
            .is_some_and(|effects| effects.contains(EffectClass::FilesystemMutation))
}

#[cfg(test)]
thread_local! {
    /// Whether test builds check derived state after every applied record.
    static ORACLE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}
/// Run `f` on this thread without the test-build equivalence oracle, as
/// benchmarks must. The oracle is on by default in every other test.
#[cfg(test)]
pub(crate) fn without_oracle<R>(f: impl FnOnce() -> R) -> R {
    let previous = ORACLE.replace(false);
    let result = f();
    ORACLE.set(previous);
    result
}

/// Derived state of one snapshot, for comparing live and replayed state.
#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(crate) struct Derived {
    pub branch: OrdMap<EntryId, Arc<BranchFacts>>,
    pub root: Arc<BranchFacts>,
    pub consumed: BTreeSet<TurnId>,
    pub unresolved: OrdSet<InvocationId>,
    pub invocation_by_call: OrdMap<CallKey, InvocationId>,
    pub unused_approvals: OrdMap<CallKey, ApprovalId>,
    pub unused_decisions: OrdSet<DecisionId>,
}

/// The scanning implementations that derived state replaced, kept as an oracle.
#[cfg(test)]
impl State {
    fn walked_model_at(&self, head: Option<&EntryId>) -> Result<&ModelRef> {
        for stored in self.ancestry(head)?.into_iter().rev() {
            if let EntryPayload::ModelChanged(change) = &stored.entry.payload {
                return Ok(&change.model);
            }
        }
        Ok(&self.header.initial_model)
    }
    fn walked_controls(&self, head: Option<&EntryId>) -> Result<OrdMap<TurnId, ContextMode>> {
        let mut controls = OrdMap::new();
        for stored in self.ancestry(head)? {
            if let EntryPayload::ContextControl(control) = &stored.entry.payload {
                if control.mode.is_default() {
                    controls.remove(&control.turn_id);
                } else {
                    controls.insert(control.turn_id.clone(), control.mode);
                }
            }
        }
        Ok(controls)
    }
    fn walked_consumed(&self, head: Option<&EntryId>) -> Result<BTreeSet<TurnId>> {
        let mut consumed = BTreeSet::new();
        for stored in self.ancestry(head)? {
            if let EntryPayload::UndoNote(note) = &stored.entry.payload
                && note.outcome.consumes()
            {
                consumed.insert(note.target_turn_id.clone());
            }
        }
        Ok(consumed)
    }
    fn walked_pending(&self, head: Option<&EntryId>) -> Result<Vec<CallRef>> {
        let mut pending = Vec::new();
        for stored in self.ancestry(head)? {
            match &stored.entry.payload {
                EntryPayload::AssistantResponse(response) => {
                    require(pending.is_empty(), "inference_inside_tool_exchange")?;
                    if response.outcome == ResponseOutcome::Completed
                        && response.finish_reason == Some(FinishReason::ToolCalls)
                    {
                        for block in &response.content {
                            if let ContentBlock::ToolCall { call_id, .. } = block {
                                pending.push(CallRef {
                                    response_entry_id: stored.entry.id.clone(),
                                    call_id: call_id.clone(),
                                });
                            }
                        }
                    }
                }
                EntryPayload::ToolResult(result) => {
                    require(
                        pending.first() == Some(&result.result.call),
                        "orphan_duplicate_or_reordered_result",
                    )?;
                    pending.remove(0);
                }
                EntryPayload::UserMessage(_)
                | EntryPayload::ModelChanged(_)
                | EntryPayload::Compaction(_)
                | EntryPayload::ContextControl(_)
                | EntryPayload::UndoNote(_) => {
                    require(pending.is_empty(), "head_inside_tool_exchange")?;
                }
            }
        }
        Ok(pending)
    }
    pub(crate) fn derived(&self) -> Derived {
        Derived {
            branch: self.branch.clone(),
            root: self.root.clone(),
            consumed: self
                .facts(self.head.as_ref())
                .map(|facts| (*facts.consumed).clone())
                .unwrap_or_default(),
            unresolved: self.unresolved.clone(),
            invocation_by_call: self.invocation_by_call.clone(),
            unused_approvals: self.unused_approvals.clone(),
            unused_decisions: self.unused_decisions.clone(),
        }
    }
    /// Assert that branch facts and live indexes equal their scanning
    /// definitions after `record` was applied.
    fn check_derived(&self, record: &Record) {
        let mut heads = vec![None, self.head.as_ref()];
        if let RecordPayload::Commit(commit) = &record.payload {
            heads.extend(commit.entries.iter().map(|entry| Some(&entry.id)));
        }
        for head in heads {
            assert_eq!(
                self.pending(head).unwrap(),
                self.walked_pending(head).unwrap().as_slice(),
                "pending at {head:?}"
            );
            assert_eq!(
                self.model_at(head).unwrap(),
                self.walked_model_at(head).unwrap(),
                "model at {head:?}"
            );
            assert_eq!(
                self.controls_at(head).unwrap(),
                &self.walked_controls(head).unwrap(),
                "controls at {head:?}"
            );
            assert_eq!(
                &*self.facts(head).unwrap().consumed,
                &self.walked_consumed(head).unwrap(),
                "consumed turns at {head:?}"
            );
        }
        assert!(self.branch.keys().eq(self.entries.keys()));

        let unresolved: OrdSet<_> = self
            .invocations
            .iter()
            .filter(|(_, v)| v.result.is_none() || (v.uncertain && !v.recovery_resolved))
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(self.unresolved, unresolved);
        let by_call: OrdMap<_, _> = self
            .invocations
            .iter()
            .map(|(id, v)| (key(&v.intent.call), id.clone()))
            .collect();
        assert_eq!(
            by_call.len(),
            self.invocations.len(),
            "invocations per call"
        );
        assert_eq!(self.invocation_by_call, by_call);
        let unused: Vec<_> = self.approvals.iter().filter(|(_, a)| !a.used).collect();
        let approvals: OrdMap<_, _> = unused
            .iter()
            .map(|(id, a)| (key(&a.request.call), (*id).clone()))
            .collect();
        assert_eq!(approvals.len(), unused.len(), "unused approvals per call");
        assert_eq!(self.unused_approvals, approvals);
        let decisions: OrdSet<_> = self
            .decisions
            .iter()
            .filter(|(_, d)| !d.used)
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(self.unused_decisions, decisions);
        // `TurnFinished` drains these indexes, so only the active turn holds any.
        let active = self.active_turn.as_ref();
        assert!(
            unused
                .iter()
                .all(|(_, a)| Some(&a.request.turn_id) == active)
        );
        assert!(
            self.decisions
                .values()
                .filter(|d| !d.used)
                .all(|d| Some(&d.decision.turn_id) == active)
        );
    }
}
