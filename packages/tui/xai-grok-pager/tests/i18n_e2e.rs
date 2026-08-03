//! Live-apply proof for the i18n runtime path.
//!
//! This is a SEPARATE integration-test binary: the `xai-grok-pager` lib is
//! linked WITHOUT `cfg(test)` here, so `i18n::apply` really calls
//! `rust_i18n::set_locale` (inside lib unit tests it is a deliberate no-op to
//! keep the process-global locale stable across parallel tests). Keeping
//! these tests in their own binary also quarantines the global-locale flips
//! from every other suite.

use xai_grok_pager::i18n;

#[test]
fn apply_flips_process_locale_for_subsequent_lookups() {
    // English baseline.
    i18n::apply(Some("en"));
    assert_eq!(i18n::tr_or("welcome.quit", "Quit"), "Quit");

    // Commit zh-CN — every later lookup resolves from the zh-CN bundle.
    i18n::apply(Some("zh-CN"));
    assert_eq!(i18n::tr_or("welcome.quit", "Quit"), "退出");
    // Registry-catalog keys (tr_or design) translate too.
    assert_eq!(
        i18n::tr_or("settings.compact_mode.label", "Compact mode"),
        "紧凑模式"
    );

    // Switch again — ja takes over without a restart.
    i18n::apply(Some("ja"));
    assert_eq!(i18n::tr_or("welcome.quit", "Quit"), "終了");

    // Unknown configured values degrade to `auto` (OS locale) — no panic, no
    // raw-key leak for bundle-covered keys.
    i18n::apply(Some("xx-invalid"));
    let degraded = i18n::tr_or("welcome.quit", "Quit");
    assert!(
        degraded == "Quit" || degraded == "退出" || degraded == "終了",
        "unknown language must resolve to a real bundle, got: {degraded}"
    );

    // Restore English so the binary's own later assertions are deterministic.
    i18n::apply(Some("en"));
    assert_eq!(i18n::tr_or("welcome.quit", "Quit"), "Quit");
}

#[test]
fn resolve_locale_maps_os_independent_explicit_choices() {
    // Pure mapping sanity (no global mutation).
    assert_eq!(i18n::resolve_locale(Some("zh-Hant")), "zh-TW");
    assert_eq!(i18n::resolve_locale(Some("pt")), "pt-BR");
    assert_eq!(i18n::resolve_locale(Some("fr")), "fr");
    // All supported locales resolve to themselves.
    for loc in i18n::SUPPORTED_LOCALES {
        assert_eq!(&i18n::resolve_locale(Some(loc)), loc);
    }
}
