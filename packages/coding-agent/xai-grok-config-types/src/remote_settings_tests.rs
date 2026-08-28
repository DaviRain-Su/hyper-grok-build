use super::RemoteSettings;

#[test]
fn active_agent_messages_round_trip_and_default_absent() {
    let settings: RemoteSettings =
        serde_json::from_str(r#"{"active_agent_messages_enabled":true}"#).unwrap();
    assert_eq!(settings.active_agent_messages_enabled, Some(true));

    let serialized = serde_json::to_string(&settings).unwrap();
    let round_trip: RemoteSettings = serde_json::from_str(&serialized).unwrap();
    assert_eq!(round_trip.active_agent_messages_enabled, Some(true));
    assert_eq!(
        RemoteSettings::default().active_agent_messages_enabled,
        None
    );
}
