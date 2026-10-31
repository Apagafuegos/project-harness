//! Sourced catalog limits and capabilities for the Claude and DeepSeek families.
//!
//! Every number is transcribed from a cited source in
//! `docs/decisions/0018-anthropic-adapter.md` (Claude) and
//! `docs/decisions/0024-deepseek-profile.md` (DeepSeek). A limit that could not
//! be sourced stays `None` rather than becoming a guess.

use std::collections::BTreeSet;

use harness_core::{
    context::{InputMediaKind, ModelCapabilities, ModelCatalog, ModelInfo, ModelLimits},
    session::ModelRef,
};

use super::Profile;

/// Default model for the opt-in Anthropic live check
/// (`HARNESS_LIVE_ANTHROPIC_MODEL`).
pub const LIVE_CHECK_MODEL: &str = "claude-haiku-5-5";

/// Default model for the opt-in DeepSeek live check
/// (`HARNESS_LIVE_DEEPSEEK_MODEL`).
pub const DEEPSEEK_LIVE_CHECK_MODEL: &str = "deepseek-flash";

/// Conservative `max_output_tokens` for a DeepSeek request.
///
/// Both DeepSeek models document a 384K maximum output, and the harness's
/// default output reserve is 4096 tokens, so the reserve fits both with a wide
/// margin. The constant exists so callers that build a request themselves (the
/// live check) use one justified number instead of the ceiling.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;

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
const CLAUDE_ENTRIES: &[Entry] = &[
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

/// DeepSeek rows, from the official models and pricing page (read 2026-10-31):
/// 1M-token context and a 384K maximum output for both models, thinking mode on
/// by default with an effort control, and tool calls. Vision is listed for
/// `deepseek-flash` only, and the compatibility page limits its images to user
/// messages and tool results (an assistant image is a 400), which is exactly the
/// user-side capability modelled here.
const DEEPSEEK_ENTRIES: &[Entry] = &[
    Entry {
        model: "deepseek-flash",
        context_tokens: Some(1_000_000),
        max_output_tokens: Some(384_000),
        images: true,
        reasoning: true,
    },
    Entry {
        model: "deepseek-v4-pro",
        context_tokens: Some(1_000_000),
        max_output_tokens: Some(384_000),
        images: false,
        reasoning: true,
    },
];

fn entries(profile: Profile) -> &'static [Entry] {
    match profile {
        Profile::Anthropic => CLAUDE_ENTRIES,
        Profile::DeepSeek => DEEPSEEK_ENTRIES,
    }
}

fn entry(profile: Profile, model: &str) -> Option<&'static Entry> {
    entries(profile).iter().find(|entry| entry.model == model)
}

/// Capabilities of `model` on `profile`. An uncatalogued model is text-only with
/// tools and no reasoning control, matching the core's unknown-model default.
pub fn capabilities(profile: Profile, model: &str) -> ModelCapabilities {
    let Some(entry) = entry(profile, model) else {
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

/// Sourced metadata for `model` on `profile`, if catalogued.
pub fn info(profile: Profile, model: &str) -> Option<ModelInfo> {
    let entry = entry(profile, model)?;
    Some(ModelInfo {
        limits: ModelLimits {
            context_tokens: entry.context_tokens,
            max_output_tokens: entry.max_output_tokens,
        },
        capabilities: capabilities(profile, model),
    })
}

/// A catalog holding every sourced row of `profile`, keyed under the
/// caller-chosen provider instance name.
pub fn catalog(profile: Profile, provider_instance: &str) -> ModelCatalog {
    let mut catalog = ModelCatalog::new();
    extend_catalog(&mut catalog, profile, provider_instance);
    catalog
}

/// Add every sourced row of `profile` to an existing catalog, under
/// `provider_instance`.
///
/// A session may use several provider instances (and switch between them), so
/// the runtime needs one catalog holding each registered instance's rows. The
/// core's `ModelCatalog` has no merge, so each adapter adds its own rows.
pub fn extend_catalog(catalog: &mut ModelCatalog, profile: Profile, provider_instance: &str) {
    for entry in entries(profile) {
        let Some(metadata) = info(profile, entry.model) else {
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
}
