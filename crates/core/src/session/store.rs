//! Single-writer append, bounded inspection, and explicit torn-tail recovery.
use super::{codec, error::Result, io::Point, state::State, validation, *};
use rustix::fs::{FlockOperation, flock};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    sync::Arc,
};

/// Acknowledgement issued only after a complete record has been synchronized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitAck {
    /// Durable journal sequence.
    pub sequence: u64,
    /// Exact-byte record checksum.
    pub checksum: Digest,
    /// Selected head at this acceptance, even if later commits moved it.
    pub head: Option<EntryId>,
}

/// Bounded read-only reconstruction, retaining diagnostics and the valid prefix.
#[derive(Clone, Debug)]
pub struct Inspection {
    /// Last fully validated state, absent if no valid header exists.
    pub snapshot: Option<SessionSnapshot>,
    /// Length of the fully validated LF-terminated prefix.
    pub valid_bytes: u64,
    /// Length of the inspected source.
    pub file_bytes: u64,
    /// Exact digest of the entire source, including an invalid suffix.
    pub fingerprint: Digest,
    /// First unaccepted record or tail; None for a complete valid journal.
    pub diagnostic: Option<Diagnostic>,
    /// Number of validated records of each version in the valid prefix.
    pub versions: BTreeMap<u32, u64>,
}

/// Durable, blocking session storage with one exclusive journal-descriptor lock.
///
/// The async driver owns this store on a dedicated storage thread. Mutations
/// stage a separate state, validate the bundled schema and semantics, then append,
/// flush, and sync before publishing. No executor exists in this component.
#[derive(Debug)]
pub struct JournalStore {
    root: Arc<DataRoot>,
    file: File,
    snapshot: SessionSnapshot,
    poisoned: bool,
    recovery_barrier: bool,
}

fn lock(file: &File, exclusive: bool) -> Result<()> {
    match flock(
        file,
        if exclusive {
            FlockOperation::NonBlockingLockExclusive
        } else {
            FlockOperation::NonBlockingLockShared
        },
    ) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(StoreError::Locked),
        Err(error) => Err(error.into()),
    }
}
fn next_record(
    snapshot: &SessionSnapshot,
    payload: RecordPayload,
    at: Timestamp,
) -> Result<Record> {
    Ok(Record {
        version: codec::CURRENT_VERSION,
        seq: snapshot
            .sequence()
            .checked_add(1)
            .filter(|v| *v <= codec::MAX_COUNTER)
            .ok_or(StoreError::Limit)?,
        record_id: RecordId::new(),
        session_id: snapshot.session_id().clone(),
        at,
        previous_checksum: Some(snapshot.checksum().clone()),
        payload,
    })
}

impl JournalStore {
    /// Create a new session, syncing its header and directory before returning.
    /// A failed creation leaves evidence at its original name; it is not retried.
    pub fn create(
        root: Arc<DataRoot>,
        id: SessionId,
        header: SessionHeader,
        at: Timestamp,
    ) -> Result<Self> {
        let record = Record {
            version: codec::CURRENT_VERSION,
            seq: 0,
            record_id: RecordId::new(),
            session_id: id.clone(),
            at,
            previous_checksum: None,
            payload: RecordPayload::SessionCreated(header),
        };
        let (bytes, digest) = codec::encode(&record)?;
        let state = State::start(&record, digest)?;
        validation::record_artifacts(&root, &record, &state)?;
        let mut file = root.journal(&id, true, true)?;
        lock(&file, true)?;
        file.write_all(&bytes)?;
        root.io.check(Point::HeaderWritten)?;
        file.flush()?;
        file.sync_all()?;
        root.io.check(Point::HeaderSynced)?;
        root.sessions.sync_all()?;
        root.io.check(Point::SessionDirectorySynced)?;
        Ok(Self {
            root,
            file,
            snapshot: SessionSnapshot(Arc::new(state)),
            poisoned: false,
            recovery_barrier: false,
        })
    }

    /// Open under an exclusive lock. Invalid complete records and torn tails
    /// both block writable opening; call `inspect` for prefix diagnostics.
    /// Unfinished work is classified without modifying history or retrying it.
    pub fn open(root: Arc<DataRoot>, id: &SessionId) -> Result<Self> {
        let mut file = root.journal(id, false, true)?;
        lock(&file, true)?;
        let inspection = scan(&root, &mut file, id)?;
        if let Some(diagnostic) = inspection.diagnostic {
            return Err(StoreError::Journal(diagnostic));
        }
        let snapshot = inspection
            .snapshot
            .ok_or(StoreError::Invalid("missing_header"))?;
        file.sync_all()?;
        root.sessions.sync_all()?;
        root.io.check(Point::ReopenSynced)?;
        let recovery_barrier = snapshot.requires_recovery();
        Ok(Self {
            root,
            file,
            snapshot,
            poisoned: false,
            recovery_barrier,
        })
    }

    /// Inspect a stable file under a shared lock, with memory bounded per line.
    /// While another engine owns it, inspection returns `Locked`; that engine's
    /// immutable snapshot remains the appropriate live inspection API.
    pub fn inspect(root: &DataRoot, id: &SessionId) -> Result<Inspection> {
        let mut file = root.journal(id, false, false)?;
        lock(&file, false)?;
        scan(root, &mut file, id)
    }

    /// Read the last durably published snapshot. Previously cloned snapshots
    /// remain unchanged after an append or a failed synchronization.
    pub fn snapshot(&self) -> SessionSnapshot {
        self.snapshot.clone()
    }

    /// True after a journal write/flush/sync failure; only reopen can clear it.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Persist a normalized command object and bind it to a proposed commit.
    /// Generated entry/record identities need not be reused when retrying a
    /// command: append returns the original receipt for the same ID/fingerprint.
    pub fn bind_command(
        &self,
        commit: &mut Commit,
        id: CommandId,
        command: &serde_json::Value,
    ) -> Result<()> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        let bytes = normalized_command(command)?;
        let reference = self.root.put_artifact(&bytes)?;
        commit.command_id = Some(id);
        commit.command_fingerprint = Some(reference.sha256);
        Ok(())
    }

    /// Validate and durably append one atomic transaction. An identical accepted
    /// command returns its original acknowledgement without appending again.
    pub fn append(&mut self, commit: Commit, at: Timestamp) -> Result<CommitAck> {
        self.append_inner(commit, at, false)
    }

    fn append_inner(&mut self, commit: Commit, at: Timestamp, repair: bool) -> Result<CommitAck> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        if let Some(receipt) = commit
            .command_id
            .as_ref()
            .and_then(|id| self.snapshot.command_receipt(id))
        {
            return if commit.command_fingerprint.as_ref() == Some(&receipt.fingerprint) {
                Ok(receipt.acknowledgement.clone())
            } else {
                Err(StoreError::StaleCommand)
            };
        }
        if !repair
            && commit
                .events
                .iter()
                .any(|e| matches!(e, JournalEvent::TailRepaired(_)))
        {
            return Err(StoreError::Invalid("repair_requires_explicit_operation"));
        }
        if self.recovery_barrier && !recovery_commit(&commit) {
            return Err(StoreError::Invalid("recovery_required"));
        }
        let record = next_record(&self.snapshot, RecordPayload::Commit(commit), at)?;
        let (bytes, checksum) = codec::encode(&record)?;
        let mut staged = (*self.snapshot.0).clone();
        staged.apply(&record, checksum.clone())?;
        validation::record_artifacts(&self.root, &record, &staged)?;
        if let Err(error) = self.root.io.append(&mut self.file, &bytes) {
            self.poisoned = true;
            return Err(error);
        }
        let ack = CommitAck {
            sequence: record.seq,
            checksum,
            head: staged.head.clone(),
        };
        self.snapshot = SessionSnapshot(Arc::new(staged));
        if !self.snapshot.requires_recovery() {
            self.recovery_barrier = false;
        }
        Ok(ack)
    }

    /// Repair exactly an unterminated suffix after confirming the entire file's
    /// expected digest and size under the writer lock. Quarantine is durable
    /// before truncation; the repair event is synced before this returns.
    /// Complete corrupt records and files without a valid header are refused.
    pub fn repair_tail(
        root: Arc<DataRoot>,
        id: &SessionId,
        expected: &Inspection,
        at: Timestamp,
    ) -> Result<Self> {
        Self::repair_inspected_tail(root, id, &expected.fingerprint, expected.file_bytes, at)
    }

    /// `repair_tail` bound to the whole-file fingerprint and size a user
    /// inspected, for callers that hold only those two values.
    pub fn repair_inspected_tail(
        root: Arc<DataRoot>,
        id: &SessionId,
        expected_fingerprint: &Digest,
        expected_size: u64,
        at: Timestamp,
    ) -> Result<Self> {
        let mut file = root.journal(id, false, true)?;
        lock(&file, true)?;
        let inspection = scan(&root, &mut file, id)?;
        if inspection.fingerprint != *expected_fingerprint || inspection.file_bytes != expected_size
        {
            return Err(StoreError::StaleRepair);
        }
        if !inspection
            .diagnostic
            .as_ref()
            .is_some_and(|d| d.kind == DiagnosticKind::TornTail)
        {
            return Err(StoreError::NotRepairable);
        }
        let snapshot = inspection.snapshot.ok_or(StoreError::NotRepairable)?;
        file.seek(SeekFrom::Start(inspection.valid_bytes))?;
        let quarantine = root.put_artifact_reader(
            &mut (&mut file).take(inspection.file_bytes - inspection.valid_bytes),
        )?;
        file.set_len(inspection.valid_bytes)?;
        root.io.check(Point::TailTruncated)?;
        file.sync_all()?;
        root.io.check(Point::TailSynced)?;
        let commit = Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: snapshot.head().cloned(),
                to: snapshot.head().cloned(),
                reason: HeadReason::Retain,
            },
            entries: Vec::new(),
            events: vec![JournalEvent::TailRepaired(TailRepaired {
                original_size: inspection.file_bytes,
                valid_bytes: inspection.valid_bytes,
                quarantine_artifact: quarantine,
            })],
        };
        let recovery_barrier = snapshot.requires_recovery();
        let mut store = Self {
            root,
            file,
            snapshot,
            poisoned: false,
            recovery_barrier,
        };
        store.append_inner(commit, at, true)?;
        Ok(store)
    }
}

/// The exact artifact bytes `bind_command` stores for a submitted command. Its
/// SHA-256 is the commit's `command_fingerprint`, so the runtime fingerprints
/// commands with this function instead of a second normalization.
pub(crate) fn normalized_command(command: &serde_json::Value) -> Result<Vec<u8>> {
    if !command.is_object() {
        return Err(StoreError::Invalid("command_not_object"));
    }
    codec::bounded_json(command)
}

fn recovery_commit(commit: &Commit) -> bool {
    commit.head.reason != HeadReason::Navigate
        && commit
            .entries
            .iter()
            .all(|e| matches!(e.payload, EntryPayload::ToolResult(_)))
        && commit.events.iter().all(|e| match e {
            JournalEvent::ExecutionUncertain(_)
            | JournalEvent::RecoveryResolved(_)
            | JournalEvent::TailRepaired(_) => true,
            JournalEvent::TurnFinished(v) => v.outcome == TurnOutcome::Interrupted,
            JournalEvent::StateChanged(v) => v.to == RuntimeState::Recovering,
            _ => false,
        })
}

// Never allocate a whole journal or an unbounded physical line. Oversized tails
// can still be quarantined by streaming; oversized complete records are corrupt.
fn physical_line(reader: &mut impl BufRead) -> Result<(Vec<u8>, u64, bool)> {
    let mut bytes = Vec::new();
    let mut length = 0u64;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((bytes, length, false));
        }
        let lf = available.iter().position(|b| *b == b'\n');
        let count = lf.map_or(available.len(), |i| i + 1);
        length = length.checked_add(count as u64).ok_or(StoreError::Limit)?;
        if length <= codec::MAX_LINE_BYTES as u64 {
            bytes.extend_from_slice(&available[..count]);
        }
        reader.consume(count);
        if lf.is_some() {
            return Ok((bytes, length, true));
        }
    }
}
fn scan(root: &DataRoot, file: &mut File, id: &SessionId) -> Result<Inspection> {
    use sha2::{Digest as _, Sha256};
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut file_bytes = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        file_bytes = file_bytes
            .checked_add(n as u64)
            .filter(|v| *v <= codec::MAX_COUNTER)
            .ok_or(StoreError::Limit)?;
    }
    let fingerprint = format!("{:x}", hash.finalize()).parse().expect("SHA-256");
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(file);
    let mut state: Option<State> = None;
    let mut valid_bytes = 0;
    let mut diagnostic = None;
    let mut versions = BTreeMap::new();
    loop {
        let (bytes, length, terminated) = physical_line(&mut reader)?;
        if length == 0 {
            break;
        }
        if !terminated {
            diagnostic = Some(Diagnostic {
                offset: valid_bytes,
                kind: DiagnosticKind::TornTail,
                code: "unterminated_tail".into(),
            });
            break;
        }
        let result = (|| {
            if length > codec::MAX_LINE_BYTES as u64 {
                return Err(StoreError::Limit);
            }
            let (record, checksum) = codec::decode(&bytes[..bytes.len() - 1])?;
            if record.session_id != *id {
                return Err(StoreError::Invalid("session_filename_mismatch"));
            }
            let staged = if let Some(previous) = &state {
                let mut staged = previous.clone();
                staged.apply(&record, checksum)?;
                staged
            } else {
                State::start(&record, checksum)?
            };
            if let RecordPayload::Commit(commit) = &record.payload {
                for event in &commit.events {
                    if let JournalEvent::TailRepaired(repair) = event
                        && (repair.valid_bytes != valid_bytes
                            || commit.events.len() != 1
                            || !commit.entries.is_empty()
                            || commit.head.reason != HeadReason::Retain)
                    {
                        return Err(StoreError::Invalid("invalid_repair_boundary"));
                    }
                }
            }
            validation::record_artifacts(root, &record, &staged)?;
            Ok((staged, record.version))
        })();
        match result {
            Ok((staged, version)) => {
                state = Some(staged);
                valid_bytes += length;
                *versions.entry(version).or_insert(0) += 1;
            }
            Err(error) => {
                let (kind, code) = match error {
                    StoreError::Invalid("unsupported_version") => {
                        (DiagnosticKind::UnsupportedVersion, "unsupported_version")
                    }
                    StoreError::Invalid(code) => (DiagnosticKind::Corrupt, code),
                    StoreError::Artifact => {
                        (DiagnosticKind::Artifact, "artifact_missing_or_invalid")
                    }
                    StoreError::Limit => (DiagnosticKind::Limit, "record_limit_exceeded"),
                    StoreError::StaleHead => (DiagnosticKind::Corrupt, "stale_head"),
                    StoreError::StaleCommand => (DiagnosticKind::Corrupt, "stale_command"),
                    _ => return Err(error),
                };
                diagnostic = Some(Diagnostic {
                    offset: valid_bytes,
                    kind,
                    code: code.into(),
                });
                break;
            }
        }
    }
    if state.is_none() && diagnostic.is_none() {
        diagnostic = Some(Diagnostic {
            offset: 0,
            kind: DiagnosticKind::Corrupt,
            code: "missing_header".into(),
        });
    }
    Ok(Inspection {
        snapshot: state.map(|s| SessionSnapshot(Arc::new(s))),
        valid_bytes,
        file_bytes,
        fingerprint,
        diagnostic,
        versions,
    })
}
