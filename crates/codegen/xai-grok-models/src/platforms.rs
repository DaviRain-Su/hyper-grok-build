//! Built-in third-party platform registry.
//!
//! Phase 1: Moonshot open platforms (API key).
//! Phase 2: Kimi Code subscription (device OAuth).

use std::num::NonZeroU64;

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

const MOONSHOT_CN_BASE_URL_DEFAULT: &str = "https://api.moonshot.cn/v1";
const MOONSHOT_AI_BASE_URL_DEFAULT: &str = "https://api.moonshot.ai/v1";
const KIMI_CODE_BASE_URL_DEFAULT: &str = "https://api.kimi.com/coding/v1";
const KIMI_CODE_OAUTH_HOST_DEFAULT: &str = "https://auth.kimi.com";

fn env_or(var: &str, compiled: &str) -> String {
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => compiled.to_string(),
    }
}

/// Built-in inference platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PlatformId {
    /// Kimi Code subscription (device OAuth → api.kimi.com/coding/v1).
    KimiCode,
    /// Moonshot AI open platform — api.moonshot.cn (China).
    MoonshotCn,
    /// Moonshot AI open platform — api.moonshot.ai (global).
    MoonshotAi,
}

impl PlatformId {
    /// All platforms; subscription first so "default = first" can favor it.
    pub const ALL: [PlatformId; 3] = [Self::KimiCode, Self::MoonshotCn, Self::MoonshotAi];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::KimiCode => "kimi-code",
            Self::MoonshotCn => "moonshot-cn",
            Self::MoonshotAi => "moonshot-ai",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "kimi-code" => Some(Self::KimiCode),
            "moonshot-cn" => Some(Self::MoonshotCn),
            "moonshot-ai" => Some(Self::MoonshotAi),
            _ => None,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::KimiCode => "Kimi Code",
            Self::MoonshotCn => "Moonshot AI Open Platform (moonshot.cn)",
            Self::MoonshotAi => "Moonshot AI Open Platform (moonshot.ai)",
        }
    }

    /// Inference / model-list base URL.
    pub fn base_url(self) -> String {
        match self {
            Self::KimiCode => env_or(KIMI_CODE_BASE_URL_ENV, KIMI_CODE_BASE_URL_DEFAULT),
            Self::MoonshotCn => env_or(MOONSHOT_CN_BASE_URL_ENV, MOONSHOT_CN_BASE_URL_DEFAULT),
            Self::MoonshotAi => env_or(MOONSHOT_AI_BASE_URL_ENV, MOONSHOT_AI_BASE_URL_DEFAULT),
        }
    }

    /// OAuth host for the subscription channel only.
    pub fn oauth_host(self) -> Option<String> {
        match self {
            Self::KimiCode => Some(env_or(KIMI_CODE_OAUTH_HOST_ENV, KIMI_CODE_OAUTH_HOST_DEFAULT)),
            Self::MoonshotCn | Self::MoonshotAi => None,
        }
    }

    /// True for the OAuth-bearer subscription channel.
    pub fn uses_oauth(self) -> bool {
        matches!(self, Self::KimiCode)
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
        }
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

/// One built-in offline catalog entry for a platform.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinPlatformModel {
    pub platform: PlatformId,
    pub model: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub context_window: u64,
    pub supports_reasoning_effort: bool,
    /// When false, only OAuth session users see this in the picker.
    pub supported_in_api: bool,
}

impl BuiltinPlatformModel {
    pub fn catalog_key(&self) -> String {
        self.platform.managed_model_key(self.model)
    }

    pub fn context_window_nonzero(&self) -> NonZeroU64 {
        NonZeroU64::new(self.context_window).expect("builtin context_window is non-zero")
    }
}

/// Offline built-in catalog for all registry platforms.
pub fn platform_builtin_models() -> &'static [BuiltinPlatformModel] {
    const KIMI_CODING: BuiltinPlatformModel = BuiltinPlatformModel {
        platform: PlatformId::KimiCode,
        model: "kimi-for-coding",
        name: "Kimi for Coding",
        description: "Kimi Code subscription coding model",
        context_window: 262_144,
        supports_reasoning_effort: true,
        // Subscription-only: visible once OAuth credentials are stamped.
        supported_in_api: false,
    };
    const CN_TURBO: BuiltinPlatformModel = BuiltinPlatformModel {
        platform: PlatformId::MoonshotCn,
        model: "kimi-k2-turbo-preview",
        name: "Kimi K2 Turbo (moonshot.cn)",
        description: "Moonshot open platform coding model",
        context_window: 262_144,
        supports_reasoning_effort: true,
        supported_in_api: true,
    };
    const CN_THINKING: BuiltinPlatformModel = BuiltinPlatformModel {
        platform: PlatformId::MoonshotCn,
        model: "kimi-k2-thinking-turbo",
        name: "Kimi K2 Thinking Turbo (moonshot.cn)",
        description: "Moonshot open platform reasoning model",
        context_window: 262_144,
        supports_reasoning_effort: true,
        supported_in_api: true,
    };
    const AI_TURBO: BuiltinPlatformModel = BuiltinPlatformModel {
        platform: PlatformId::MoonshotAi,
        model: "kimi-k2-turbo-preview",
        name: "Kimi K2 Turbo (moonshot.ai)",
        description: "Moonshot open platform coding model (global)",
        context_window: 262_144,
        supports_reasoning_effort: true,
        supported_in_api: true,
    };
    const AI_THINKING: BuiltinPlatformModel = BuiltinPlatformModel {
        platform: PlatformId::MoonshotAi,
        model: "kimi-k2-thinking-turbo",
        name: "Kimi K2 Thinking Turbo (moonshot.ai)",
        description: "Moonshot open platform reasoning model (global)",
        context_window: 262_144,
        supports_reasoning_effort: true,
        supported_in_api: true,
    };
    &[KIMI_CODING, CN_TURBO, CN_THINKING, AI_TURBO, AI_THINKING]
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
        assert_eq!(PlatformId::parse("openai"), None);
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
    fn builtins_have_unique_catalog_keys() {
        let mut keys = std::collections::HashSet::new();
        for m in platform_builtin_models() {
            assert!(keys.insert(m.catalog_key()), "duplicate {}", m.catalog_key());
        }
    }
}
