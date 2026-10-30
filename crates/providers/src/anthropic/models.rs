//! Sourced catalog limits and capabilities for the current Claude family.
//!
//! Every number is transcribed from a cited source in
//! `docs/decisions/0018-anthropic-adapter.md`. A limit that could not be
//! sourced stays `None` rather than becoming a guess.

use std::collections::BTreeSet;

use harness_core::{
    context::{InputMediaKind, ModelCapabilities, ModelCatalog, ModelInfo, ModelLimits},
    session::ModelRef,
};

/// Default model for the opt-in live check (`HARNESS_LIVE_ANTHROPIC_MODEL`).
pub const LIVE_CHECK_MODEL: &str = "claude-haiku-5-5";

struct Entry {
    model: &'static str,
    /// Total context window in tokens, when sourced.
    context_tokens: Option<u32>,
    /// Largest response the model can produce, when sourced.
    max_output_tokens: Option<u32>,
    /// Accepts image input.
    images: bool,
    /// Accepts an effort control (adaptive thinking).
    reasoning: bool,
}

/// Claude 5.5 family rows, from the official models overview (read
/// 2026-10-30): 1M-token context and 128K-token maximum output for all three,
/// text and image input, adaptive thinking with an effort control, tool use.
const ENTRIES: &[Entry] = &[
    Entry {
        model: "claude-haiku-5-5",
        context_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        images: true,
        reasoning: true,
    },
    Entry {
        model: "claude-sonnet-5-5",
        context_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        images: true,
        reasoning: true,
    },
    Entry {
        model: "claude-opus-5-5",
        context_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        images: true,
        reasoning: true,
    },
];

fn entry(model: &str) -> Option<&'static Entry> {
    ENTRIES.iter().find(|entry| entry.model == model)
}

/// Capabilities of `model`. An uncatalogued model is text-only with tools and
/// no reasoning control, matching the core's unknown-model default.
pub fn capabilities(model: &str) -> ModelCapabilities {
    let Some(entry) = entry(model) else {
        return ModelCapabilities::default();
    };
    let mut input_media = BTreeSet::new();
    if entry.images {
        input_media.insert(InputMediaKind::Image);
    }
    ModelCapabilities {
        input_media,
        tools: true,
        reasoning_effort: entry.reasoning,
    }
}

/// Sourced metadata for `model`, if catalogued.
pub fn info(model: &str) -> Option<ModelInfo> {
    let entry = entry(model)?;
    Some(ModelInfo {
        limits: ModelLimits {
            context_tokens: entry.context_tokens,
            max_output_tokens: entry.max_output_tokens,
        },
        capabilities: capabilities(model),
    })
}

/// A catalog holding every sourced row, keyed under the caller-chosen
/// provider instance name.
pub fn catalog(provider_instance: &str) -> ModelCatalog {
    let mut catalog = ModelCatalog::new();
    for entry in ENTRIES {
        let Some(metadata) = info(entry.model) else {
            continue;
        };
        catalog.insert(
            ModelRef {
                provider_instance: provider_instance.to_owned(),
                model: entry.model.to_owned(),
            },
            metadata,
        );
    }
    catalog
}
