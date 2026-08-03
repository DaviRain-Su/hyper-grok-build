use indexmap::IndexMap;

/// Configuration for the web search tool.
///
/// Use `Disabled` when no API key is available or web search should be turned off.
/// Use `Enabled { … }` to provide credentials and endpoint configuration.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WebSearchConfig {
    #[default]
    Disabled,
    Enabled {
        api_key: String,
        base_url: String,
        model: String,
        #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
        extra_headers: IndexMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        alpha_test_key: Option<String>,
    },
}

impl WebSearchConfig {
    /// Returns `true` when the config is the `Enabled` variant.
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled { .. })
    }

    /// Return a copy safe for returning to clients.
    ///
    /// The `api_key` is replaced with `"***REDACTED***"` and the optional
    /// extra access key field is stripped.
    pub fn redacted(&self) -> Self {
        match self {
            Self::Disabled => Self::Disabled,
            Self::Enabled {
                base_url,
                model,
                extra_headers,
                ..
            } => Self::Enabled {
                api_key: "***REDACTED***".to_string(),
                base_url: base_url.clone(),
                model: model.clone(),
                extra_headers: extra_headers.clone(),
                alpha_test_key: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default_is_disabled() {
        let config = WebSearchConfig::default();
        assert!(!config.is_enabled());
    }

    #[test]
    fn test_config_enabled() {
        let config = WebSearchConfig::Enabled {
            api_key: "test-key".to_string(),
            base_url: "https://api.x.ai/v1".to_string(),
            model: "test-web-search-model".to_string(),
            extra_headers: IndexMap::new(),
            alpha_test_key: None,
        };
        assert!(config.is_enabled());
    }

    #[test]
    fn test_config_redacted() {
        let mut headers = IndexMap::new();
        headers.insert("X-Custom".to_string(), "value".to_string());
        let config = WebSearchConfig::Enabled {
            api_key: "secret-key-12345".to_string(),
            base_url: "https://api.x.ai/v1".to_string(),
            model: "test-web-search-model".to_string(),
            extra_headers: headers,
            alpha_test_key: Some("alpha-secret".to_string()),
        };
        let redacted = config.redacted();
        match redacted {
            WebSearchConfig::Enabled {
                api_key,
                base_url,
                model,
                extra_headers,
                alpha_test_key,
            } => {
                assert_eq!(api_key, "***REDACTED***");
                assert_eq!(base_url, "https://api.x.ai/v1");
                assert_eq!(model, "test-web-search-model");
                assert_eq!(extra_headers.get("X-Custom").unwrap(), "value");
                assert!(alpha_test_key.is_none());
            }
            _ => panic!("Expected Enabled variant"),
        }
    }

    #[test]
    fn test_config_serde_roundtrip() {
        let config = WebSearchConfig::Enabled {
            api_key: "key".to_string(),
            base_url: "https://api.x.ai/v1".to_string(),
            model: "test-web-search-model".to_string(),
            extra_headers: IndexMap::new(),
            alpha_test_key: None,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: WebSearchConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_enabled());
    }

    #[test]
    fn test_config_deserialize_from_set_options_payload() {
        let json = r#"{
            "status": "enabled",
            "api_key": "xai-abc123",
            "base_url": "https://api.x.ai/v1",
            "model": "test-web-search-model"
        }"#;
        let config: WebSearchConfig = serde_json::from_str(json).unwrap();
        assert!(config.is_enabled());
    }
}
