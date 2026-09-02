use super::cache::{is_fresh, read_capped, write_private_atomic};
use super::*;

pub(crate) const SETTINGS_CACHE_FILE: &str = "settings_cache.json";
pub(crate) const SETTINGS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);
const SETTINGS_CACHE_MAX_BYTES: u64 = 1 << 20;

const SETTINGS_CACHE_HMAC_KEY: &[u8] =
    b"grok-shell-settings-cache-hmac-v1-ba6c43d3-404f-4b5c-b0cd-df09b2f5bdf4";

#[derive(serde::Serialize, serde::Deserialize)]
struct SettingsCache {
    fetched_at: DateTime<Utc>,
    grok_version: String,
    identity: String,
    origin: String,
    client: String,
    settings: crate::util::config::RemoteSettings,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SignedSettingsCache {
    payload: String,
    signature: Vec<u8>,
}

struct CachedSettings(crate::util::config::RemoteSettings);

impl CachedSettings {
    fn into_settings(self) -> crate::util::config::RemoteSettings {
        let mut settings = self.0;
        settings.managed_config_signature_verification = None;
        settings
    }
}

pub(crate) struct SettingsCacheManager {
    path: std::path::PathBuf,
    ttl: std::time::Duration,
}

impl SettingsCacheManager {
    pub(crate) fn new() -> Self {
        Self {
            path: crate::util::grok_home::grok_home().join(SETTINGS_CACHE_FILE),
            ttl: SETTINGS_CACHE_TTL,
        }
    }

    pub(crate) fn load_or_fetch(
        &self,
        auth: &crate::auth::GrokAuth,
        origin: &str,
        alpha_test_key: Option<&str>,
        fetch: impl FnOnce() -> Option<crate::util::config::RemoteSettings>,
    ) -> (
        Option<crate::util::config::RemoteSettings>,
        Option<SettingsCacheWrite>,
    ) {
        if crate::agent::config::env_bool("GROK_SETTINGS_CACHE") == Some(false) {
            return (fetch(), None);
        }
        let identity = Self::identity(auth, alpha_test_key);
        if let Some(cached) = self.load_fresh(&identity, origin) {
            tracing::info!("settings cache hit");
            return (Some(cached.into_settings()), None);
        }
        tracing::info!("settings cache miss; fetching");
        let Some(fetched) = fetch() else {
            return (None, None);
        };
        let write = SettingsCacheWrite {
            path: self.path.clone(),
            ttl: self.ttl,
            identity,
            origin: origin.to_string(),
            settings: fetched.clone(),
        };
        (Some(fetched), Some(write))
    }

    fn identity(auth: &crate::auth::GrokAuth, alpha_test_key: Option<&str>) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        auth.user_id.hash(&mut hasher);
        auth.key.hash(&mut hasher);
        alpha_test_key.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    fn load_fresh(&self, identity: &str, origin: &str) -> Option<CachedSettings> {
        let data = read_capped(&self.path, SETTINGS_CACHE_MAX_BYTES)?;
        let signed: SignedSettingsCache = serde_json::from_slice(&data)
            .inspect_err(|e| tracing::debug!(error = %e, "settings cache parse failed"))
            .ok()?;
        if !verify_cache_signature(signed.payload.as_bytes(), &signed.signature) {
            tracing::debug!("settings cache signature mismatch");
            return None;
        }
        let cache: SettingsCache = serde_json::from_str(&signed.payload).ok()?;
        if cache.grok_version != xai_grok_version::VERSION {
            tracing::debug!("settings cache version mismatch");
            return None;
        }
        if cache.identity != identity {
            tracing::debug!("settings cache identity mismatch");
            return None;
        }
        if cache.origin != origin {
            tracing::debug!("settings cache origin mismatch");
            return None;
        }
        if cache.client != crate::http::process_client_identifier() {
            tracing::debug!("settings cache client mismatch");
            return None;
        }
        if !is_fresh(cache.fetched_at, self.ttl) {
            tracing::debug!("settings cache is stale");
            return None;
        }
        Some(CachedSettings(cache.settings))
    }

    fn persist(
        &self,
        identity: &str,
        origin: &str,
        settings: &crate::util::config::RemoteSettings,
    ) {
        let cache = SettingsCache {
            fetched_at: Utc::now(),
            grok_version: xai_grok_version::VERSION.to_string(),
            identity: identity.to_string(),
            origin: origin.to_string(),
            client: crate::http::process_client_identifier(),
            settings: settings.clone(),
        };
        let Ok(payload) = serde_json::to_string(&cache) else {
            return;
        };
        let signature = sign_cache_payload(payload.as_bytes());
        let Ok(json) = serde_json::to_vec_pretty(&SignedSettingsCache { payload, signature })
        else {
            return;
        };
        write_private_atomic(&self.path, self.ttl, &json);
    }
}

pub(crate) struct SettingsCacheWrite {
    path: std::path::PathBuf,
    ttl: std::time::Duration,
    identity: String,
    origin: String,
    settings: crate::util::config::RemoteSettings,
}

impl SettingsCacheWrite {
    pub(crate) fn commit(self) {
        SettingsCacheManager {
            path: self.path,
            ttl: self.ttl,
        }
        .persist(&self.identity, &self.origin, &self.settings);
    }
}

type HmacSha256 = hmac::Hmac<sha2::Sha256>;

fn sign_cache_payload(payload: &[u8]) -> Vec<u8> {
    use hmac::Mac;
    let Ok(mut m) = HmacSha256::new_from_slice(SETTINGS_CACHE_HMAC_KEY) else {
        return Vec::new();
    };
    m.update(payload);
    m.finalize().into_bytes().to_vec()
}

fn verify_cache_signature(payload: &[u8], signature: &[u8]) -> bool {
    use hmac::Mac;
    let Ok(mut m) = HmacSha256::new_from_slice(SETTINGS_CACHE_HMAC_KEY) else {
        return false;
    };
    m.update(payload);
    m.verify_slice(signature).is_ok()
}

#[cfg(test)]
mod settings_cache_tests {
    use super::*;

    const ORIGIN: &str = "https://proxy.example";

    fn temp_manager(ttl: std::time::Duration) -> (tempfile::TempDir, SettingsCacheManager) {
        let dir = tempfile::tempdir().unwrap();
        let manager = SettingsCacheManager {
            path: dir.path().join(SETTINGS_CACHE_FILE),
            ttl,
        };
        (dir, manager)
    }

    fn settings() -> crate::util::config::RemoteSettings {
        crate::util::config::RemoteSettings {
            leader_mode: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn load_or_fetch_skips_the_fetch_on_a_warm_hit() {
        let (_dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let auth = crate::auth::GrokAuth::test_default();
        let (cold, write) = manager.load_or_fetch(&auth, ORIGIN, None, || Some(settings()));
        write.unwrap().commit();
        let (warm, warm_write) =
            manager.load_or_fetch(&auth, ORIGIN, None, || panic!("warm hit must not fetch"));
        assert_eq!(cold.unwrap().leader_mode, Some(true));
        assert_eq!(warm.unwrap().leader_mode, Some(true));
        assert!(warm_write.is_none());
    }

    #[test]
    fn load_or_fetch_defers_the_write_until_commit() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let auth = crate::auth::GrokAuth::test_default();
        let (_settings, write) = manager.load_or_fetch(&auth, ORIGIN, None, || Some(settings()));
        assert!(!dir.path().join(SETTINGS_CACHE_FILE).exists());
        write.unwrap().commit();
        assert!(dir.path().join(SETTINGS_CACHE_FILE).exists());
    }

    #[test]
    fn load_or_fetch_misses_on_a_different_alpha_test_key() {
        let (_dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let auth = crate::auth::GrokAuth::test_default();
        let (_a, write) =
            manager.load_or_fetch(&auth, ORIGIN, Some("alpha-a"), || Some(settings()));
        write.unwrap().commit();
        let (other, _) = manager.load_or_fetch(&auth, ORIGIN, Some("alpha-b"), || {
            Some(crate::util::config::RemoteSettings {
                leader_mode: Some(false),
                ..Default::default()
            })
        });
        assert_eq!(other.unwrap().leader_mode, Some(false));
    }

    #[test]
    fn load_or_fetch_does_not_persist_a_failed_fetch() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let auth = crate::auth::GrokAuth::test_default();
        let (result, write) = manager.load_or_fetch(&auth, ORIGIN, None, || None);
        assert!(result.is_none());
        assert!(write.is_none());
        assert!(!dir.path().join(SETTINGS_CACHE_FILE).exists());
    }

    #[test]
    fn misses_on_identity_origin_or_ttl_mismatch() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        manager.persist("id", ORIGIN, &settings());
        assert!(manager.load_fresh("other", ORIGIN).is_none());
        assert!(manager.load_fresh("id", "https://other").is_none());
        let expired = SettingsCacheManager {
            path: dir.path().join(SETTINGS_CACHE_FILE),
            ttl: std::time::Duration::ZERO,
        };
        assert!(expired.load_fresh("id", ORIGIN).is_none());
    }

    fn cache_file(fetched_at: DateTime<Utc>) -> SettingsCache {
        SettingsCache {
            fetched_at,
            grok_version: xai_grok_version::VERSION.to_string(),
            identity: "id".to_string(),
            origin: ORIGIN.to_string(),
            client: crate::http::process_client_identifier(),
            settings: settings(),
        }
    }

    fn signed_cache_bytes(cache: &SettingsCache) -> Vec<u8> {
        let payload = serde_json::to_string(cache).unwrap();
        let signature = sign_cache_payload(payload.as_bytes());
        serde_json::to_vec(&SignedSettingsCache { payload, signature }).unwrap()
    }

    #[test]
    fn hand_written_file_with_stale_version_or_client_misses() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let path = dir.path().join(SETTINGS_CACHE_FILE);
        let base = || cache_file(Utc::now());
        let write = |c: &SettingsCache| std::fs::write(&path, signed_cache_bytes(c)).unwrap();

        write(&SettingsCache {
            grok_version: format!("{}-stale", xai_grok_version::VERSION),
            ..base()
        });
        assert!(manager.load_fresh("id", ORIGIN).is_none());

        write(&SettingsCache {
            client: "other-client".to_string(),
            ..base()
        });
        assert!(manager.load_fresh("id", ORIGIN).is_none());
    }

    #[test]
    fn load_rejects_a_tampered_cache() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let path = dir.path().join(SETTINGS_CACHE_FILE);
        manager.persist("id", ORIGIN, &settings());
        assert!(manager.load_fresh("id", ORIGIN).is_some());

        let mut signed: SignedSettingsCache =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut payload: SettingsCache = serde_json::from_str(&signed.payload).unwrap();
        payload.settings.folder_trust_enabled = Some(false);
        signed.payload = serde_json::to_string(&payload).unwrap();
        std::fs::write(&path, serde_json::to_vec(&signed).unwrap()).unwrap();
        assert!(manager.load_fresh("id", ORIGIN).is_none());
    }

    #[test]
    fn load_rearms_signature_verification_even_from_a_valid_cache() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let mut cache = cache_file(Utc::now());
        cache.settings.managed_config_signature_verification = Some(false);
        std::fs::write(
            dir.path().join(SETTINGS_CACHE_FILE),
            signed_cache_bytes(&cache),
        )
        .unwrap();
        let loaded = manager.load_fresh("id", ORIGIN).unwrap().into_settings();
        assert_eq!(loaded.managed_config_signature_verification, None);
    }

    #[test]
    fn corrupt_cache_file_is_ignored() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        std::fs::write(dir.path().join(SETTINGS_CACHE_FILE), b"{ not json").unwrap();
        assert!(manager.load_fresh("id", ORIGIN).is_none());
    }

    fn padded_cache_file(len: usize) -> Vec<u8> {
        let mut json = signed_cache_bytes(&cache_file(Utc::now()));
        assert!(
            json.len() <= len,
            "base envelope already exceeds the target size"
        );
        json.resize(len, b' ');
        json
    }

    #[test]
    fn load_rejects_a_file_over_the_size_cap() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let over = padded_cache_file(SETTINGS_CACHE_MAX_BYTES as usize + 1);
        std::fs::write(dir.path().join(SETTINGS_CACHE_FILE), over).unwrap();
        assert!(manager.load_fresh("id", ORIGIN).is_none());
    }

    #[test]
    fn load_accepts_a_file_at_the_size_cap() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let at = padded_cache_file(SETTINGS_CACHE_MAX_BYTES as usize);
        std::fs::write(dir.path().join(SETTINGS_CACHE_FILE), at).unwrap();
        assert!(manager.load_fresh("id", ORIGIN).is_some());
    }

    #[test]
    fn load_rejects_a_future_dated_cache() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let future = cache_file(Utc::now() + ChronoDuration::hours(1));
        std::fs::write(
            dir.path().join(SETTINGS_CACHE_FILE),
            signed_cache_bytes(&future),
        )
        .unwrap();
        assert!(manager.load_fresh("id", ORIGIN).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn persist_writes_an_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        manager.persist("id", ORIGIN, &settings());
        let mode = std::fs::metadata(dir.path().join(SETTINGS_CACHE_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
