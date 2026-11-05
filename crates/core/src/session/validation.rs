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
                    EntryPayload::ContextControl(_) => (),
                    // A note references no artifact.
                    EntryPayload::UndoNote(_) => (),
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
                    // A captured pre-image is a direct reference: its bytes
                    // must exist before the intent that binds them is durable,
                    // and at every replay afterwards.
                    JournalEvent::ExecutionIntent(intent) => {
                        if let Some(image) = &intent.file_image {
                            if let Some(pre) = &image.pre_image {
                                root.verify_artifact(pre)?;
                            }
                            plan_image(root, intent, state)?;
                        }
                    }
                    JournalEvent::UndoIntent(intent) => {
                        root.verify_artifact(&intent.configuration)?;
                        for target in &intent.targets {
                            if let Some(pre) = &target.pre_image {
                                root.verify_artifact(pre)?;
                            }
                        }
                        undo_sources(intent, state)?;
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

/// The bound plan of a captured mutation must agree with the image the intent
/// binds: same path, same pre-image, and — for a write — the same post-image.
fn plan_image(root: &DataRoot, intent: &ExecutionIntent, state: &State) -> Result<()> {
    let image = intent
        .file_image
        .as_ref()
        .ok_or(StoreError::Invalid("undo_image_binding"))?;
    let plan = state
        .events
        .iter()
        .find_map(|entry| match &entry.2 {
            JournalEvent::AuthorizationDecided(decision)
                if decision.decision_id == intent.decision_id
                    && decision.plan_digest == intent.plan_digest =>
            {
                Some(decision.plan.clone())
            }
            _ => None,
        })
        .ok_or(StoreError::Invalid("undo_image_binding"))?;
    let bytes = root.read_artifact(&plan, codec::MAX_LINE_BYTES)?;
    let value = codec::unique_json(&bytes)?;
    if value["name"].as_str() == Some("multi_edit") {
        let planned = value["targets"]
            .as_array()
            .ok_or(StoreError::Invalid("undo_image_binding"))?;
        let captured: Vec<_> = image.targets().collect();
        if planned.len() != captured.len()
            || planned.iter().zip(&captured).any(|(target, image)| {
                target["path"].as_str() != Some(image.path)
                    || target["pre_image"].as_str()
                        != image.pre_image.as_ref().map(|pre| pre.sha256.as_str())
                    || image.pre_image.is_none()
                    || image.pre_mode.is_none()
            })
        {
            return Err(StoreError::Invalid("undo_image_binding"));
        }
        return Ok(());
    }
    if !image.additional_targets.is_empty() {
        return Err(StoreError::Invalid("undo_image_binding"));
    }
    let target = &value["target"];
    let pre = target["pre_image"].as_str();
    let pre_matches = match (&image.pre_image, pre) {
        (None, Some("absent")) => true,
        (Some(reference), Some(stored)) => stored == reference.sha256.as_str(),
        _ => false,
    };
    let post_matches = match value["name"].as_str() {
        // A write plan carries the digest of exactly the bytes it will write.
        Some("write") => target["content_sha256"].as_str() == Some(image.post_image.as_str()),
        // An edit plan binds literal arguments and a pre-digest only; capture
        // computed the exact replacement with the executor's own rules.
        Some("edit") => true,
        _ => false,
    };
    if target["path"].as_str() != Some(image.path.as_str()) || !pre_matches || !post_matches {
        return Err(StoreError::Invalid("undo_image_binding"));
    }
    Ok(())
}

/// Every target of an undo intent must come from the durable images of its
/// target turn: same path, same first pre-image and same last post-image.
fn undo_sources(intent: &UndoIntent, state: &State) -> Result<()> {
    for target in &intent.targets {
        let mut images = Vec::new();
        for id in &target.invocation_ids {
            let invocation = state
                .invocations
                .get(id)
                .ok_or(StoreError::Invalid("undo_binding"))?;
            let image = invocation
                .image()
                .ok_or(StoreError::Invalid("undo_binding"))?;
            if invocation.turn() != &intent.target_turn_id || !invocation.has_result() {
                return Err(StoreError::Invalid("undo_binding"));
            }
            let Some(image) = image.targets().find(|image| image.path == target.path) else {
                return Err(StoreError::Invalid("undo_binding"));
            };
            images.push((image, invocation));
        }
        let first = images.first().ok_or(StoreError::Invalid("undo_binding"))?;
        let last = images.last().ok_or(StoreError::Invalid("undo_binding"))?;
        if first.0.pre_image != target.pre_image
            || first.0.pre_mode != target.pre_mode
            || last.0.post_image != target.post_image
        {
            return Err(StoreError::Invalid("undo_binding"));
        }
    }
    Ok(())
}
