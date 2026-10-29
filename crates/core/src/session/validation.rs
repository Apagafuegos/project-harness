//! Verify every typed artifact reference before publishing reconstructed state.
use super::{error::Result, state::State, *};

fn blocks(root: &DataRoot, content: &[ContentBlock]) -> Result<()> {
    for block in content {
        match block {
            ContentBlock::Image { artifact, .. }
            | ContentBlock::Audio { artifact, .. }
            | ContentBlock::File { artifact, .. }
            | ContentBlock::ProviderState { artifact, .. } => root.verify_artifact(artifact)?,
            _ => (),
        }
    }
    Ok(())
}

pub(crate) fn record_artifacts(root: &DataRoot, record: &Record, state: &State) -> Result<()> {
    match &record.payload {
        RecordPayload::SessionCreated(header) => {
            root.verify_artifact(&header.configuration)?;
            let workspace = std::path::Path::new(&header.workspace);
            if !workspace.is_absolute()
                || workspace
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(StoreError::Invalid("workspace_not_absolute"));
            }
            let resolved = workspace
                .canonicalize()
                .unwrap_or_else(|_| workspace.to_path_buf());
            if root.path().starts_with(&resolved) || resolved.starts_with(root.path()) {
                return Err(StoreError::Invalid("storage_overlaps_workspace"));
            }
        }
        RecordPayload::Commit(commit) => {
            if let Some(fingerprint) = &commit.command_fingerprint {
                root.verify_digest(fingerprint)?;
                let reference = ArtifactRef {
                    sha256: fingerprint.clone(),
                    bytes: root.artifact_size(fingerprint)?,
                };
                let bytes = root.read_artifact(&reference, codec::MAX_LINE_BYTES)?;
                if !codec::unique_json(&bytes)?.is_object() {
                    return Err(StoreError::Invalid("command_not_object"));
                }
            }
            for entry in &commit.entries {
                match &entry.payload {
                    EntryPayload::UserMessage(message) => blocks(root, &message.content)?,
                    EntryPayload::AssistantResponse(response) => {
                        blocks(root, &response.content)?;
                        if let Some(continuation) = &response.continuation {
                            root.verify_artifact(&continuation.artifact)?;
                        }
                    }
                    EntryPayload::ToolResult(result) => {
                        blocks(root, &result.result.content)?;
                        for reference in &result.result.metadata.artifacts {
                            root.verify_artifact(reference)?;
                        }
                    }
                    EntryPayload::Compaction(compaction) => {
                        root.verify_artifact(&compaction.source_manifest)?;
                        root.verify_digest(&compaction.configuration_fingerprint)?;
                        manifest(root, compaction, state)?;
                    }
                    EntryPayload::ModelChanged(_) => (),
                }
            }
            for event in &commit.events {
                match event {
                    JournalEvent::TurnStarted(start) => {
                        root.verify_artifact(&start.configuration)?
                    }
                    JournalEvent::ConfigurationChanged(change) => {
                        root.verify_artifact(&change.configuration)?
                    }
                    JournalEvent::ApprovalRequested(request) => {
                        root.verify_artifact(&request.plan)?
                    }
                    JournalEvent::AuthorizationDecided(decision) => {
                        root.verify_artifact(&decision.plan)?
                    }
                    JournalEvent::TailRepaired(repair) => {
                        root.verify_artifact(&repair.quarantine_artifact)?
                    }
                    _ => (),
                }
            }
        }
    }
    Ok(())
}

fn manifest(root: &DataRoot, compaction: &Compaction, state: &State) -> Result<()> {
    let bytes = root.read_artifact(&compaction.source_manifest, codec::MAX_LINE_BYTES)?;
    let actual = codec::unique_json(&bytes)?;
    let expected = codec::unique_json(&super::manifest::manifest_bytes(
        &compaction.covered_entry_ids,
        &compaction.supersedes,
        &compaction.configuration_fingerprint,
        |id| state.entries.get(id).map(std::sync::Arc::as_ref),
    )?)?;
    if actual != expected {
        return Err(StoreError::Invalid("source_manifest_mismatch"));
    }
    Ok(())
}
