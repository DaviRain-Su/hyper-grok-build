//! Built-in third-party platform registry.
//!
//! Phase 1: Moonshot open platforms (API key).
//! Phase 2: Kimi Code subscription (device OAuth).
//! Phase 3: OpenAI + Anthropic (API key; catalog from Pi models.generated).

use std::num::NonZeroU64;
use std::sync::LazyLock;

/// Env var for the moonshot.cn API key (wins over the generic name).
pub const MOONSHOT_CN_API_KEY_ENV: &str = "GROK_MOONSHOT_CN_API_KEY";
/// Env var for the moonshot.ai API key (wins over the generic name).
pub const MOONSHOT_AI_API_KEY_ENV: &str = "GROK_MOONSHOT_AI_API_KEY";
/// Generic Moonshot API key applied to both open platforms when the
/// platform-scoped name is unset. Also accepts the common `MOONSHOT_API_KEY`
/// alias used by Moonshot docs.
pub const MOONSHOT_API_KEY_ENV: &str = "GROK_MOONSHOT_API_KEY";
/// Common third-party alias (Moonshot open-platform docs).
pub const MOONSHOT_API_KEY_ALIAS_ENV: &str = "MOONSHOT_API_KEY";

/// Env overrides for Moonshot base URLs (dev/test only).
pub const MOONSHOT_CN_BASE_URL_ENV: &str = "GROK_MOONSHOT_CN_BASE_URL";
pub const MOONSHOT_AI_BASE_URL_ENV: &str = "GROK_MOONSHOT_AI_BASE_URL";

/// Env override for the Kimi Code subscription inference base.
pub const KIMI_CODE_BASE_URL_ENV: &str = "GROK_KIMI_CODE_BASE_URL";
/// Env override for the Kimi Code OAuth host.
pub const KIMI_CODE_OAUTH_HOST_ENV: &str = "GROK_KIMI_CODE_OAUTH_HOST";

/// OpenAI API key (platform-scoped, wins over `OPENAI_API_KEY`).
pub const OPENAI_API_KEY_ENV: &str = "GROK_OPENAI_API_KEY";
/// Common OpenAI SDK alias.
pub const OPENAI_API_KEY_ALIAS_ENV: &str = "OPENAI_API_KEY";
pub const OPENAI_BASE_URL_ENV: &str = "GROK_OPENAI_BASE_URL";

/// Anthropic API key (platform-scoped).
pub const ANTHROPIC_API_KEY_ENV: &str = "GROK_ANTHROPIC_API_KEY";
/// Common Anthropic aliases used by Claude Code / Pi.
pub const ANTHROPIC_API_KEY_ALIAS_ENV: &str = "ANTHROPIC_API_KEY";
pub const ANTHROPIC_AUTH_TOKEN_ENV: &str = "ANTHROPIC_AUTH_TOKEN";
pub const ANTHROPIC_BASE_URL_ENV: &str = "GROK_ANTHROPIC_BASE_URL";

const MOONSHOT_CN_BASE_URL_DEFAULT: &str = "https://api.moonshot.cn/v1";
const MOONSHOT_AI_BASE_URL_DEFAULT: &str = "https://api.moonshot.ai/v1";
/// Kimi Code subscription base for Grok's HTTP client.
///
/// Official Pi stores `https://api.kimi.com/coding` and lets the Anthropic SDK
/// append `/v1/messages`. Grok's sampler joins `{base}/messages`, so the base
/// must include `/v1` (same pattern as Anthropic's `…/v1`). Override with
/// `GROK_KIMI_CODE_BASE_URL`.
const KIMI_CODE_BASE_URL_DEFAULT: &str = "https://api.kimi.com/coding/v1";
const KIMI_CODE_OAUTH_HOST_DEFAULT: &str = "https://auth.kimi.com";
const OPENAI_BASE_URL_DEFAULT: &str = "https://api.openai.com/v1";
const ANTHROPIC_BASE_URL_DEFAULT: &str = "https://api.anthropic.com/v1";
/// Required Anthropic Messages API version header (also sent for Kimi Code).
pub const ANTHROPIC_VERSION_HEADER_VALUE: &str = "2023-06-01";

fn env_or(var: &str, compiled: &str) -> String {
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => compiled.to_string(),
    }
}

/// Grok's sampler joins `{base}/messages`. Official Pi stores
/// `https://api.kimi.com/coding` and lets the Anthropic SDK append `/v1/messages`.
/// Accept both shapes so `GROK_KIMI_CODE_BASE_URL=…/coding` does not 404 as
/// `…/coding/messages` (`resource_not_found_error`).
pub fn normalize_kimi_code_base_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return KIMI_CODE_BASE_URL_DEFAULT.to_string();
    }
    // Pi / Anthropic-SDK style base ends at `/coding` — add `/v1` for Grok.
    if trimmed.ends_with("/coding") {
        return format!("{trimmed}/v1");
    }
    trimmed.to_string()
}

/// Built-in inference platforms (aligned with official Pi `@earendil-works/pi-ai`
/// provider ids where applicable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PlatformId {
    KimiCode,
    MoonshotCn,
    MoonshotAi,
    OpenAi,
    Anthropic,
    DeepSeek,
    Groq,
    /// Reserved; Pi Mistral uses a proprietary API we do not speak yet.
    Mistral,
    XaiDirect,
    Together,
    Fireworks,
    Cerebras,
    Nvidia,
    OpenRouter,
    MiniMax,
    MiniMaxCn,
    Zai,
    ZaiCodingCn,
    Ollama,
}

impl PlatformId {
    /// All platforms; subscription first.
    pub const ALL: [PlatformId; 19] = [
        Self::KimiCode,
        Self::MoonshotCn,
        Self::MoonshotAi,
        Self::OpenAi,
        Self::Anthropic,
        Self::DeepSeek,
        Self::Groq,
        Self::Mistral,
        Self::XaiDirect,
        Self::Together,
        Self::Fireworks,
        Self::Cerebras,
        Self::Nvidia,
        Self::OpenRouter,
        Self::MiniMax,
        Self::MiniMaxCn,
        Self::Zai,
        Self::ZaiCodingCn,
        Self::Ollama,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::KimiCode => "kimi-code",
            Self::MoonshotCn => "moonshot-cn",
            Self::MoonshotAi => "moonshot-ai",
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::DeepSeek => "deepseek",
            Self::Groq => "groq",
            Self::Mistral => "mistral",
            Self::XaiDirect => "xai-direct",
            Self::Together => "together",
            Self::Fireworks => "fireworks",
            Self::Cerebras => "cerebras",
            Self::Nvidia => "nvidia",
            Self::OpenRouter => "openrouter",
            Self::MiniMax => "minimax",
            Self::MiniMaxCn => "minimax-cn",
            Self::Zai => "zai",
            Self::ZaiCodingCn => "zai-coding-cn",
            Self::Ollama => "ollama",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "kimi-code" | "kimi-coding" => Some(Self::KimiCode),
            "moonshot-cn" | "moonshotai-cn" => Some(Self::MoonshotCn),
            "moonshot-ai" | "moonshotai" => Some(Self::MoonshotAi),
            "openai" => Some(Self::OpenAi),
            "anthropic" => Some(Self::Anthropic),
            "deepseek" => Some(Self::DeepSeek),
            "groq" => Some(Self::Groq),
            "mistral" => Some(Self::Mistral),
            "xai-direct" | "xai" => Some(Self::XaiDirect),
            "together" => Some(Self::Together),
            "fireworks" => Some(Self::Fireworks),
            "cerebras" => Some(Self::Cerebras),
            "nvidia" => Some(Self::Nvidia),
            "openrouter" => Some(Self::OpenRouter),
            "minimax" => Some(Self::MiniMax),
            "minimax-cn" => Some(Self::MiniMaxCn),
            "zai" => Some(Self::Zai),
            "zai-coding-cn" => Some(Self::ZaiCodingCn),
            "ollama" => Some(Self::Ollama),
            _ => None,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::KimiCode => "Kimi For Coding",
            Self::MoonshotCn => "Moonshot AI (moonshot.cn)",
            Self::MoonshotAi => "Moonshot AI (moonshot.ai)",
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::DeepSeek => "DeepSeek",
            Self::Groq => "Groq",
            Self::Mistral => "Mistral",
            Self::XaiDirect => "xAI (direct API key)",
            Self::Together => "Together AI",
            Self::Fireworks => "Fireworks",
            Self::Cerebras => "Cerebras",
            Self::Nvidia => "NVIDIA NIM",
            Self::OpenRouter => "OpenRouter",
            Self::MiniMax => "MiniMax",
            Self::MiniMaxCn => "MiniMax (China)",
            Self::Zai => "Z.AI",
            Self::ZaiCodingCn => "Z.AI Coding Plan (CN)",
            Self::Ollama => "Ollama Cloud",
        }
    }

    /// Compiled-in default base (overridable via `GROK_*_BASE_URL` env).
    fn default_base_url(self) -> &'static str {
        match self {
            // Kimi Code subscription: https://api.kimi.com/coding/v1.
            Self::KimiCode => KIMI_CODE_BASE_URL_DEFAULT,
            Self::MoonshotCn => MOONSHOT_CN_BASE_URL_DEFAULT,
            Self::MoonshotAi => MOONSHOT_AI_BASE_URL_DEFAULT,
            Self::OpenAi => OPENAI_BASE_URL_DEFAULT,
            Self::Anthropic => ANTHROPIC_BASE_URL_DEFAULT,
            Self::DeepSeek => "https://api.deepseek.com",
            Self::Groq => "https://api.groq.com/openai/v1",
            Self::Mistral => "https://api.mistral.ai/v1",
            Self::XaiDirect => "https://api.x.ai/v1",
            Self::Together => "https://api.together.xyz/v1",
            Self::Fireworks => "https://api.fireworks.ai/inference/v1",
            Self::Cerebras => "https://api.cerebras.ai/v1",
            Self::Nvidia => "https://integrate.api.nvidia.com/v1",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
            Self::MiniMax => "https://api.minimax.io/v1",
            Self::MiniMaxCn => "https://api.minimaxi.com/v1",
            Self::Zai => "https://api.z.ai/api/paas/v4",
            Self::ZaiCodingCn => "https://open.bigmodel.cn/api/coding/paas/v4",
            Self::Ollama => "https://ollama.com/v1",
        }
    }

    /// Inference / model-list base URL.
    pub fn base_url(self) -> String {
        // Prefer well-known envs for core platforms; generic GROK_{ID}_BASE_URL for others.
        let specific = match self {
            Self::KimiCode => Some(KIMI_CODE_BASE_URL_ENV),
            Self::MoonshotCn => Some(MOONSHOT_CN_BASE_URL_ENV),
            Self::MoonshotAi => Some(MOONSHOT_AI_BASE_URL_ENV),
            Self::OpenAi => Some(OPENAI_BASE_URL_ENV),
            Self::Anthropic => Some(ANTHROPIC_BASE_URL_ENV),
            _ => None,
        };
        let raw = if let Some(var) = specific {
            env_or(var, self.default_base_url())
        } else {
            let generic = format!(
                "GROK_{}_BASE_URL",
                self.as_str().replace('-', "_").to_ascii_uppercase()
            );
            match std::env::var(&generic) {
                Ok(v) if !v.trim().is_empty() => v,
                _ => self.default_base_url().to_string(),
            }
        };
        if self == Self::KimiCode {
            normalize_kimi_code_base_url(&raw)
        } else {
            raw
        }
    }

    /// OAuth host for the subscription channel only.
    pub fn oauth_host(self) -> Option<String> {
        match self {
            Self::KimiCode => Some(env_or(KIMI_CODE_OAUTH_HOST_ENV, KIMI_CODE_OAUTH_HOST_DEFAULT)),
            _ => None,
        }
    }

    /// True for the OAuth-bearer subscription channel.
    pub fn uses_oauth(self) -> bool {
        matches!(self, Self::KimiCode)
    }

    /// Anthropic Messages uses `x-api-key` rather than Bearer.
    pub fn uses_x_api_key(self) -> bool {
        matches!(self, Self::Anthropic)
    }

    /// Model-id prefixes admitted from this platform's `/models` listing.
    /// `None` = no filtering.
    pub fn allowed_model_prefixes(self) -> Option<&'static [&'static str]> {
        match self {
            Self::KimiCode => None,
            Self::MoonshotCn | Self::MoonshotAi => Some(&["kimi-k", "kimi-k3", "k3", "k2p7"]),
            _ => None,
        }
    }

    /// Env var names holding this platform's API key (open platforms only).
    /// Empty for the OAuth channel.
    ///
    /// SECURITY: the *values* behind these names must never be logged.
    pub fn api_key_env_names(self) -> &'static [&'static str] {
        match self {
            Self::KimiCode => &[],
            Self::MoonshotCn => &[
                MOONSHOT_CN_API_KEY_ENV,
                MOONSHOT_API_KEY_ENV,
                MOONSHOT_API_KEY_ALIAS_ENV,
            ],
            Self::MoonshotAi => &[
                MOONSHOT_AI_API_KEY_ENV,
                MOONSHOT_API_KEY_ENV,
                MOONSHOT_API_KEY_ALIAS_ENV,
            ],
            Self::OpenAi => &[OPENAI_API_KEY_ENV, OPENAI_API_KEY_ALIAS_ENV],
            Self::Anthropic => &[
                ANTHROPIC_API_KEY_ENV,
                ANTHROPIC_API_KEY_ALIAS_ENV,
                ANTHROPIC_AUTH_TOKEN_ENV,
            ],
            Self::DeepSeek => &["GROK_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"],
            Self::Groq => &["GROK_GROQ_API_KEY", "GROQ_API_KEY"],
            Self::Mistral => &["GROK_MISTRAL_API_KEY", "MISTRAL_API_KEY"],
            Self::XaiDirect => &["GROK_XAI_DIRECT_API_KEY", "XAI_API_KEY"],
            Self::Together => &["GROK_TOGETHER_API_KEY", "TOGETHER_API_KEY"],
            Self::Fireworks => &["GROK_FIREWORKS_API_KEY", "FIREWORKS_API_KEY"],
            Self::Cerebras => &["GROK_CEREBRAS_API_KEY", "CEREBRAS_API_KEY"],
            Self::Nvidia => &["GROK_NVIDIA_API_KEY", "NVIDIA_API_KEY"],
            Self::OpenRouter => &["GROK_OPENROUTER_API_KEY", "OPENROUTER_API_KEY"],
            Self::MiniMax => &["GROK_MINIMAX_API_KEY", "MINIMAX_API_KEY"],
            Self::MiniMaxCn => &["GROK_MINIMAX_CN_API_KEY", "MINIMAX_API_KEY"],
            Self::Zai => &["GROK_ZAI_API_KEY", "ZAI_API_KEY"],
            Self::ZaiCodingCn => &["GROK_ZAI_CODING_CN_API_KEY", "ZAI_API_KEY"],
            Self::Ollama => &["GROK_OLLAMA_API_KEY", "OLLAMA_API_KEY"],
        }
    }

    /// `{base}/models` URL for catalog sync.
    pub fn models_list_url(self) -> String {
        let base = self.base_url().trim_end_matches('/').to_string();
        format!("{base}/models")
    }

    /// Human setup instructions for enabling this platform (no secrets).
    ///
    /// Shown wherever a locked (credential-less) platform model surfaces:
    /// the model picker description, `set_session_model` rejections, and
    /// the pager's `/providers` overview.
    pub fn setup_hint(self) -> String {
        if self.uses_oauth() {
            return format!(
                "Sign in with your {} subscription: run /login kimi",
                self.display_name()
            );
        }
        let envs = self.api_key_env_names();
        let env_part = match envs {
            [] => String::new(),
            [one] => format!("export {one}=<key>"),
            [first, rest @ ..] => format!("export {first}=<key> (or {})", rest.join(" / ")),
        };
        let ui_part = format!("run /providers {} <api_key>", self.as_str());
        let config_part = format!(
            "add `api_key = \"<key>\"` under `[platforms.{}]` in ~/.grok/config.toml",
            self.as_str()
        );
        if env_part.is_empty() {
            format!("{ui_part}, or {config_part}")
        } else {
            format!("{ui_part}, or {env_part}, or {config_part}")
        }
    }

    /// Whether to auto-fetch live `GET /models` for this platform.
    ///
    /// Kimi / Moonshot / Ollama Cloud auto-sync; others use the Pi offline
    /// catalog (org listings are huge / noisy).
    pub fn live_models_list_enabled(self) -> bool {
        matches!(
            self,
            Self::KimiCode | Self::MoonshotCn | Self::MoonshotAi | Self::Ollama
        )
    }

    /// Managed catalog key: `{platform_id}/{model_id}`.
    pub fn managed_model_key(self, model_id: &str) -> String {
        format!("{}/{model_id}", self.as_str())
    }

    /// Whether `url` is this platform's inference base (scheme+host match).
    pub fn base_url_matches(self, url: &str) -> bool {
        let base = self.base_url();
        urls_same_origin(&base, url)
    }
}

fn urls_same_origin(a: &str, b: &str) -> bool {
    fn host_key(u: &str) -> Option<String> {
        let u = u.trim().trim_end_matches('/');
        let rest = u
            .strip_prefix("https://")
            .or_else(|| u.strip_prefix("http://"))?;
        let host = rest.split('/').next()?.to_ascii_lowercase();
        Some(host)
    }
    match (host_key(a), host_key(b)) {
        (Some(ha), Some(hb)) => ha == hb,
        _ => {
            let na = a.trim().trim_end_matches('/').to_ascii_lowercase();
            let nb = b.trim().trim_end_matches('/').to_ascii_lowercase();
            !na.is_empty() && na == nb
        }
    }
}

/// Split `{platform_id}/{model_id}` back into platform + bare model id.
pub fn parse_managed_model_key(key: &str) -> Option<(PlatformId, &str)> {
    let (platform, model_id) = key.split_once('/')?;
    let platform = PlatformId::parse(platform)?;
    if model_id.is_empty() {
        return None;
    }
    Some((platform, model_id))
}

/// Wire API backend for a built-in catalog entry (maps to shell `ApiBackend`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformApiBackend {
    ChatCompletions,
    Responses,
    Messages,
}

impl PlatformApiBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::Messages => "messages",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "chat_completions" | "chat-completions" => Some(Self::ChatCompletions),
            "responses" => Some(Self::Responses),
            "messages" => Some(Self::Messages),
            _ => None,
        }
    }
}

/// One built-in offline catalog entry for a platform.
///
/// Source of truth for open-platform ids: platform.kimi.ai `/docs/models`
/// (2026-07): `kimi-k3`, `kimi-k2.7-code`, `kimi-k2.7-code-highspeed`,
/// `kimi-k2.6`, `kimi-k2.5`. Deprecated `kimi-k2-*-preview` / thinking-turbo
/// are kept only as last-resort aliases until live `/models` replaces them.
///
/// OpenAI / Anthropic entries are loaded from [`PLATFORM_CATALOG_JSON`]
/// (curated from Pi `models.generated.ts`).
#[derive(Debug, Clone)]
pub struct BuiltinPlatformModel {
    pub platform: PlatformId,
    pub model: String,
    pub name: String,
    pub description: String,
    pub context_window: u64,
    pub supports_reasoning_effort: bool,
    /// When false, only OAuth session users see this in the picker.
    pub supported_in_api: bool,
    /// Recommended `max_tokens` / max_completion_tokens (Kimi docs: 32k for
    /// coding thinking models).
    pub max_completion_tokens: Option<u32>,
    pub api_backend: PlatformApiBackend,
}

impl BuiltinPlatformModel {
    pub fn catalog_key(&self) -> String {
        self.platform.managed_model_key(&self.model)
    }

    pub fn context_window_nonzero(&self) -> NonZeroU64 {
        NonZeroU64::new(self.context_window).expect("builtin context_window is non-zero")
    }
}

const CTX_256K: u64 = 262_144;
const CTX_1M: u64 = 1_048_576;
const MAX_TOK_32K: Option<u32> = Some(32_768);

/// Embedded Pi-curated OpenAI/Anthropic catalog (offline). See
/// `platform_catalog.json` header for source.
pub const PLATFORM_CATALOG_JSON: &str = include_str!("../platform_catalog.json");

#[derive(serde::Deserialize)]
struct CatalogFile {
    models: Vec<CatalogModelRow>,
}

#[derive(serde::Deserialize)]
struct CatalogModelRow {
    platform: String,
    model: String,
    name: String,
    description: String,
    context_window: u64,
    max_completion_tokens: Option<u32>,
    api_backend: String,
    supports_reasoning_effort: bool,
    // `supported_in_api` is intentionally ignored: all built-in platform
    // entries start hidden until the shell stamps credentials.
}

fn load_pi_catalog_models() -> Vec<BuiltinPlatformModel> {
    let file: CatalogFile = serde_json::from_str(PLATFORM_CATALOG_JSON)
        .expect("platform_catalog.json must parse");
    file.models
        .into_iter()
        .filter_map(|row| {
            let platform = PlatformId::parse(&row.platform)?;
            let api_backend = PlatformApiBackend::parse(&row.api_backend)?;
            Some(BuiltinPlatformModel {
                platform,
                model: row.model,
                name: row.name,
                description: row.description,
                context_window: row.context_window,
                supports_reasoning_effort: row.supports_reasoning_effort,
                // Pi catalog ships `supported_in_api: true` for many providers,
                // but we must not show models in the picker until credentials
                // (env/config OAuth) are actually available. The shell's
                // `apply_platform_credentials` re-enables visibility when keys
                // resolve.
                supported_in_api: false,
                max_completion_tokens: row.max_completion_tokens,
                api_backend,
            })
        })
        .collect()
}

/// Offline catalog. Primary source: official Pi `packages/ai` generated data
/// (`platform_catalog.json`). Hand-maintained Kimi/Moonshot rows fill gaps only
/// when the Pi catalog lacks that catalog key.
pub fn platform_builtin_models() -> &'static [BuiltinPlatformModel] {
    static MODELS: LazyLock<Vec<BuiltinPlatformModel>> = LazyLock::new(|| {
        let mut out: Vec<BuiltinPlatformModel> = load_pi_catalog_models();
        let mut existing: std::collections::HashMap<String, usize> = out
            .iter()
            .enumerate()
            .map(|(i, m)| (m.catalog_key(), i))
            .collect();
        // Hand-maintained Kimi/Moonshot fallbacks override the Pi catalog so
        // we keep canonical ids / descriptions. Kimi Code subscription uses
        // Anthropic Messages (same as official Pi kimi-coding).
        for m in kimi_moonshot_offline_fallbacks() {
            if let Some(idx) = existing.get(&m.catalog_key()) {
                out[*idx] = m;
            } else {
                existing.insert(m.catalog_key(), out.len());
                out.push(m);
            }
        }
        out
    });
    &MODELS
}

fn kimi_moonshot_offline_fallbacks() -> Vec<BuiltinPlatformModel> {
    // ── Kimi Code subscription (api.kimi.com/coding/v1) ──────────────
    // Official Pi `kimi-coding` uses Anthropic Messages + forceAdaptiveThinking.
    // Canonical ids: k3, k2p7, kimi-for-coding-highspeed. Older open-platform
    // style ids remain as offline aliases for configs that still reference them.
    macro_rules! kimi {
        ($id:literal, $name:literal, $desc:literal, $ctx:expr, $effort:expr, $max_tok:expr) => {
            BuiltinPlatformModel {
                platform: PlatformId::KimiCode,
                model: $id.into(),
                name: $name.into(),
                description: $desc.into(),
                context_window: $ctx,
                supports_reasoning_effort: $effort,
                supported_in_api: false,
                max_completion_tokens: $max_tok,
                api_backend: PlatformApiBackend::Messages,
            }
        };
    }
    let kimi_k3 = kimi!(
        "k3",
        "Kimi K3",
        "Official Pi catalog (kimi-coding); adaptive thinking; 1M context",
        CTX_1M,
        true,
        Some(131_072)
    );
    let kimi_k2p7 = kimi!(
        "k2p7",
        "Kimi K2.7 Code",
        "Official Pi catalog (kimi-coding); adaptive thinking; 256k context",
        CTX_256K,
        true,
        MAX_TOK_32K
    );
    let kimi_hs = kimi!(
        "kimi-for-coding-highspeed",
        "Kimi For Coding HighSpeed",
        "Official Pi catalog (kimi-coding); adaptive thinking; HyperSpeed",
        CTX_256K,
        true,
        MAX_TOK_32K
    );
    // Retired offline aliases (no longer listed in the picker):
    // kimi-k2.7-code, kimi-k2.7-code-highspeed, kimi-k2.6, kimi-k2.5.
    // Use k2p7 / kimi-for-coding-highspeed / k3 instead.
    let kimi_coding = kimi!(
        "kimi-for-coding",
        "Kimi for Coding",
        "Legacy Kimi Code subscription id (offline fallback)",
        CTX_256K,
        true,
        MAX_TOK_32K
    );

    // ── Moonshot open platform — current multimodal lineup ───────────
    // Official Model List (platform.kimi.ai/docs/models). Hidden until an API
    // key is configured; the shell's `apply_platform_credentials` reveals them.
    macro_rules! open {
        ($plat:ident, $id:literal, $name:literal, $desc:literal, $ctx:expr, $effort:expr) => {
            BuiltinPlatformModel {
                platform: PlatformId::$plat,
                model: $id.into(),
                name: $name.into(),
                description: $desc.into(),
                context_window: $ctx,
                supports_reasoning_effort: $effort,
                supported_in_api: false,
                max_completion_tokens: MAX_TOK_32K,
                api_backend: PlatformApiBackend::ChatCompletions,
            }
        };
    }

    vec![
        // Subscription first (Pi canonical ids, then kimi-for-coding fallback).
        kimi_k3,
        kimi_k2p7,
        kimi_hs,
        kimi_coding,
        open!(
            MoonshotCn,
            "kimi-k3",
            "Kimi K3 (moonshot.cn)",
            "Flagship 1M context / always-thinking (offline fallback)",
            CTX_1M,
            true
        ),
        open!(
            MoonshotCn,
            "kimi-k2.7-code",
            "Kimi K2.7 Code (moonshot.cn)",
            "Dedicated coding model; thinking always on; 256k context",
            CTX_256K,
            false
        ),
        open!(
            MoonshotCn,
            "kimi-k2.7-code-highspeed",
            "Kimi K2.7 Code HighSpeed (moonshot.cn)",
            "HyperSpeed coding model (~180–260 tok/s); same quality as K2.7 Code",
            CTX_256K,
            false
        ),
        open!(
            MoonshotCn,
            "kimi-k2.6",
            "Kimi K2.6 (moonshot.cn)",
            "General multimodal; thinking on/off + preserved thinking; 256k",
            CTX_256K,
            false
        ),
        open!(
            MoonshotCn,
            "kimi-k2.5",
            "Kimi K2.5 (moonshot.cn)",
            "Multimodal agent model; thinking on/off (no preserved thinking); 256k",
            CTX_256K,
            false
        ),
        open!(
            MoonshotAi,
            "kimi-k3",
            "Kimi K3 (moonshot.ai)",
            "Flagship 1M context / always-thinking global (offline fallback)",
            CTX_1M,
            true
        ),
        open!(
            MoonshotAi,
            "kimi-k2.7-code",
            "Kimi K2.7 Code (moonshot.ai)",
            "Dedicated coding model; thinking always on; 256k context",
            CTX_256K,
            false
        ),
        open!(
            MoonshotAi,
            "kimi-k2.7-code-highspeed",
            "Kimi K2.7 Code HighSpeed (moonshot.ai)",
            "HyperSpeed coding model (~180–260 tok/s); same quality as K2.7 Code",
            CTX_256K,
            false
        ),
        open!(
            MoonshotAi,
            "kimi-k2.6",
            "Kimi K2.6 (moonshot.ai)",
            "General multimodal; thinking on/off + preserved thinking; 256k",
            CTX_256K,
            false
        ),
        open!(
            MoonshotAi,
            "kimi-k2.5",
            "Kimi K2.5 (moonshot.ai)",
            "Multimodal agent model; thinking on/off (no preserved thinking); 256k",
            CTX_256K,
            false
        ),
        // Deprecated aliases last.
        open!(
            MoonshotCn,
            "kimi-k2-turbo-preview",
            "Kimi K2 Turbo (deprecated, moonshot.cn)",
            "Deprecated K2 turbo alias — prefer kimi-k2.7-code / kimi-k2.6",
            CTX_256K,
            true
        ),
        open!(
            MoonshotCn,
            "kimi-k2-thinking-turbo",
            "Kimi K2 Thinking Turbo (deprecated, moonshot.cn)",
            "Deprecated K2 thinking alias — prefer kimi-k2.6 / kimi-k3",
            CTX_256K,
            true
        ),
        open!(
            MoonshotAi,
            "kimi-k2-turbo-preview",
            "Kimi K2 Turbo (deprecated, moonshot.ai)",
            "Deprecated K2 turbo alias — prefer kimi-k2.7-code / kimi-k2.6",
            CTX_256K,
            true
        ),
        open!(
            MoonshotAi,
            "kimi-k2-thinking-turbo",
            "Kimi K2 Thinking Turbo (deprecated, moonshot.ai)",
            "Deprecated K2 thinking alias — prefer kimi-k2.6 / kimi-k3",
            CTX_256K,
            true
        ),
    ]
}

// ── Per-model request-body profiles (platform.kimi.ai docs) ────────────────

/// How a Kimi/Moonshot model expects request fields.
///
/// Sources:
/// - platform.kimi.ai "Thinking Mode" + "K2.7 Code Parameters Differences" (Chat Completions)
/// - official Pi `kimi-coding` catalog: Anthropic Messages + `forceAdaptiveThinking`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KimiRequestProfile {
    /// `kimi-k3` / subscription `k3`: always thinks.
    /// - Chat Completions: top-level `reasoning_effort` (default `max`).
    /// - Messages (Pi): `thinking.type=adaptive` + `output_config.effort`
    ///   (`thinkingLevelMap` documents `max` only; default effort `max`).
    K3,
    /// Official Pi `k2p7` / `kimi-for-coding-highspeed` and open-platform
    /// `kimi-k2.7-code` (+ highspeed): thinking always on.
    /// - Chat Completions: fixed sampling; omit K2 `thinking` object.
    /// - Messages (Pi): `forceAdaptiveThinking` — adaptive + effort, no budget.
    K27Code,
    /// `kimi-k2.6`: `thinking.type` enabled/disabled; `thinking.keep` null|all.
    K26,
    /// `kimi-k2.5`: `thinking.type` only (no `keep`).
    K25,
    /// Older k2 turbo / thinking-turbo / kimi-for-coding — treat like always-thinking
    /// coding models (omit fixed-param fields; Messages adaptive when used).
    LegacyCoding,
}

/// Whether this profile uses Pi-style Anthropic adaptive thinking on the
/// Messages path (`thinking.type=adaptive` + `output_config.effort`).
pub fn kimi_force_adaptive_thinking(profile: KimiRequestProfile) -> bool {
    matches!(
        profile,
        KimiRequestProfile::K3 | KimiRequestProfile::K27Code | KimiRequestProfile::LegacyCoding
    )
}

/// Whether empty thinking `signature: ""` must be replayed (Pi
/// `compat.allowEmptySignature` for K3 / legacy kimi-for-coding).
pub fn kimi_allow_empty_thinking_signature(profile: KimiRequestProfile) -> bool {
    matches!(
        profile,
        KimiRequestProfile::K3 | KimiRequestProfile::LegacyCoding
    )
}

/// Classify a bare model id (or catalog key's model half) for request shaping.
pub fn kimi_request_profile(model_id: &str) -> Option<KimiRequestProfile> {
    // Accept both bare ids and `{platform}/{id}` catalog keys.
    let id = model_id
        .rsplit_once('/')
        .map(|(_, m)| m)
        .unwrap_or(model_id)
        .to_ascii_lowercase();
    match id.as_str() {
        "k3" | "kimi-k3" => Some(KimiRequestProfile::K3),
        // Official Pi subscription ids + open-platform aliases.
        "k2p7"
        | "kimi-k2.7-code"
        | "kimi-k2.7-code-highspeed"
        | "kimi-for-coding-highspeed" => Some(KimiRequestProfile::K27Code),
        "kimi-k2.6" => Some(KimiRequestProfile::K26),
        "kimi-k2.5" => Some(KimiRequestProfile::K25),
        "kimi-for-coding"
        | "kimi-k2-turbo-preview"
        | "kimi-k2-thinking-turbo"
        | "kimi-k2-thinking"
        | "kimi-k2-0905-preview"
        | "kimi-k2-0711-preview" => Some(KimiRequestProfile::LegacyCoding),
        _ if id.starts_with("kimi-k2.7") || id.starts_with("k2p7") => {
            Some(KimiRequestProfile::K27Code)
        }
        _ if id.starts_with("kimi-k2.6") => Some(KimiRequestProfile::K26),
        _ if id.starts_with("kimi-k2.5") => Some(KimiRequestProfile::K25),
        _ if id.starts_with("kimi-k3") || id == "k3" => Some(KimiRequestProfile::K3),
        _ => None,
    }
}

/// Kimi docs recommend ≥16k–32k max_tokens for thinking + tool loops.
pub const KIMI_DEFAULT_MAX_TOKENS: u32 = 32_768;

/// Whether the model rejects non-default temperature / top_p / penalties.
pub fn kimi_sampling_is_fixed(profile: KimiRequestProfile) -> bool {
    matches!(
        profile,
        KimiRequestProfile::K27Code | KimiRequestProfile::K26 | KimiRequestProfile::LegacyCoding
    )
}

// ── Live `/models` wire contract ────────────────────────────────────────────

/// Capability tags derived from the `/models` listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ModelCapability {
    Thinking,
    AlwaysThinking,
    ImageIn,
    VideoIn,
}

/// One entry of `GET {base}/models` `data[]` (Kimi/Moonshot F4 shape).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WireModel {
    pub id: String,
    #[serde(default)]
    pub context_length: u64,
    #[serde(default)]
    pub supports_reasoning: bool,
    #[serde(default)]
    pub supports_image_in: bool,
    #[serde(default)]
    pub supports_video_in: bool,
    #[serde(default)]
    pub display_name: Option<String>,
    /// `"only"` → always-thinking (cannot disable).
    #[serde(default)]
    pub supports_thinking_type: Option<String>,
    #[serde(default)]
    pub think_efforts: Option<WireThinkEfforts>,
}

/// Selectable thinking levels (e.g. K3: low/high/max).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct WireThinkEfforts {
    #[serde(default)]
    pub support: bool,
    #[serde(default)]
    pub valid_efforts: Vec<String>,
    #[serde(default)]
    pub default_effort: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WireModelsResponse {
    pub data: Vec<WireModel>,
}

impl WireModel {
    pub fn capabilities(&self) -> Vec<ModelCapability> {
        let mut caps = derive_capabilities(
            &self.id,
            self.supports_reasoning,
            self.supports_image_in,
            self.supports_video_in,
        );
        if self.supports_thinking_type.as_deref() == Some("only") {
            for cap in [ModelCapability::Thinking, ModelCapability::AlwaysThinking] {
                if !caps.contains(&cap) {
                    caps.push(cap);
                }
            }
            caps.sort();
        }
        caps
    }
}

pub fn derive_capabilities(
    id: &str,
    supports_reasoning: bool,
    supports_image_in: bool,
    supports_video_in: bool,
) -> Vec<ModelCapability> {
    let id_lower = id.to_lowercase();
    let mut caps = std::collections::BTreeSet::new();
    if supports_reasoning {
        caps.insert(ModelCapability::Thinking);
    }
    if id_lower.contains("thinking") {
        caps.insert(ModelCapability::Thinking);
        caps.insert(ModelCapability::AlwaysThinking);
    }
    if supports_image_in {
        caps.insert(ModelCapability::ImageIn);
    }
    if supports_video_in {
        caps.insert(ModelCapability::VideoIn);
    }
    // Current multimodal coding lineup + legacy k2* / Pi ids: thinking + vision.
    if id_lower.starts_with("kimi-k2")
        || id_lower == "k3"
        || id_lower.starts_with("kimi-k3")
        || id_lower == "k2p7"
        || id_lower.starts_with("k2p7")
        || id_lower == "kimi-for-coding"
        || id_lower == "kimi-for-coding-highspeed"
    {
        caps.insert(ModelCapability::Thinking);
        caps.insert(ModelCapability::ImageIn);
        caps.insert(ModelCapability::VideoIn);
    }
    // K2.7 Code / HighSpeed / K3 / Pi coding ids: thinking cannot be disabled.
    if id_lower.contains("k2.7-code")
        || id_lower == "k2p7"
        || id_lower.starts_with("k2p7")
        || id_lower == "k3"
        || id_lower.starts_with("kimi-k3")
        || id_lower == "kimi-for-coding"
        || id_lower == "kimi-for-coding-highspeed"
    {
        caps.insert(ModelCapability::AlwaysThinking);
    }
    caps.into_iter().collect()
}

/// Apply platform prefix filter. No-op when the platform has no filter.
pub fn filter_allowed_models(platform: PlatformId, models: Vec<WireModel>) -> Vec<WireModel> {
    let Some(prefixes) = platform.allowed_model_prefixes() else {
        return models;
    };
    models
        .into_iter()
        .filter(|m| prefixes.iter().any(|p| m.id.starts_with(p) || m.id == *p))
        .collect()
}

/// Alias for Phase-1 callers.
pub fn moonshot_builtin_models() -> &'static [BuiltinPlatformModel] {
    platform_builtin_models()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_roundtrip() {
        for p in PlatformId::ALL {
            assert_eq!(PlatformId::parse(p.as_str()), Some(p));
            assert!(!p.base_url().is_empty());
        }
        assert!(PlatformId::KimiCode.uses_oauth());
        assert!(!PlatformId::MoonshotCn.uses_oauth());
        assert!(PlatformId::KimiCode.api_key_env_names().is_empty());
        assert!(!PlatformId::MoonshotCn.api_key_env_names().is_empty());
        assert_eq!(PlatformId::parse("openai"), Some(PlatformId::OpenAi));
        assert_eq!(PlatformId::parse("anthropic"), Some(PlatformId::Anthropic));
        assert!(PlatformId::Anthropic.uses_x_api_key());
        assert!(!PlatformId::OpenAi.uses_x_api_key());
        // Ollama Cloud live-syncs its `/models` listing once OLLAMA_API_KEY resolves.
        assert!(PlatformId::Ollama.live_models_list_enabled());
        assert!(PlatformId::KimiCode.live_models_list_enabled());
        assert!(!PlatformId::DeepSeek.live_models_list_enabled());
    }

    #[test]
    fn managed_key_roundtrip() {
        let key = PlatformId::KimiCode.managed_model_key("kimi-for-coding");
        assert_eq!(key, "kimi-code/kimi-for-coding");
        assert_eq!(
            parse_managed_model_key(&key),
            Some((PlatformId::KimiCode, "kimi-for-coding"))
        );
    }

    #[test]
    fn base_url_matches_host() {
        assert!(PlatformId::KimiCode.base_url_matches("https://api.kimi.com/coding/v1"));
        assert!(PlatformId::KimiCode.base_url_matches("https://api.kimi.com/coding/v1/chat"));
        assert!(!PlatformId::KimiCode.base_url_matches("https://api.moonshot.cn/v1"));
    }

    #[test]
    fn normalize_kimi_code_base_url_adds_v1_for_pi_style() {
        assert_eq!(
            normalize_kimi_code_base_url("https://api.kimi.com/coding"),
            "https://api.kimi.com/coding/v1"
        );
        assert_eq!(
            normalize_kimi_code_base_url("https://api.kimi.com/coding/"),
            "https://api.kimi.com/coding/v1"
        );
        // Already Grok-style — leave alone.
        assert_eq!(
            normalize_kimi_code_base_url("https://api.kimi.com/coding/v1"),
            "https://api.kimi.com/coding/v1"
        );
        assert_eq!(
            normalize_kimi_code_base_url("https://api.kimi.com/coding/v1/"),
            "https://api.kimi.com/coding/v1"
        );
    }

    #[test]
    fn builtins_have_unique_catalog_keys() {
        let mut keys = std::collections::HashSet::new();
        for m in platform_builtin_models() {
            assert!(keys.insert(m.catalog_key()), "duplicate {}", m.catalog_key());
        }
    }

    /// Mirror of the live `api.kimi.com/coding/v1/models` K3 entry
    /// (fetched 2026-07): `supports_thinking_type: "only"` plus a
    /// `think_efforts` block with low/high/max and a max default.
    #[test]
    fn wire_model_parses_live_k3_think_efforts() {
        let json = serde_json::json!({
            "id": "k3",
            "created": 1_761_264_000,
            "object": "model",
            "display_name": "K3",
            "type": "model",
            "context_length": 1_048_576,
            "supports_reasoning": true,
            "supports_image_in": true,
            "supports_video_in": true,
            "supports_thinking_type": "only",
            "think_efforts": {
                "support": true,
                "valid_efforts": ["low", "high", "max"],
                "default_effort": "max"
            }
        });
        let wire: WireModel = serde_json::from_value(json).expect("k3 wire parses");
        assert_eq!(wire.id, "k3");
        assert_eq!(wire.context_length, 1_048_576);
        assert_eq!(wire.display_name.as_deref(), Some("K3"));
        assert_eq!(wire.supports_thinking_type.as_deref(), Some("only"));
        let think = wire.think_efforts.as_ref().expect("think_efforts present");
        assert!(think.support);
        assert_eq!(think.valid_efforts, ["low", "high", "max"]);
        assert_eq!(think.default_effort.as_deref(), Some("max"));
        let caps = wire.capabilities();
        assert!(caps.contains(&ModelCapability::Thinking));
        assert!(caps.contains(&ModelCapability::AlwaysThinking));
        assert!(caps.contains(&ModelCapability::ImageIn));
        assert!(caps.contains(&ModelCapability::VideoIn));
    }

    #[test]
    fn filter_allowed_keeps_open_platform_kimi_family() {
        let models = vec![
            WireModel {
                id: "kimi-k3".into(),
                context_length: 1_048_576,
                supports_reasoning: true,
                supports_image_in: true,
                supports_video_in: true,
                display_name: Some("Kimi K3".into()),
                supports_thinking_type: None,
                think_efforts: None,
            },
            WireModel {
                id: "moonshot-v1-8k".into(),
                context_length: 8_192,
                supports_reasoning: false,
                supports_image_in: false,
                supports_video_in: false,
                display_name: None,
                supports_thinking_type: None,
                think_efforts: None,
            },
            WireModel {
                id: "kimi-k2-turbo-preview".into(),
                context_length: 262_144,
                supports_reasoning: true,
                supports_image_in: true,
                supports_video_in: true,
                display_name: None,
                supports_thinking_type: None,
                think_efforts: None,
            },
        ];
        let kept = filter_allowed_models(PlatformId::MoonshotCn, models);
        let ids: Vec<_> = kept.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["kimi-k3", "kimi-k2-turbo-preview"]);
    }

    #[test]
    fn subscription_filter_is_noop() {
        let models = vec![WireModel {
            id: "k3".into(),
            context_length: 1_048_576,
            supports_reasoning: true,
            supports_image_in: true,
            supports_video_in: true,
            display_name: Some("K3".into()),
            supports_thinking_type: Some("only".into()),
            think_efforts: None,
        }];
        let kept = filter_allowed_models(PlatformId::KimiCode, models);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "k3");
    }

    #[test]
    fn models_list_url_appends_models() {
        let url = PlatformId::KimiCode.models_list_url();
        assert!(url.ends_with("/models"), "{url}");
        assert!(url.contains("kimi.com"), "{url}");
    }

    #[test]
    fn offline_catalog_includes_official_open_platform_lineup() {
        let keys: std::collections::HashSet<_> = platform_builtin_models()
            .iter()
            .map(|m| m.catalog_key())
            .collect();
        for id in [
            "moonshot-cn/kimi-k3",
            "moonshot-cn/kimi-k2.7-code",
            "moonshot-cn/kimi-k2.7-code-highspeed",
            "moonshot-cn/kimi-k2.6",
            "moonshot-cn/kimi-k2.5",
            "moonshot-ai/kimi-k3",
            "moonshot-ai/kimi-k2.7-code",
            "moonshot-ai/kimi-k2.7-code-highspeed",
            "moonshot-ai/kimi-k2.6",
            "moonshot-ai/kimi-k2.5",
            "kimi-code/k3",
            "kimi-code/k2p7",
            "kimi-code/kimi-for-coding-highspeed",
            "kimi-code/kimi-for-coding",
            "openai/gpt-4.1",
            "openai/gpt-5",
            "anthropic/claude-sonnet-4-5",
            "anthropic/claude-opus-4-5",
            "anthropic/claude-opus-4-8",
            "openrouter/openai/gpt-4o",
            "deepseek/deepseek-v4-flash",
            "groq/llama-3.3-70b-versatile",
            "ollama/gpt-oss:120b",
            "ollama/kimi-k2.7-code",
            "ollama/deepseek-v4-pro",
        ] {
            assert!(keys.contains(id), "missing offline fallback {id}");
        }
        assert!(
            platform_builtin_models().len() >= 100,
            "expected full Pi-derived catalog, got {}",
            platform_builtin_models().len()
        );
        let anth = platform_builtin_models()
            .iter()
            .find(|m| m.catalog_key() == "anthropic/claude-sonnet-4-5")
            .expect("claude-sonnet-4-5");
        assert_eq!(anth.api_backend, PlatformApiBackend::Messages);
        let oai = platform_builtin_models()
            .iter()
            .find(|m| m.catalog_key() == "openai/gpt-5")
            .expect("gpt-5");
        assert_eq!(oai.api_backend, PlatformApiBackend::Responses);
        for key in [
            "kimi-code/k3",
            "kimi-code/k2p7",
            "kimi-code/kimi-for-coding-highspeed",
        ] {
            let m = platform_builtin_models()
                .iter()
                .find(|m| m.catalog_key() == key)
                .unwrap_or_else(|| panic!("missing {key}"));
            assert_eq!(
                m.api_backend,
                PlatformApiBackend::Messages,
                "{key}: official Pi kimi-coding uses anthropic-messages"
            );
            assert!(
                !m.supported_in_api,
                "{key} starts hidden until OAuth"
            );
            assert!(
                m.supports_reasoning_effort,
                "{key} supports adaptive effort"
            );
        }
    }

    #[test]
    fn request_profiles_cover_official_ids() {
        assert_eq!(kimi_request_profile("kimi-k3"), Some(KimiRequestProfile::K3));
        assert_eq!(kimi_request_profile("k3"), Some(KimiRequestProfile::K3));
        assert_eq!(
            kimi_request_profile("kimi-code/k3"),
            Some(KimiRequestProfile::K3)
        );
        assert_eq!(
            kimi_request_profile("k2p7"),
            Some(KimiRequestProfile::K27Code)
        );
        assert_eq!(
            kimi_request_profile("kimi-code/k2p7"),
            Some(KimiRequestProfile::K27Code)
        );
        assert_eq!(
            kimi_request_profile("kimi-for-coding-highspeed"),
            Some(KimiRequestProfile::K27Code)
        );
        assert_eq!(
            kimi_request_profile("kimi-k2.7-code"),
            Some(KimiRequestProfile::K27Code)
        );
        assert_eq!(
            kimi_request_profile("kimi-k2.7-code-highspeed"),
            Some(KimiRequestProfile::K27Code)
        );
        assert_eq!(
            kimi_request_profile("moonshot-cn/kimi-k2.6"),
            Some(KimiRequestProfile::K26)
        );
        assert_eq!(
            kimi_request_profile("kimi-k2.5"),
            Some(KimiRequestProfile::K25)
        );
        assert_eq!(
            kimi_request_profile("kimi-for-coding"),
            Some(KimiRequestProfile::LegacyCoding)
        );
        assert!(kimi_sampling_is_fixed(KimiRequestProfile::K27Code));
        assert!(!kimi_sampling_is_fixed(KimiRequestProfile::K3));
        assert!(kimi_force_adaptive_thinking(KimiRequestProfile::K3));
        assert!(kimi_force_adaptive_thinking(KimiRequestProfile::K27Code));
        assert!(kimi_allow_empty_thinking_signature(KimiRequestProfile::K3));
        assert!(!kimi_allow_empty_thinking_signature(
            KimiRequestProfile::K27Code
        ));
    }
}
