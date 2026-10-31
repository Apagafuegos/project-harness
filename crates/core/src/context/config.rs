//! What the builder reads besides the snapshot: runtime configuration, model
//! limits, and the decoded session configuration artifact.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{estimate::HeuristicEstimator, projection::Instruction};
use crate::model::{InferenceOptions, ReasoningEffort};
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

/// Accepted user input media kinds; text is always supported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputMediaKind {
    /// Image input.
    Image,
    /// Audio input.
    Audio,
    /// File input.
    File,
}

/// Application-supplied capabilities, also constructible by adapter crates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    /// Accepted media. An empty set means text-only.
    pub input_media: std::collections::BTreeSet<InputMediaKind>,
    /// Tool calls and results are accepted.
    pub tools: bool,
    /// Reasoning effort can be requested.
    pub reasoning_effort: bool,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            input_media: Default::default(),
            tools: true,
            reasoning_effort: false,
        }
    }
}

/// Limits and capabilities of one model. Unknown models use this default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Known limits, never guessed from a model name.
    pub limits: ModelLimits,
    /// Accepted features.
    pub capabilities: ModelCapabilities,
}

impl From<ModelLimits> for ModelInfo {
    fn from(limits: ModelLimits) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }
}

/// Explicitly configured model limits. A model missing from the catalog has
/// unknown limits, which the core refuses to infer with. The provider is never
/// consulted, which keeps the builder pure.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelCatalog {
    models: BTreeMap<ModelRef, ModelInfo>,
}

impl ModelCatalog {
    /// An empty catalog: every model has unknown limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace one model's limits.
    pub fn insert(&mut self, model: ModelRef, info: impl Into<ModelInfo>) {
        self.models.insert(model, info.into());
    }

    /// Builder form of [`insert`](Self::insert).
    pub fn with(mut self, model: ModelRef, info: impl Into<ModelInfo>) -> Self {
        self.insert(model, info);
        self
    }

    /// The configured limits of `model`, if any.
    pub fn limits(&self, model: &ModelRef) -> Option<&ModelLimits> {
        self.models.get(model).map(|info| &info.limits)
    }

    /// Configured metadata, if present.
    pub fn info(&self, model: &ModelRef) -> Option<&ModelInfo> {
        self.models.get(model)
    }

    /// Unknown models accept text and tools, with no reasoning control.
    pub fn capabilities(&self, model: &ModelRef) -> ModelCapabilities {
        self.info(model)
            .map(|info| info.capabilities.clone())
            .unwrap_or_default()
    }

    /// Every configured model with its metadata, ordered by provider instance
    /// and then model name. Read-only; clients use it to offer model choices.
    pub fn iter(&self) -> impl Iterator<Item = (&ModelRef, &ModelInfo)> {
        self.models.iter()
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
    /// `inference` must be an object with supported field shapes.
    #[error("invalid_inference")]
    InvalidInference,
    /// `compaction` must have supported fields and valid bounds.
    #[error("invalid_compaction")]
    InvalidCompaction,
}

impl ConfigurationError {
    /// Stable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unreadable(_) => "configuration_unreadable",
            Self::Mismatch => "configuration_mismatch",
            Self::NotAnObject => "configuration_not_an_object",
            Self::InvalidInstructions => "invalid_instructions",
            Self::InvalidInference => "invalid_inference",
            Self::InvalidCompaction => "invalid_compaction",
        }
    }
}

/// The parts of a session's configuration artifact the builder reads, bound
/// to that artifact. The builder refuses one that is not the snapshot's
/// current configuration, so it cannot silently use stale instructions.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionConfiguration {
    artifact: ArtifactRef,
    bytes: Vec<u8>,
    instructions: Option<Vec<Instruction>>,
    inference: ConfiguredInference,
    compaction: CompactionConfig,
}

/// Independently configured summary generation; never defaults to the session model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ConfiguredCompaction")]
pub struct CompactionConfig {
    /// Required for generation, but not for pure selection.
    pub summary_model: Option<ModelRef>,
    /// Maximum estimated summary tokens; defaults to 2048.
    pub max_summary_tokens: u32,
    /// Completed turns left verbatim; at least two, plus any current turn.
    pub retain_recent_turns: usize,
    /// Automatic idle preparation; absent defaults to whether a summary model is set.
    pub automatic: bool,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            summary_model: None,
            max_summary_tokens: 2048,
            retain_recent_turns: 2,
            automatic: false,
        }
    }
}

impl CompactionConfig {
    /// Whether the configured automatic idle trigger is enabled.
    pub fn automatic(&self) -> bool {
        self.automatic
    }

    /// Validate programmatically constructed values as well as decoded values.
    pub fn validate(&self) -> Result<(), ConfigurationError> {
        if (self.automatic() && self.summary_model.is_none())
            || self.retain_recent_turns < 2
            || self.max_summary_tokens == 0
            || self
                .summary_model
                .as_ref()
                .is_some_and(|m| m.provider_instance.is_empty() || m.model.is_empty())
        {
            return Err(ConfigurationError::InvalidCompaction);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConfiguredCompaction {
    summary_model: Option<ModelRef>,
    max_summary_tokens: u32,
    retain_recent_turns: usize,
    automatic: Option<bool>,
}
impl Default for ConfiguredCompaction {
    fn default() -> Self {
        let defaults = CompactionConfig::default();
        Self {
            summary_model: None,
            max_summary_tokens: defaults.max_summary_tokens,
            retain_recent_turns: defaults.retain_recent_turns,
            automatic: None,
        }
    }
}
impl TryFrom<ConfiguredCompaction> for CompactionConfig {
    type Error = ConfigurationError;
    fn try_from(raw: ConfiguredCompaction) -> Result<Self, Self::Error> {
        let config = Self {
            automatic: raw.automatic.unwrap_or(raw.summary_model.is_some()),
            summary_model: raw.summary_model,
            max_summary_tokens: raw.max_summary_tokens,
            retain_recent_turns: raw.retain_recent_turns,
        };
        config.validate()?;
        Ok(config)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredInference {
    #[serde(default)]
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    provider_options: serde_json::Map<String, serde_json::Value>,
}

impl SessionConfiguration {
    /// Largest configuration artifact `load` reads.
    pub const MAX_BYTES: usize = 1024 * 1024;

    /// Decode the exact bytes of `artifact`. The artifact must be one JSON
    /// object. Its optional `instructions` field is an ordered array of
    /// `{"level": "system" | "developer", "text": ...}`. Optional `inference`
    /// holds reasoning effort and namespaced provider option objects. Unrelated
    /// configuration fields are ignored here.
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
        let inference = match object.remove("inference") {
            None => ConfiguredInference::default(),
            Some(value) if value.is_object() => {
                let parsed: ConfiguredInference = serde_json::from_value(value)
                    .map_err(|_| ConfigurationError::InvalidInference)?;
                if parsed
                    .provider_options
                    .iter()
                    .any(|(namespace, value)| namespace.is_empty() || !value.is_object())
                {
                    return Err(ConfigurationError::InvalidInference);
                }
                parsed
            }
            Some(_) => return Err(ConfigurationError::InvalidInference),
        };
        let compaction = match object.remove("compaction") {
            None => CompactionConfig::default(),
            Some(value) if value.is_object() => {
                serde_json::from_value(value).map_err(|_| ConfigurationError::InvalidCompaction)?
            }
            Some(_) => return Err(ConfigurationError::InvalidCompaction),
        };
        compaction.validate()?;
        Ok(Self {
            artifact,
            bytes: bytes.to_vec(),
            instructions,
            inference,
            compaction,
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

    /// Exact verified source bytes, also decoded by the runtime's policy layer.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Build request options using the same output budget as the projection.
    pub fn inference_options(&self, output_reserve: u32) -> InferenceOptions {
        InferenceOptions {
            max_output_tokens: output_reserve,
            reasoning_effort: self.inference.reasoning_effort,
            provider_options: self.inference.provider_options.clone(),
        }
    }

    /// Optional compaction policy, with defaults applied.
    pub fn compaction(&self) -> &CompactionConfig {
        &self.compaction
    }

    /// The `instructions` field, or `None` when it is absent.
    pub fn instructions(&self) -> Option<&[Instruction]> {
        self.instructions.as_deref()
    }
}

#[cfg(test)]
mod catalog_iter_tests {
    use super::*;

    fn model(instance: &str, name: &str) -> ModelRef {
        ModelRef {
            provider_instance: instance.into(),
            model: name.into(),
        }
    }

    #[test]
    fn iter_lists_every_model_in_instance_then_name_order() {
        let limits = |tokens| ModelLimits {
            context_tokens: Some(tokens),
            max_output_tokens: None,
        };
        let catalog = ModelCatalog::new()
            .with(model("openai-codex", "gpt-b"), limits(2))
            .with(model("anthropic", "claude"), limits(3))
            .with(model("openai-codex", "gpt-a"), limits(1));
        let listed: Vec<_> = catalog
            .iter()
            .map(|(model, info)| {
                (
                    model.provider_instance.as_str(),
                    model.model.as_str(),
                    info.limits.context_tokens,
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("anthropic", "claude", Some(3)),
                ("openai-codex", "gpt-a", Some(1)),
                ("openai-codex", "gpt-b", Some(2)),
            ]
        );
        assert_eq!(ModelCatalog::new().iter().count(), 0);
    }
}
