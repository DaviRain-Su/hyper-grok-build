#![allow(
    unused_imports,
    unused_variables,
    unused_mut,
    unreachable_code,
    dead_code
)]
//! Backend environment presets for the Grok CLI crate family: endpoint URL
//! defaults, environment selection, and env-var test support.
//!
//! Public builds expose production endpoints. Values resolve as a `GROK_*`
//! env-var override when set, else the compiled production default.
/// The endpoint set for one backend environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrokBuildEndpoints {
    pub cli_chat_proxy_base_url: &'static str,
    pub asset_server_url: &'static str,
    pub relay_ws_url: &'static str,
    pub gateway_ws_url: &'static str,
    pub ws_origin: &'static str,
}
const PRODUCTION_ENDPOINTS: GrokBuildEndpoints = GrokBuildEndpoints {
    cli_chat_proxy_base_url: "https://cli-chat-proxy.grok.com/v1",
    asset_server_url: "https://assets.grok.com",
    relay_ws_url: "wss://code.grok.com/ws/code-agent",
    gateway_ws_url: "wss://grok.com/ws/gw/",
    ws_origin: "https://grok.com",
};
pub const PROD_CLI_CHAT_PROXY_BASE_URL: &str = PRODUCTION_ENDPOINTS.cli_chat_proxy_base_url;
pub const PROD_ASSET_SERVER_URL: &str = PRODUCTION_ENDPOINTS.asset_server_url;
pub const PROD_RELAY_WS_URL: &str = PRODUCTION_ENDPOINTS.relay_ws_url;
pub const PROD_GATEWAY_WS_URL: &str = PRODUCTION_ENDPOINTS.gateway_ws_url;
pub const PROD_WS_ORIGIN: &str = PRODUCTION_ENDPOINTS.ws_origin;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GrokBuildEnvironment {
    #[default]
    Production,
}
impl GrokBuildEnvironment {
    pub fn from_flags(_dev: bool, _staging: bool) -> Self {
        GrokBuildEnvironment::Production
    }
    /// Indicator string for display; `None` for Production.
    pub fn indicator(&self) -> Option<&'static str> {
        match self {
            GrokBuildEnvironment::Production => None,
        }
    }
    pub fn is_production(&self) -> bool {
        matches!(self, GrokBuildEnvironment::Production)
    }
    fn env_prefix(&self) -> &'static str {
        match self {
            GrokBuildEnvironment::Production => "GROK_PRODUCTION",
        }
    }
    /// Compiled endpoint set for this environment (production by default).
    pub fn endpoints(&self) -> GrokBuildEndpoints {
        match self {
            GrokBuildEnvironment::Production => PRODUCTION_ENDPOINTS,
        }
    }
    /// Env-var override when set, else the compiled endpoint.
    fn resolve(&self, var_suffix: &str, compiled: &'static str) -> String {
        std::env::var(format!("{}{var_suffix}", self.env_prefix()))
            .unwrap_or_else(|_| compiled.to_string())
    }
    pub fn cli_chat_proxy_base_url(&self) -> String {
        self.resolve(
            "_CLI_CHAT_PROXY_BASE_URL",
            self.endpoints().cli_chat_proxy_base_url,
        )
    }
    pub fn ws_origin(&self) -> String {
        self.resolve("_WS_ORIGIN", self.endpoints().ws_origin)
    }
    pub fn asset_server_url(&self) -> String {
        self.resolve("_ASSET_SERVER_URL", self.endpoints().asset_server_url)
    }
    /// The relay WebSocket URL (Web Frontend at `grok.com/code` driving a
    /// local agent). Not the cloud-sandbox gateway ([`Self::gateway_ws_url`]);
    /// the two speak different protocols.
    pub fn relay_ws_url(&self) -> String {
        self.resolve("_WS_URL", self.endpoints().relay_ws_url)
    }
    /// The gateway WebSocket URL for `/cloud new` sandboxes. The shell's
    /// `GROK_GATEWAY_URL` opt-in takes precedence.
    pub fn gateway_ws_url(&self) -> String {
        self.resolve("_GATEWAY_WS_URL", self.endpoints().gateway_ws_url)
    }
}
impl std::fmt::Display for GrokBuildEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GrokBuildEnvironment::Production => write!(f, "production"),
        }
    }
}
/// Serializes env-var mutation across tests; `std::env` is process-global.
#[cfg(any(test, feature = "test-support"))]
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(any(test, feature = "test-support"))]
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}
/// RAII env-var override for tests: constructors snapshot the prior value
/// under [`ENV_LOCK`], `Drop` restores it, panics included.
#[cfg(any(test, feature = "test-support"))]
pub struct EnvVarGuard {
    key: &'static str,
    prev: Option<String>,
    _lock: std::sync::MutexGuard<'static, ()>,
}
#[cfg(any(test, feature = "test-support"))]
impl EnvVarGuard {
    pub fn set(key: &'static str, value: &str) -> Self {
        let lock = env_lock();
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, value) };
        Self {
            key,
            prev,
            _lock: lock,
        }
    }
    pub fn remove(key: &'static str) -> Self {
        let lock = env_lock();
        let prev = std::env::var(key).ok();
        unsafe { std::env::remove_var(key) };
        Self {
            key,
            prev,
            _lock: lock,
        }
    }
    /// Update the value while still holding the env lock.
    pub fn set_value(&self, value: &str) {
        unsafe { std::env::set_var(self.key, value) };
    }
}
#[cfg(any(test, feature = "test-support"))]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(prev) => unsafe { std::env::set_var(self.key, prev) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    /// The env-var prefixes are an operator interface; do not rename.
    #[test]
    fn test_env_prefix() {
        assert_eq!(
            GrokBuildEnvironment::Production.env_prefix(),
            "GROK_PRODUCTION"
        );
    }
    #[test]
    fn env_var_guard_set_value_updates_then_restores_on_drop() {
        const KEY: &str = "XAI_GROK_ENV_VAR_GUARD_SET_VALUE_PROBE";
        let before = std::env::var(KEY).ok();
        {
            let guard = EnvVarGuard::set(KEY, "initial");
            assert_eq!(std::env::var(KEY).ok().as_deref(), Some("initial"));
            guard.set_value("updated");
            assert_eq!(
                std::env::var(KEY).ok().as_deref(),
                Some("updated"),
                "set_value must update the env var while the guard is live"
            );
        }
        assert_eq!(
            std::env::var(KEY).ok(),
            before,
            "Drop must restore the pre-guard snapshot (was {before:?})"
        );
    }
    /// Guards against conflating the relay and gateway endpoints (a relay
    /// loop mistakenly connecting to `wss://grok.com/ws/gw/`).
    #[test]
    fn relay_and_gateway_urls_are_distinct() {
        assert_ne!(
            GrokBuildEnvironment::Production.relay_ws_url(),
            GrokBuildEnvironment::Production.gateway_ws_url(),
        );
    }
    #[test]
    fn test_from_flags() {
        assert_eq!(
            GrokBuildEnvironment::from_flags(false, false),
            GrokBuildEnvironment::Production
        );
    }
}
