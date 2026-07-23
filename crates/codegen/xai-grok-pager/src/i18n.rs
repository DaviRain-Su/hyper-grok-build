//! UI language (i18n) — locale resolution and initialization.
//!
//! Translations live in `locales/*.yml` and are embedded at compile time by
//! `rust-i18n` (loaded once in `lib.rs`). The active locale is resolved from,
//! in order:
//!
//! 1. `[ui].language` in `config.toml` (`auto` | one of [`SUPPORTED_LOCALES`])
//! 2. the OS locale reported by `sys-locale` (when `auto` / unset / unknown)
//!
//! Anything unresolved falls back to English. Call [`init`] once at startup
//! (before the first render), and [`apply`] whenever the `language` setting
//! changes.
//!
//! **Scope policy**: localization covers the interactive TUI (fullscreen and
//! minimal share the same event loop, which calls [`init`] before the first
//! render). Headless / ACP / doctor output is intentionally **English-only**:
//! those surfaces are machine-consumed (scripts, CI, editor protocols) where a
//! stable language is a feature, and they never call `t!` today. RTL languages
//! (ar, he, …) are intentionally absent: ratatui's left-to-right layout
//! assumptions make them render incorrectly today.

/// Canonical UI-language choices for the `language` setting.
/// Keep in sync with `settings/defs.rs::LANGUAGE_CHOICES` and `locales/*.yml`.
pub const SUPPORTED_LOCALES: &[&str] = &[
    "en", "de", "es", "fr", "ja", "ko", "pt-BR", "ru", "zh-CN", "zh-TW",
];

/// Canonicalize a raw `[ui].language` value: unset / unknown → `"auto"`.
///
/// Mirrors the `canonical_screen_mode` pattern in `settings::registry` — the
/// settings modal displays the canonical value, never the raw on-disk string.
pub fn canonical_language(value: Option<&str>) -> &'static str {
    match value.unwrap_or_default().trim() {
        "en" => "en",
        "de" => "de",
        "es" => "es",
        "fr" => "fr",
        "ja" => "ja",
        "ko" => "ko",
        // Accept common spellings; store the BCP-47 forms `pt-BR` / `zh-*`.
        "pt-BR" | "pt-br" | "pt_br" | "pt" => "pt-BR",
        "ru" => "ru",
        "zh-CN" | "zh-cn" | "zh_cn" | "zh" | "zh-Hans" | "zh-hans" | "zh-SG" | "zh-sg" => "zh-CN",
        "zh-TW" | "zh-tw" | "zh_tw" | "zh-Hant" | "zh-hant" | "zh-HK" | "zh-hk" | "zh-MO"
        | "zh-mo" => "zh-TW",
        _ => "auto",
    }
}

/// Resolve the effective locale id from the configured language.
///
/// `auto` (and unknown values) follow the OS locale; explicit choices win.
pub fn resolve_locale(configured: Option<&str>) -> &'static str {
    let canonical = canonical_language(configured);
    if canonical == "auto" {
        os_locale()
    } else {
        canonical
    }
}

/// Map the OS locale onto a supported UI locale.
fn os_locale() -> &'static str {
    let Some(raw) = sys_locale::get_locale() else {
        return "en";
    };
    let l = raw.to_ascii_lowercase().replace('_', "-");
    let code = l.as_str();
    if code.starts_with("zh") {
        // Traditional-Chinese locales → zh-TW; everything else zh-* → zh-CN.
        if code.starts_with("zh-tw")
            || code.starts_with("zh-hk")
            || code.starts_with("zh-mo")
            || code.starts_with("zh-hant")
        {
            return "zh-TW";
        }
        return "zh-CN";
    }
    if code.starts_with("ja") {
        return "ja";
    }
    if code.starts_with("ko") {
        return "ko";
    }
    if code.starts_with("es") {
        return "es";
    }
    // Only a pt-BR bundle exists today; pt-PT falls back to it (closest match).
    if code.starts_with("pt") {
        return "pt-BR";
    }
    if code.starts_with("fr") {
        return "fr";
    }
    if code.starts_with("de") {
        return "de";
    }
    if code.starts_with("ru") {
        return "ru";
    }
    "en"
}

/// Set the process-wide UI locale from the configured language.
/// Cheap and idempotent — safe to call on every settings commit.
///
/// Under `cfg(test)` this is a no-op: `rust_i18n::set_locale` flips a
/// process-global atomic, and lib unit tests run multi-threaded in one
/// process — a dispatch test committing `SetLanguage("zh-CN")` would leak
/// Chinese strings into unrelated render tests. `resolve_locale` (the pure
/// mapping) stays unit-tested; the live-apply path is covered by the
/// settings e2e suites, which link the lib without `cfg(test)`.
pub fn apply(configured: Option<&str>) {
    #[cfg(not(test))]
    rust_i18n::set_locale(resolve_locale(configured));
    #[cfg(test)]
    let _ = configured;
}

/// Initialize the UI locale at startup, before the first render.
pub fn init(configured: Option<&str>) {
    apply(configured);
}

/// Localized "press {key} again to {label}" pending-confirmation hint,
/// shared by the full TUI and the minimal pager — sibling crates can't
/// invoke `t!` themselves (the macro only exists inside this crate).
pub fn press_again_hint(key: &str, label: &str) -> String {
    rust_i18n::t!("shortcuts.press_again_key", key = key, label = label).into_owned()
}

/// Translate a runtime-computed key for the current locale, falling back to
/// `fallback` (the English source text) when the key has no bundle entry.
///
/// Long-tail surfaces (settings catalog, action registry) derive their
/// translation keys at render time (`format!("settings.{}.label", key)`),
/// which the literal-only `t!` call style can't express. The English source
/// in `defs.rs` / `defaults.rs` stays the single source of truth, locale
/// bundles are filled incrementally, and missing entries degrade to English
/// instead of leaking a raw key. Brand/proper-noun entries (theme names,
/// language endonyms) simply have no bundle key at all.
///
/// Uses the crate-internal `Option`-returning lookup generated by the
/// `rust_i18n::i18n!` invocation in `lib.rs`.
pub fn tr_or<'a>(key: &str, fallback: &'a str) -> std::borrow::Cow<'a, str> {
    let locale = rust_i18n::locale();
    crate::_rust_i18n_try_translate(locale.as_ref(), key)
        .map(|c| std::borrow::Cow::Owned(c.into_owned()))
        .unwrap_or(std::borrow::Cow::Borrowed(fallback))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tr_or_translates_dynamic_keys_and_falls_back() {
        // Dynamic keys resolve like literal `t!` lookups…
        // (lib tests always run in the default `en` locale — `apply` is a
        // no-op under cfg(test) — so this reads the en.yml bundle.)
        let value = tr_or("welcome.quit", "Quit");
        assert_eq!(value, "Quit");
        // …and a missing key degrades to the English source fallback.
        let missing = tr_or("settings.__definitely_missing__.label", "English source");
        assert_eq!(missing, "English source");
    }

    #[test]
    fn canonical_language_aliases() {
        assert_eq!(canonical_language(None), "auto");
        assert_eq!(canonical_language(Some("")), "auto");
        assert_eq!(canonical_language(Some("en")), "en");
        assert_eq!(canonical_language(Some("de")), "de");
        assert_eq!(canonical_language(Some("es")), "es");
        assert_eq!(canonical_language(Some("fr")), "fr");
        assert_eq!(canonical_language(Some("ja")), "ja");
        assert_eq!(canonical_language(Some("ko")), "ko");
        assert_eq!(canonical_language(Some("ru")), "ru");
        assert_eq!(canonical_language(Some("zh-CN")), "zh-CN");
        assert_eq!(canonical_language(Some("zh-cn")), "zh-CN");
        assert_eq!(canonical_language(Some("zh")), "zh-CN");
        assert_eq!(canonical_language(Some("zh-Hans")), "zh-CN");
        assert_eq!(canonical_language(Some("zh-TW")), "zh-TW");
        assert_eq!(canonical_language(Some("zh-tw")), "zh-TW");
        assert_eq!(canonical_language(Some("zh-Hant")), "zh-TW");
        assert_eq!(canonical_language(Some("zh-HK")), "zh-TW");
        assert_eq!(canonical_language(Some("pt-BR")), "pt-BR");
        assert_eq!(canonical_language(Some("pt-br")), "pt-BR");
        assert_eq!(canonical_language(Some("pt")), "pt-BR");
        // Unknown / unsupported locales fold back to auto (OS detection).
        assert_eq!(canonical_language(Some("ar")), "auto");
        assert_eq!(canonical_language(Some("it")), "auto");
    }

    #[test]
    fn explicit_choice_wins_over_os() {
        assert_eq!(resolve_locale(Some("en")), "en");
        assert_eq!(resolve_locale(Some("zh-CN")), "zh-CN");
        assert_eq!(resolve_locale(Some("zh-TW")), "zh-TW");
        assert_eq!(resolve_locale(Some("pt-BR")), "pt-BR");
        // Alias canonicalization flows through resolution too.
        assert_eq!(resolve_locale(Some("zh-Hant")), "zh-TW");
        assert_eq!(resolve_locale(Some("pt")), "pt-BR");
    }

    #[test]
    fn fallback_translation_is_english() {
        // Spot-check one key end-to-end through the macro in both locales.
        // NOTE: `rust_i18n::set_locale` is process-global — tests pass
        // `locale =` explicitly to stay race-free under parallel test runs.
        let zh = rust_i18n::t!("welcome.quit", locale = "zh-CN");
        assert_eq!(zh, "退出");
        let en = rust_i18n::t!("welcome.quit", locale = "en");
        assert_eq!(en, "Quit");
    }

    #[test]
    fn fallback_chain_and_tr_or_degrade_gracefully() {
        // A key missing from EVERY bundle comes back untranslated through
        // `t!` (rust-i18n v4.2 returns the key itself; older versions
        // prefixed it with the locale)…
        let value = rust_i18n::t!("no.such.key.exists", locale = "de");
        assert!(
            value == "no.such.key.exists" || value == "de.no.such.key.exists",
            "missing key must surface untranslated, got: {value}"
        );
        // …while `tr_or` degrades to its English source fallback instead.
        assert_eq!(
            tr_or("no.such.key.exists", "English source"),
            "English source"
        );
        // Registry keys are absent from en.yml by design (`tr_or` source
        // fallback), so under the default en locale they render the source.
        assert_eq!(
            tr_or("settings.compact_mode.label", "Compact mode"),
            "Compact mode"
        );
    }

    #[test]
    fn interpolation_works_in_both_locales() {
        let zh = rust_i18n::t!("welcome.login_with", locale = "zh-CN", label = "grok.com");
        assert_eq!(zh, "使用 grok.com 登录");
        let en = rust_i18n::t!("welcome.login_with", locale = "en", label = "grok.com");
        assert_eq!(en, "Login with grok.com");
        let ja = rust_i18n::t!("welcome.login_with", locale = "ja", label = "grok.com");
        assert_eq!(ja, "grok.com でログイン");
    }

    /// Every translation key referenced from code for the static (non-registry)
    /// namespaces. Registry-driven keys (`settings.*` catalog, `actions.*`,
    /// `verbgroup.*` vocabulary) are covered by
    /// [`registry_keys_are_covered_by_every_locale`] instead — that test
    /// derives the key space from the live registries.
    const ALL_KEYS: &[&str] = &[
        "welcome.login_with",
        "welcome.quit",
        "welcome.quit_hint",
        "welcome.switch_account",
        "welcome.zdr_blocked",
        "welcome.trust_question",
        "welcome.trust_warning_1",
        "welcome.trust_warning_2",
        "welcome.trust_yes",
        "welcome.trust_no",
        "welcome.prompt_placeholder",
        "version.tier",
        "version.api_key",
        "shortcuts.press_again",
        "shortcuts.press_again_key",
        "settings_modal.value_on",
        "settings_modal.value_off",
        "settings_modal.no_override",
        "settings_modal.reset_prompt",
        "settings_modal.reset_breadcrumb",
        "settings_modal.reset_prompt_fallback",
        "settings_modal.reset_breadcrumb_fallback",
        "settings_modal.docs_tip_long",
        "settings_modal.docs_tip_short",
        "settings_modal.no_matches",
        "settings_modal.placeholder_shell_default",
        "settings_modal.placeholder_type_value",
        "modal.title.save_and_send",
        "modal.title.save_changes",
        "modal.title.commands",
        "modal.title.resume_session",
        "modal.title.pick_effort",
        "modal.title.pick_model",
        "modal.title.pick_theme",
        "modal.title.pick_option",
        "modal.title.howto_guides",
        "modal.title.keyboard_shortcuts",
        "modal.title.memory",
        "modal.title.settings",
        "modal.title.reset_setting",
        "modal.title.memory_note",
        "shortcuts_help.category.getting_started",
        "shortcuts_help.category.input",
        "shortcuts_help.category.conversation_nav",
        "shortcuts_help.category.conversation_action",
        "shortcuts_help.category.panels",
        "shortcuts_help.category.session",
        "shortcuts_help.category.dashboard",
        "shortcuts_help.search_scrollback",
        "shortcuts_help.paste_desc",
        "shortcuts_help.paste_long_help",
        "shortcuts_help.dimmed_note",
        "permission.scope_hint_a",
        "permission.scope_hint_b",
        "question.other",
        "toast.saved",
        "toast.saved_restart",
        "toast.voice_language_system",
        "toast.compact_auto_note",
        "toast.rollback_no_arm",
        "notices.mode_switch",
        "notices.images_tmux",
        "notices.images_unsupported",
        "notices.browser_unavailable",
        "notices.mouse_reporting_on",
        "notices.mouse_off_scrollback",
        "notices.mouse_off_prompt",
        "status.queue_empty",
        "status.queue_header_one",
        "status.queue_header_other",
        "status.queue_row",
        "status.queue_row_more_one",
        "status.queue_row_more_other",
        "status.tasks_empty",
        "status.tasks_header_one",
        "status.tasks_header_other",
        "status.workflow_row",
        "status.workflow_agents_one",
        "status.workflow_agents_other",
        "status.subagent_row",
        "status.task_row",
        "status.kind.monitor",
        "status.kind.task",
        "status.scheduled_row",
        "status.word.running",
        "status.word.stopping",
        "status.word.done",
        "status.word.failed",
        "status.word.scheduled",
        "status.usage_incomplete_empty",
        "status.usage_empty",
        "status.usage_input",
        "status.usage_output",
        "status.usage_total",
        "status.usage_calls",
        "status.usage_cost",
        "status.usage_by_model",
        "status.usage_model_row",
        "status.usage_note_incomplete",
        "status.usage_header",
        "status.cost_unavailable_partial",
        "status.cost_unavailable",
        "verbgroup.segment",
        "verbgroup.joiner",
        "verbgroup.failed_suffix",
        "hints.send",
        "hints.queue",
        "hints.send_now",
        "hints.newline",
        "hints.expand",
        "hints.collapse",
        "hints.lines",
        "hints.accept_suggestion",
        "hints.mode",
        "hints.save",
        "hints.cancel",
        "hints.select",
        "hints.nav",
        "hints.page",
        "hints.turn",
        "hints.top_btm",
        "hints.next_prev",
        "hints.go",
        "hints.back",
        "hints.open",
        "hints.view",
        "hints.copy",
        "hints.copy_output",
        "hints.edit",
        "hints.delete_row",
        "hints.reorder",
        "hints.kill",
        "hints.show_done",
        "hints.hide_done",
        "hints.send_to_bg",
        "hints.quit",
        "hints.search",
        "hints.paste",
        "hints.close",
        "hints.scope",
        "hints.expand_thinking",
        "hints.collapse_thinking",
    ];

    #[test]
    fn every_supported_locale_covers_every_key() {
        // Fallback-blind check: `_rust_i18n_try_translate` follows the
        // fallback chain, so a missing zh-CN key would silently resolve from
        // en.yml and appear "covered". Parse each bundle's literal key set
        // instead — a key is covered only if it physically exists in the file.
        for locale in SUPPORTED_LOCALES {
            let keys = bundle_keys(locale);
            for key in ALL_KEYS {
                assert!(
                    keys.contains(*key),
                    "missing translation for `{key}` in locale `{locale}`"
                );
            }
        }
    }

    /// VerbGroupKind slugs (13 variants) — keep in sync with
    /// `scrollback/blocks/tool/mod.rs::VerbGroupKind::slug`.
    const VERBGROUP_SLUGS: &[&str] = &[
        "file",
        "skill",
        "search",
        "dir",
        "web_fetch",
        "web_search",
        "memory_search",
        "integration_search",
        "mcp_call",
        "subagent",
        "command",
        "other_tool",
        "edit_file",
    ];

    /// The registry-derived key space (`settings.*` catalog + `actions.*` +
    /// `settings.category.*` + `verbgroup.*` vocabulary), computed from the
    /// LIVE registries so a new setting/action fails this test until its
    /// translations land.
    fn registry_key_space() -> (Vec<String>, Vec<String>) {
        use crate::settings::{SettingKind, SettingsRegistry};
        let mut required: Vec<String> = Vec::new();
        // Display-name keys that intentionally stay untranslated (brand theme
        // names, language endonyms) — valid in a bundle, never required.
        let mut optional: Vec<String> = Vec::new();
        let reg = SettingsRegistry::defaults();
        for meta in reg.all() {
            required.push(format!("settings.{}.label", meta.key));
            required.push(format!("settings.{}.description", meta.key));
            if let SettingKind::Enum { choices, .. } = &meta.kind {
                for c in *choices {
                    let display_key =
                        format!("settings.{}.choice.{}.display", meta.key, c.canonical);
                    let desc_key =
                        format!("settings.{}.choice.{}.description", meta.key, c.canonical);
                    if matches!(
                        meta.key,
                        "theme" | "auto_dark_theme" | "auto_light_theme" | "language"
                    ) {
                        optional.push(display_key);
                    } else if !c.display.is_empty() {
                        required.push(display_key);
                    }
                    if !c.description.is_empty() {
                        required.push(desc_key);
                    }
                }
            }
        }
        let mut seen_ids = std::collections::HashSet::new();
        let fullscreen = crate::actions::ActionRegistry::defaults_with_config(true);
        let minimal = crate::actions::ActionRegistry::defaults_with_config_for(
            crate::app::ScreenMode::Minimal,
            true,
        );
        for def in fullscreen.all().iter().chain(minimal.all().iter()) {
            if !seen_ids.insert(def.id) {
                continue;
            }
            required.push(format!("actions.{}.label", def.id.i18n_key()));
            required.push(format!("actions.{}.description", def.id.i18n_key()));
            if let Some(lh) = def.long_help
                && !lh.is_empty()
            {
                required.push(format!("actions.{}.long_help", def.id.i18n_key()));
            }
        }
        for cat in crate::settings::SettingCategory::ALL {
            required.push(format!("settings.category.{}", cat.slug()));
        }
        for slug in VERBGROUP_SLUGS {
            for tense in ["past", "present"] {
                required.push(format!("verbgroup.verb.{slug}.{tense}"));
            }
            for plural in ["one", "other"] {
                required.push(format!("verbgroup.noun.{slug}.{plural}"));
            }
        }
        (required, optional)
    }

    #[test]
    fn registry_keys_are_covered_by_every_locale() {
        // `en` is exempt: registry surfaces fall back to the English source
        // in code via `tr_or` by design. Fallback-blind (see above).
        let (required, _) = registry_key_space();
        for locale in SUPPORTED_LOCALES {
            if locale == &"en" {
                continue;
            }
            let keys = bundle_keys(locale);
            let mut missing: Vec<&str> = Vec::new();
            for key in &required {
                if !keys.contains(key) {
                    missing.push(key);
                }
            }
            assert!(
                missing.is_empty(),
                "locale `{locale}` is missing {} registry translations: {:?}",
                missing.len(),
                &missing[..missing.len().min(25)]
            );
        }
    }

    /// Parse a bundle's literal key set from `locales/<locale>.yml`.
    /// Fallback-independent (see [`every_supported_locale_covers_every_key`]).
    fn bundle_keys(locale: &str) -> std::collections::HashSet<String> {
        let path = format!("{}/locales/{locale}.yml", env!("CARGO_MANIFEST_DIR"));
        let content = std::fs::read_to_string(&path).expect("read locale file");
        content
            .lines()
            .filter_map(|line| {
                let (key, _) = line.split_once(':')?;
                let key = key.trim();
                // Flat `key:` lines only — skip comments and blanks.
                (!key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'))
                .then(|| key.to_string())
            })
            .collect()
    }

    #[test]
    fn locale_bundles_have_no_orphan_registry_keys() {
        // Every `settings.*` / `actions.*` key present in a bundle must map to
        // a real registry entry (or the explicitly-optional display names);
        // stale keys from removed settings/actions would silently leak.
        let (required, optional) = registry_key_space();
        let valid: std::collections::HashSet<&str> = required
            .iter()
            .chain(optional.iter())
            .map(String::as_str)
            .collect();
        let locales_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/locales");
        for entry in std::fs::read_dir(locales_dir).expect("locales dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yml") {
                continue;
            }
            let content = std::fs::read_to_string(&path).expect("read locale file");
            for (lineno, line) in content.lines().enumerate() {
                let Some((key, _)) = line.split_once(':') else {
                    continue;
                };
                let key = key.trim();
                if (key.starts_with("settings.") || key.starts_with("actions."))
                    && !valid.contains(key)
                {
                    panic!(
                        "orphan key `{key}` in {}:{} — no matching registry entry",
                        path.display(),
                        lineno + 1
                    );
                }
            }
        }
    }
}
