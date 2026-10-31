//! Read-only, descriptor-rooted discovery; no writer ownership or recovery.
use super::{artifacts::verify_private, codec, error::Result, store, *};
use rustix::fs::{self, FlockOperation, Mode, OFlags};
use serde::Serialize;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
};

/// Maximum title length in Unicode scalar values, without an added ellipsis.
pub const SESSION_TITLE_CHARS: usize = 120;

/// Readability of a catalog entry; diagnostics contain codes, never record payloads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogStatus {
    /// Fully validated journal.
    Ok,
    /// A writer holds the journal lock.
    Busy,
    /// A valid header followed by an unterminated tail; explicit repair required.
    NeedsRepair,
    /// Corrupt, unsupported, unsafe or inaccessible journal.
    Unreadable,
}

/// One discovered session. Metadata is null when it cannot be inspected safely;
/// broken journals retain metadata from their fully validated prefix only.
#[derive(Clone, Debug, Serialize)]
pub struct SessionCatalogEntry {
    /// Identity parsed from the filename.
    pub session_id: SessionId,
    /// Authorized workspace from the validated header.
    pub workspace: Option<String>,
    /// Header record's timestamp.
    pub created_at: Option<Timestamp>,
    /// Last committed record's timestamp (not the file modification time).
    pub last_activity: Option<Timestamp>,
    /// Model at the durable current head.
    pub model: Option<ModelRef>,
    /// Durable operational state, without synthetic recovery.
    pub state: Option<RuntimeState>,
    /// First user message in journal order: its text's first line, at most 120 chars.
    pub title: Option<String>,
    /// Inspection result.
    pub status: CatalogStatus,
    /// Short content-free diagnostic, absent for healthy journals.
    pub reason: Option<String>,
}

impl DataRoot {
    /// Enumerate valid session filenames without writing, repairing or taking an
    /// exclusive lock. Per-session failures become entries, not listing failures.
    /// Last activity descends; unknown activity sorts last; IDs break ties.
    pub fn sessions(&self) -> Result<Vec<SessionCatalogEntry>> {
        let mut entries = Vec::new();
        for item in fs::Dir::read_from(&self.sessions)? {
            let item = item?;
            let Ok(name) = item.file_name().to_str() else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".jsonl") else {
                continue;
            };
            let Ok(id) = stem.parse::<SessionId>() else {
                continue;
            };
            let mut entry = SessionCatalogEntry {
                session_id: id.clone(),
                workspace: None,
                created_at: None,
                last_activity: None,
                model: None,
                state: None,
                title: None,
                status: CatalogStatus::Ok,
                reason: None,
            };
            if let Err(error) = self.inspect_catalog_entry(name, &id, &mut entry) {
                entry.status = if matches!(error, StoreError::Locked) {
                    CatalogStatus::Busy
                } else {
                    CatalogStatus::Unreadable
                };
                entry.reason = Some(
                    match error {
                        StoreError::Locked => "session_locked",
                        StoreError::UnsafePath => "unsafe_path",
                        StoreError::Io(_) => "io_error",
                        _ => "inspection_failed",
                    }
                    .into(),
                );
            }
            entries.push(entry);
        }
        entries.sort_by(|a, b| {
            b.last_activity
                .as_ref()
                .map(Timestamp::instant)
                .cmp(&a.last_activity.as_ref().map(Timestamp::instant))
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        Ok(entries)
    }

    fn inspect_catalog_entry(
        &self,
        name: &str,
        id: &SessionId,
        entry: &mut SessionCatalogEntry,
    ) -> Result<()> {
        let mut file: File = fs::openat(
            &self.sessions,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )?
        .into();
        verify_private(&file, false)?;
        fs::flock(&file, FlockOperation::NonBlockingLockShared).map_err(|error| {
            if error == rustix::io::Errno::WOULDBLOCK {
                StoreError::Locked
            } else {
                error.into()
            }
        })?;
        // Keep this same shared lock through validation and timestamp extraction.
        let inspection = store::scan(self, &mut file, id)?;
        if let Some(diagnostic) = inspection.diagnostic {
            entry.status =
                if diagnostic.kind == DiagnosticKind::TornTail && inspection.snapshot.is_some() {
                    CatalogStatus::NeedsRepair
                } else {
                    CatalogStatus::Unreadable
                };
            entry.reason = Some(diagnostic.code);
        }
        if let Some(snapshot) = inspection.snapshot {
            entry.workspace = Some(snapshot.header().workspace.clone());
            entry.model = Some(snapshot.model_at(snapshot.head())?.clone());
            entry.state = Some(snapshot.state());
            let first = snapshot
                .entries()
                .map(|(_, s)| s)
                .filter(|s| matches!(s.entry.payload, EntryPayload::UserMessage(_)))
                .min_by_key(|s| (s.sequence, s.entry_index));
            entry.title = first.map(|s| {
                let EntryPayload::UserMessage(message) = &s.entry.payload else {
                    unreachable!()
                };
                let text: String = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                text.lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(SESSION_TITLE_CHARS)
                    .collect()
            });
            file.seek(SeekFrom::Start(0))?;
            let mut reader = BufReader::new((&mut file).take(inspection.valid_bytes));
            let mut line = Vec::new();
            while reader.read_until(b'\n', &mut line)? != 0 {
                // scan already bounded and validated this exact locked prefix.
                let (record, _) = codec::decode(&line[..line.len() - 1])?;
                if entry.created_at.is_none() {
                    entry.created_at = Some(record.at.clone());
                }
                entry.last_activity = Some(record.at);
                line.clear();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
