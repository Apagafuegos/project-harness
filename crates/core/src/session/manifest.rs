//! Exact source provenance shared by preview generation and storage validation.
use super::*;

/// Canonical compact JSON manifest bytes, in supplied ancestral order.
/// References bind exact containing record bytes and entry array positions.
pub fn compaction_manifest(
    snapshot: &SessionSnapshot,
    covered: &[EntryId],
    supersedes: &[EntryId],
    configuration: &Digest,
) -> Result<Vec<u8>, StoreError> {
    manifest_bytes(covered, supersedes, configuration, |id| snapshot.entry(id))
}

pub(crate) fn manifest_bytes<'a>(
    covered: &[EntryId],
    supersedes: &[EntryId],
    configuration: &Digest,
    lookup: impl Fn(&EntryId) -> Option<&'a StoredEntry>,
) -> Result<Vec<u8>, StoreError> {
    let references = |ids: &[EntryId]| -> Result<Vec<serde_json::Value>, StoreError> {
        ids.iter().map(|id| {
            let stored = lookup(id).ok_or(StoreError::Invalid("manifest_unknown_entry"))?;
            Ok(serde_json::json!({"entry_id": id, "record_checksum": stored.record_checksum, "entry_index": stored.entry_index}))
        }).collect()
    };
    Ok(serde_json::to_vec(&serde_json::json!({
        "covered": references(covered)?,
        "supersedes": references(supersedes)?,
        "configuration_fingerprint": configuration,
    }))
    .expect("manifest values serialize"))
}
