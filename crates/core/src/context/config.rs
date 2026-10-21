//! What the builder reads besides the snapshot: runtime configuration, model
//! limits, and the decoded session configuration artifact.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{estimate::HeuristicEstimator, projection::Instruction};
use crate::session::{ArtifactRef, DataRoot, Digest, ModelRef, codec};

/// Context limits of one model. `None` means unknown, never zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLimits {
    /// Total context window in tokens.
    pub context_tokens: Option<u32>,
    /// Largest response the model can produce, if known. Informational only:
    /// the budget deducts the configured `output_reserve`.
    pub max_output_tokens: Option<u32>,
}

/// Explicitly configured model limits. A model missing from the catalog has
/// unknown limits, which the core refuses to infer with. The provider is never
/// consulted, which keeps the builder pure.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelCatalog {
    models: BTreeMap<ModelRef, ModelLimits>,
}

impl ModelCatalog {
    /// An empty catalog: every model has unknown limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace one model's limits.
    pub fn insert(&mut self, model: ModelRef, limits: ModelLimits) {
        self.models.insert(model, limits);
    }

    /// Builder form of [`insert`](Self::insert).
    pub fn with(mut self, model: ModelRef, limits: ModelLimits) -> Self {
        self.insert(model, limits);
        self
    }

    /// The configured limits of `model`, if any.
    pub fn limits(&self, model: &ModelRef) -> Option<&ModelLimits> {
        self.models.get(model)
    }
}

/// Runtime configuration of the context builder. Not persisted; a report
/// shows every value it used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextConfig {
    /// Known model limits.
    pub catalog: ModelCatalog,
    /// Tokens kept free for the response.
    pub output_reserve: u32,
    /// Tokens kept free for tool definitions and tool-call overhead.
    pub tool_headroom: u32,
    /// Tokens kept free for estimation error and wire-format overhead.
    pub safety_margin: u32,
    /// Fraction of the usable budget, in thousandths, at or above which the
    /// report recommends compaction. 850 means 0.85. Must be in `1..=1000`.
    pub compaction_trigger_permille: u32,
    /// The default estimator's parameters.
    pub estimator: HeuristicEstimator,
}

impl Default for ContextConfig {
    /// The RFC-001 example configuration's reserves, and a trigger of 0.85.
    fn default() -> Self {
        Self {
            catalog: ModelCatalog::new(),
            output_reserve: 4096,
            tool_headroom: 8192,
            safety_margin: 4096,
            compaction_trigger_permille: 850,
            estimator: HeuristicEstimator::default(),
        }
    }
}

/// Why a configuration artifact cannot be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigurationError {
    /// The artifact could not be read or failed verification.
    #[error("configuration_unreadable: {0}")]
    Unreadable(String),
    /// The bytes do not match the artifact reference.
    #[error("configuration_mismatch")]
    Mismatch,
    /// The artifact is not one JSON object without duplicate keys.
    #[error("configuration_not_an_object")]
    NotAnObject,
    /// `instructions` is present but not an array of instructions.
    #[error("invalid_instructions")]
    InvalidInstructions,
}

impl ConfigurationError {
    /// Stable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unreadable(_) => "configuration_unreadable",
            Self::Mismatch => "configuration_mismatch",
            Self::NotAnObject => "configuration_not_an_object",
            Self::InvalidInstructions => "invalid_instructions",
        }
    }
}

/// The parts of a session's configuration artifact the builder reads, bound
/// to that artifact. The builder refuses one that is not the snapshot's
/// current configuration, so it cannot silently use stale instructions.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionConfiguration {
    artifact: ArtifactRef,
    instructions: Option<Vec<Instruction>>,
}

impl SessionConfiguration {
    /// Largest configuration artifact `load` reads.
    pub const MAX_BYTES: usize = 1024 * 1024;

    /// Decode the exact bytes of `artifact`. The artifact must be one JSON
    /// object. Its optional `instructions` field is an ordered array of
    /// `{"level": "system" | "developer", "text": ...}`; other fields are
    /// ignored here.
    pub fn parse(artifact: ArtifactRef, bytes: &[u8]) -> Result<Self, ConfigurationError> {
        if bytes.len() as u64 != artifact.bytes || Digest::of(bytes) != artifact.sha256 {
            return Err(ConfigurationError::Mismatch);
        }
        let Ok(serde_json::Value::Object(mut object)) = codec::unique_json(bytes) else {
            return Err(ConfigurationError::NotAnObject);
        };
        let instructions = match object.remove("instructions") {
            None => None,
            Some(value) => Some(
                serde_json::from_value(value)
                    .map_err(|_| ConfigurationError::InvalidInstructions)?,
            ),
        };
        Ok(Self {
            artifact,
            instructions,
        })
    }

    /// Read and decode a configuration artifact. This performs I/O; the
    /// builder itself never does.
    pub fn load(root: &DataRoot, artifact: &ArtifactRef) -> Result<Self, ConfigurationError> {
        let bytes = root
            .read_artifact(artifact, Self::MAX_BYTES)
            .map_err(|error| ConfigurationError::Unreadable(error.to_string()))?;
        Self::parse(artifact.clone(), &bytes)
    }

    /// The artifact these values were decoded from.
    pub fn artifact(&self) -> &ArtifactRef {
        &self.artifact
    }

    /// The `instructions` field, or `None` when it is absent.
    pub fn instructions(&self) -> Option<&[Instruction]> {
        self.instructions.as_deref()
    }
}
