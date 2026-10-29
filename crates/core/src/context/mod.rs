//! Contract 5: the pure context builder and its report.
//!
//! Three layers stay separate. History is the append-only journal. The
//! projection is what the model should see, derived purely from one branch of
//! history. A provider request is the projection in one provider's wire
//! format; native adapters arrive later, so the projection stays a
//! [`ModelInput`](crate::model::ModelInput).
//!
//! [`ContextBuilder::build`] walks the branch ending at a head and returns the
//! [`Projection`] and a [`ContextReport`] from the same walk. The report
//! accounts for every entry on that branch exactly once, included or excluded
//! with an [`ExclusionReason`]. Building performs no I/O, reads no clock and
//! never appends; the same snapshot, head, model and configuration always give
//! the same projection and fingerprint. Neither is ever persisted.
//!
//! See `docs/context.md` for the inclusion rules, budget and report fields.
//!
//! ```
//! use std::sync::Arc;
//! use harness_core::{context::*, session::*};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let parent = tempfile::tempdir()?;
//! let root = Arc::new(DataRoot::open(parent.path().join("state"), true)?);
//! let configuration = root.put_artifact(br#"{"instructions":[{"level":"system","text":"Be brief."}]}"#)?;
//! let model = ModelRef { provider_instance: "mock".into(), model: "scripted".into() };
//! let header = SessionHeader {
//!     workspace: "/workspace".into(),
//!     initial_model: model.clone(),
//!     policy_revision: "initial".into(),
//!     configuration: configuration.clone(),
//! };
//! let store = JournalStore::create(root.clone(), SessionId::new(), header, Timestamp::now())?;
//!
//! // Reading the configuration artifact is I/O, done once outside the builder.
//! let session = SessionConfiguration::load(&root, &configuration)?;
//! let mut config = ContextConfig::default();
//! config.catalog.insert(model, ModelLimits { context_tokens: Some(32_000), max_output_tokens: None });
//!
//! let snapshot = store.snapshot();
//! let built = ContextBuilder::new(&config, &session).build(&snapshot, snapshot.head(), None)?;
//! assert!(matches!(built.projection.input.items[..], [ContextItem::Instructions { .. }]));
//! assert_eq!(built.report.fingerprint, built.projection.fingerprint);
//! assert!(!built.report.flags.overflow);
//! # Ok(())
//! # }
//! ```

mod builder;
mod check;
mod config;
mod estimate;
mod projection;
mod report;

pub use builder::{BuiltContext, ContextBuilder, ContextError};
pub use config::{
    ConfigurationError, ContextConfig, InputMediaKind, ModelCapabilities, ModelCatalog, ModelInfo,
    ModelLimits, SessionConfiguration,
};
pub use estimate::{
    Estimate, EstimateMethod, EstimatorInfo, HeuristicEstimator, MediaKind, TokenEstimator,
};
pub use projection::{ContextItem, Instruction, InstructionLevel, Projection, fingerprint};
pub use report::*;

/// Version of the inclusion rules and item shapes, hashed into every
/// fingerprint. Bump it whenever a rule or item changes, so fingerprints of
/// old and new projections can never collide.
pub const PROJECTION_VERSION: u32 = 4;

/// Version of the report and its checks. Bump it when report fields change.
pub const BUILDER_VERSION: u32 = 4;

#[cfg(test)]
mod tests;
