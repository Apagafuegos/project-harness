//! Token estimates. Every estimate names how it was made.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How a token count was obtained. Provider-reported usage is never an
/// estimate and is reported separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EstimateMethod {
    /// A byte-count heuristic, not a tokenizer.
    Heuristic,
}

/// One estimated token count and its method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Estimate {
    /// Estimated tokens.
    pub tokens: u64,
    /// How they were estimated.
    pub method: EstimateMethod,
}

/// Media kinds with a flat configured estimate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    /// Image block.
    Image,
    /// Audio block.
    Audio,
    /// File block.
    File,
}

/// Identity and parameters of an estimator, shown in every report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EstimatorInfo {
    /// Method of every estimate it makes.
    pub method: EstimateMethod,
    /// Changes whenever its arithmetic changes.
    pub version: String,
    /// Configured parameters by name.
    pub parameters: BTreeMap<String, u64>,
}

/// Estimates tokens of projected content. Implementations are pure. A model
/// tokenizer can implement this later with another method.
pub trait TokenEstimator {
    /// Method of every estimate this estimator returns.
    fn method(&self) -> EstimateMethod;
    /// Identity and parameters for the report.
    fn info(&self) -> EstimatorInfo;
    /// Tokens of a piece of text.
    fn text(&self, text: &str) -> Estimate;
    /// Tokens of one media block.
    fn media(&self, kind: MediaKind) -> Estimate;
}

/// The default estimator: text is `ceil(utf8_bytes / 4)`; each media block
/// costs a configured flat value for its kind. Opaque provider state is never
/// estimated; the builder counts its bytes instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeuristicEstimator {
    /// Tokens per image block.
    pub image_tokens: u32,
    /// Tokens per audio block.
    pub audio_tokens: u32,
    /// Tokens per file block.
    pub file_tokens: u32,
}

impl HeuristicEstimator {
    /// UTF-8 bytes per estimated text token.
    pub const BYTES_PER_TOKEN: u64 = 4;
    /// Version of the arithmetic above.
    pub const VERSION: &'static str = "bytes4-flat-media-v1";
}

impl Default for HeuristicEstimator {
    /// Placeholder flat values, chosen to err on the high side. Configure
    /// them for the models in use.
    fn default() -> Self {
        Self {
            image_tokens: 1600,
            audio_tokens: 2000,
            file_tokens: 4000,
        }
    }
}

impl TokenEstimator for HeuristicEstimator {
    fn method(&self) -> EstimateMethod {
        EstimateMethod::Heuristic
    }

    fn info(&self) -> EstimatorInfo {
        EstimatorInfo {
            method: self.method(),
            version: Self::VERSION.into(),
            parameters: BTreeMap::from([
                ("text_bytes_per_token".into(), Self::BYTES_PER_TOKEN),
                ("image_tokens".into(), self.image_tokens.into()),
                ("audio_tokens".into(), self.audio_tokens.into()),
                ("file_tokens".into(), self.file_tokens.into()),
            ]),
        }
    }

    fn text(&self, text: &str) -> Estimate {
        Estimate {
            tokens: (text.len() as u64).div_ceil(Self::BYTES_PER_TOKEN),
            method: self.method(),
        }
    }

    fn media(&self, kind: MediaKind) -> Estimate {
        let tokens = match kind {
            MediaKind::Image => self.image_tokens,
            MediaKind::Audio => self.audio_tokens,
            MediaKind::File => self.file_tokens,
        };
        Estimate {
            tokens: tokens.into(),
            method: self.method(),
        }
    }
}
