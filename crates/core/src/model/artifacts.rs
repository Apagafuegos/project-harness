//! Read-only, request-scoped artifact resolution. The builder performs no I/O.

use std::{future::Future, pin::Pin, sync::Arc};

use super::ModelInput;
use crate::{
    context::ContextItem,
    session::{ArtifactRef, ContentBlock, Digest},
};

type ReadFuture = Pin<Box<dyn Future<Output = Result<Vec<u8>, ArtifactReadError>> + Send>>;
type Reader = dyn Fn(ArtifactRef) -> ReadFuture + Send + Sync;

/// A safe artifact-read failure, without payloads or storage paths.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArtifactReadError {
    /// The exact reference was not in this request's projection.
    #[error("artifact_not_referenced")]
    NotReferenced,
    /// The bytes were unavailable or did not verify against the reference.
    #[error("artifact_unavailable")]
    Unavailable,
}

/// Read-only capability for exactly the artifacts in a model input. Cloning
/// preserves the same allowlist; adapters cannot broaden it or write artifacts.
#[derive(Clone)]
pub struct ArtifactSource {
    references: Vec<ArtifactRef>,
    reader: Arc<Reader>,
}

impl Default for ArtifactSource {
    fn default() -> Self {
        Self {
            references: Vec::new(),
            reader: Arc::new(|_| Box::pin(async { Err(ArtifactReadError::Unavailable) })),
        }
    }
}
impl std::fmt::Debug for ArtifactSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactSource")
            .field("references", &self.references)
            .finish_non_exhaustive()
    }
}
impl PartialEq for ArtifactSource {
    fn eq(&self, other: &Self) -> bool {
        self.references == other.references
    }
}
impl ArtifactSource {
    pub(crate) fn new(
        input: &ModelInput,
        reader: impl Fn(ArtifactRef) -> ReadFuture + Send + Sync + 'static,
    ) -> Self {
        let mut references = Vec::new();
        let mut add = |reference: &ArtifactRef| {
            if !references.contains(reference) {
                references.push(reference.clone());
            }
        };
        for item in &input.items {
            let content = match item {
                ContextItem::User { content, .. } | ContextItem::Assistant { content, .. } => {
                    content
                }
                ContextItem::ToolResult { result, .. } => {
                    for reference in &result.metadata.artifacts {
                        add(reference);
                    }
                    &result.content
                }
                _ => continue,
            };
            for block in content {
                match block {
                    ContentBlock::Image { artifact, .. }
                    | ContentBlock::Audio { artifact, .. }
                    | ContentBlock::File { artifact, .. }
                    | ContentBlock::ProviderState { artifact, .. } => add(artifact),
                    _ => {}
                }
            }
        }
        if let Some(continuation) = &input.continuation {
            add(&continuation.artifact);
        }
        Self {
            references,
            reader: Arc::new(reader),
        }
    }

    /// Exact permitted references, suitable for deterministic adapter tests.
    pub fn references(&self) -> &[ArtifactRef] {
        &self.references
    }

    /// Read and verify one permitted artifact. The driver bounds storage I/O
    /// by the reference's size; this boundary also verifies the returned bytes.
    pub async fn read(&self, reference: &ArtifactRef) -> Result<Vec<u8>, ArtifactReadError> {
        if !self.references.contains(reference) {
            return Err(ArtifactReadError::NotReferenced);
        }
        let bytes = (self.reader)(reference.clone()).await?;
        if bytes.len() as u64 != reference.bytes || Digest::of(&bytes) != reference.sha256 {
            return Err(ArtifactReadError::Unavailable);
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::InferenceOptions, session::ModelRef};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn input(reference: ArtifactRef) -> ModelInput {
        ModelInput {
            model: ModelRef {
                provider_instance: "mock".into(),
                model: "test".into(),
            },
            items: vec![ContextItem::User {
                entry_id: crate::session::EntryId::new(),
                content: vec![ContentBlock::File {
                    artifact: reference,
                    mime_type: "application/octet-stream".into(),
                    name: "fixture".into(),
                }],
            }],
            tools: vec![],
            options: InferenceOptions {
                max_output_tokens: 4096,
                reasoning_effort: None,
                provider_options: Default::default(),
            },
            continuation: None,
        }
    }

    #[tokio::test]
    async fn exact_allowlist_refuses_before_io_and_verifies_size_and_digest() {
        let reference = ArtifactRef {
            sha256: Digest::of(b"right"),
            bytes: 5,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let source = ArtifactSource::new(&input(reference.clone()), move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(b"right".to_vec()) })
        });
        let widened = ArtifactRef {
            bytes: 6,
            ..reference.clone()
        };
        assert_eq!(
            source.read(&widened).await,
            Err(ArtifactReadError::NotReferenced)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(source.read(&reference).await.unwrap(), b"right");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        for returned in [
            b"wrong".to_vec(),
            b"right too large".to_vec(),
            b"r".to_vec(),
        ] {
            let source = ArtifactSource::new(&input(reference.clone()), move |_| {
                let bytes = returned.clone();
                Box::pin(async move { Ok(bytes) })
            });
            assert_eq!(
                source.read(&reference).await,
                Err(ArtifactReadError::Unavailable)
            );
        }
    }
}
