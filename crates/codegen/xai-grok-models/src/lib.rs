//! Default model IDs and built-in third-party platform registry.
//!
//! - xAI defaults live in `default_models.json`
//! - Moonshot open platforms (Phase 1) live in [`platforms`]
//!
//! At runtime each model is resolved via:
//!   CLI flag > ENV var > config.toml > remote settings > these defaults

mod platforms;

pub use platforms::{
    ANTHROPIC_API_KEY_ALIAS_ENV, ANTHROPIC_API_KEY_ENV, ANTHROPIC_AUTH_TOKEN_ENV,
    ANTHROPIC_BASE_URL_ALIAS_ENV, ANTHROPIC_BASE_URL_ENV, ANTHROPIC_VERSION_HEADER_VALUE,
    BuiltinPlatformModel,
    KIMI_CODE_API_BACKEND_ENV, KIMI_CODE_BASE_URL_ENV, KIMI_CODE_OAUTH_HOST_ENV,
    KIMI_DEFAULT_MAX_TOKENS, KimiRequestProfile,
    MOONSHOT_AI_API_KEY_ENV, MOONSHOT_AI_BASE_URL_ENV, MOONSHOT_API_KEY_ALIAS_ENV,
    MOONSHOT_API_KEY_ENV, MOONSHOT_CN_API_KEY_ENV, MOONSHOT_CN_BASE_URL_ENV, ModelCapability,
    OPENAI_API_KEY_ALIAS_ENV, OPENAI_API_KEY_ENV, OPENAI_BASE_URL_ENV, PLATFORM_CATALOG_JSON,
    PlatformApiBackend, PlatformId, WireModel, WireModelsResponse, WireThinkEfforts,
    derive_capabilities, filter_allowed_models, kimi_allow_empty_thinking_signature,
    kimi_force_adaptive_thinking, kimi_request_profile, kimi_sampling_is_fixed,
    moonshot_builtin_models, normalize_kimi_code_base_url, normalize_messages_sdk_base_url,
    parse_managed_model_key, platform_builtin_models,
};

use std::sync::LazyLock;

/// The raw JSON, embedded at compile time. Re-exported through the
/// `xai_grok_shell::models` facade and consumed by `agent::config`, so it must
/// be `pub` (was `pub(crate)` when this lived inside the shell crate).
pub const DEFAULT_MODELS_JSON: &str = include_str!("../default_models.json");

#[derive(serde::Deserialize)]
struct DefaultModels {
    default: String,
    /// Falls back to `default` if not specified in JSON.
    web_search: Option<String>,
    /// Falls back to `default` if not specified in JSON.
    image_description: Option<String>,
    /// Falls back to `default` if not specified in JSON.
    session_summary: Option<String>,
    models: Vec<DefaultModelEntry>,
}

#[derive(serde::Deserialize)]
struct DefaultModelEntry {
    model: String,
}

static DEFAULTS: LazyLock<DefaultModels> = LazyLock::new(|| {
    let defaults: DefaultModels = serde_json::from_str(DEFAULT_MODELS_JSON)
        .expect("default_models.json: invalid JSON or missing 'default' field");

    // Baked-in JSON — a mismatch here is a developer error, not a runtime condition.
    let model_ids: Vec<&str> = defaults.models.iter().map(|m| m.model.as_str()).collect();
    assert!(
        model_ids.contains(&defaults.default.as_str()),
        "default_models.json: 'default' is '{}' but 'models' array only has {model_ids:?}",
        defaults.default,
    );

    defaults
});

/// Primary model for coding tasks and general fallback.
pub fn default_model() -> &'static str {
    &DEFAULTS.default
}

/// Model for web search tool synthesis. Falls back to default model.
pub fn default_web_search_model() -> &'static str {
    DEFAULTS.web_search.as_deref().unwrap_or(&DEFAULTS.default)
}

/// Model for image describe. Falls back to default model.
pub fn default_image_description_model() -> &'static str {
    DEFAULTS
        .image_description
        .as_deref()
        .unwrap_or(&DEFAULTS.default)
}

/// Model for session title generation. Falls back to default model.
pub fn default_session_summary_model() -> &'static str {
    DEFAULTS
        .session_summary
        .as_deref()
        .unwrap_or(&DEFAULTS.default)
}
