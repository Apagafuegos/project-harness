//! Sourced model limits and capabilities for both OpenAI endpoint profiles.
//!
//! Every number here is transcribed from a cited source in
//! `docs/decisions/0017-openai-adapter.md`. A limit that could not be sourced
//! stays `None` rather than becoming a guess; the context builder refuses to
//! infer from an unknown context window.

use std::collections::BTreeSet;

use harness_core::context::{
    InputMediaKind, ModelCapabilities, ModelCatalog, ModelInfo, ModelLimits,
};

use super::Profile;

/// Default model for the opt-in live check.
///
/// Account-served model from `~/.codex/models_cache.json`, fetched at
/// `2026-10-30T12:51:51.951342245Z` by codex-cli `0.166.0` (not the repo's
/// bundled catalog). Override with `HARNESS_LIVE_OPENAI_MODEL`.
pub const LIVE_CHECK_MODEL: &str = "gpt-6.1-sol";

/// One sourced catalog row. Profile-specific because the ChatGPT backend
/// serves a smaller context window for the same model name.
struct Entry {
    /// Model identifier sent on the wire.
    model: &'static str,
    /// Total context window in tokens, when sourced.
    context_tokens: Option<u32>,
    /// Largest response the model can produce, when sourced.
    max_output_tokens: Option<u32>,
    /// Accepts image input.
    images: bool,
    /// Accepts a reasoning-effort control.
    reasoning: bool,
}

/// Platform (`api.openai.com`) rows: limits from the platform model pages.
const PLATFORM: &[Entry] = &[Entry {
    model: "gpt-5.5",
    // developers.openai.com/api/docs/models/gpt-5.5: "1,050,000 context window".
    context_tokens: Some(1_050_000),
    // Same page: "128,000 max output tokens".
    max_output_tokens: Some(128_000),
    // Same page: input modalities "Text, Image"; reasoning effort supported.
    images: true,
    reasoning: true,
}];

/// ChatGPT Codex rows: account-served models from the CLI's fetched cache.
const CHATGPT: &[Entry] = &[Entry {
    model: LIVE_CHECK_MODEL,
    // ~/.codex/models_cache.json, fetched_at 2026-10-30T12:51:51.951342245Z,
    // client_version 0.166.0: gpt-6.1-sol has "context_window": 500000.
    context_tokens: Some(500_000),
    // The catalog carries no output limit for this model, and the backend
    // accepts no output-token field, so this stays unknown.
    max_output_tokens: None,
    // Same cache: "input_modalities": ["text", "image"], supported reasoning
    // levels low, medium, high, xhigh, ultra. Portable low/medium/high map
    // identically to those backend strings (see request::build).
    images: true,
    reasoning: true,
}];

fn entries(profile: Profile) -> &'static [Entry] {
    match profile {
        Profile::Platform => PLATFORM,
        Profile::ChatGptCodex => CHATGPT,
    }
}

fn entry(profile: Profile, model: &str) -> Option<&'static Entry> {
    entries(profile).iter().find(|entry| entry.model == model)
}

/// Capabilities of `model` on `profile`. An uncatalogued model is text-only
/// with tools and no reasoning control, matching the core's unknown-model
/// default, so the adapter never silently offers an unverifiable control.
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

/// A catalog holding every sourced row for `profile`, keyed under the
/// caller-chosen provider instance name. Applications pass this to their
/// `ContextConfig` so the context builder and the adapter agree.
pub fn catalog(profile: Profile, provider_instance: &str) -> ModelCatalog {
    let mut catalog = ModelCatalog::new();
    extend_catalog(&mut catalog, profile, provider_instance);
    catalog
}

/// Add every sourced row for `profile` to an existing catalog.
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
            harness_core::session::ModelRef {
                provider_instance: provider_instance.to_owned(),
                model: entry.model.to_owned(),
            },
            metadata,
        );
    }
}
