//! JSON types for cli-chat-proxy's `/v1/team/{team_id}/managed-config` routes;
//! the documents written here are served to CLIs by `/v1/deployment/config`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamManagedConfig {
    pub configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_config: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirements: Option<String>,
    pub fail_closed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Full-replace: an absent field clears its stored document, so to update an
/// existing config echo both documents and `updated_at` from a GET — otherwise
/// a partial write silently drops the other (including `fail_closed`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)] // a typoed guard key must 400, not silently unguard
pub struct SetTeamManagedConfigRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_config: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirements: Option<String>,
    /// Guard: the write fails 412 unless the stored `updated_at` equals this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)] // a typoed guard key must 400, not silently unguard
pub struct DeleteTeamManagedConfigRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_updated_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_defaults() {
        let config = TeamManagedConfig {
            configured: true,
            managed_config: Some("[cli]\n".into()),
            requirements: Some("fail_closed = true\n".into()),
            fail_closed: true,
            updated_at: Some(
                DateTime::parse_from_rfc3339("2026-01-02T03:04:05.123456Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("2026-01-02T03:04:05.123456Z"), "{json}");
        assert_eq!(
            serde_json::from_str::<TeamManagedConfig>(&json).unwrap(),
            config
        );

        let unconfigured: TeamManagedConfig =
            serde_json::from_str(r#"{"configured":false,"fail_closed":false}"#).unwrap();
        assert_eq!(unconfigured, TeamManagedConfig::default());

        let set: SetTeamManagedConfigRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(set, SetTeamManagedConfigRequest::default());
        let del: DeleteTeamManagedConfigRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(del.expected_updated_at, None);
    }
}
