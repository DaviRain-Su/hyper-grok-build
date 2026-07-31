//! Pi-style **scoped models**: soft shortlist for fast cycling.
//!
//! Config: `[models].enabled_models` (globs; also reads Pi `enabledModels`).
//! Distinct from hard-gate `allowed_models` and hide-list `hidden_models`.
//!
//! Glob matching uses `globset` (same engine as shell `ModelGlobSet`) so
//! `*`, `?`, and character classes stay aligned with config validation.

use agent_client_protocol as acp;
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::acp::model_state::{ModelState, platform_lock};

/// Glob patterns currently stored in `[models].enabled_models` / `enabledModels`.
pub(crate) fn enabled_patterns() -> Vec<String> {
    crate::config_toml_edit::enabled_model_ids()
}

/// Reject invalid glob syntax (same engine as shell config validation).
pub(crate) fn validate_glob_pattern(pattern: &str) -> Result<(), String> {
    Glob::new(pattern).map(|_| ()).map_err(|e| {
        format!("invalid glob pattern '{pattern}': {e}. Use * and ? wildcards (and [...] classes).")
    })
}

/// Validate every pattern; returns all invalid ones (empty = all ok).
pub(crate) fn invalid_glob_patterns(patterns: &[String]) -> Vec<String> {
    patterns
        .iter()
        .filter(|p| Glob::new(p).is_err())
        .cloned()
        .collect()
}

/// Compiled glob matcher aligned with shell model filters.
struct ModelGlobSet(GlobSet);

/// Result of compiling `enabled_models` patterns for cycling.
enum CompiledScope {
    /// No patterns configured → cycle all usable models.
    Unrestricted,
    /// At least one valid pattern → only matches.
    Restricted(ModelGlobSet),
    /// Patterns present but none valid → match nothing (do not broaden to all).
    AllInvalid,
}

impl ModelGlobSet {
    fn compile(patterns: &[String]) -> CompiledScope {
        if patterns.is_empty() {
            return CompiledScope::Unrestricted;
        }
        let mut builder = GlobSetBuilder::new();
        let mut any_valid = false;
        for pat in patterns {
            match Glob::new(pat) {
                Ok(glob) => {
                    builder.add(glob);
                    any_valid = true;
                }
                Err(e) => {
                    tracing::warn!(
                        pattern = %pat,
                        error = %e,
                        "enabled_models: skipping invalid glob"
                    );
                }
            }
        }
        if !any_valid {
            return CompiledScope::AllInvalid;
        }
        match builder.build() {
            Ok(set) => CompiledScope::Restricted(Self(set)),
            Err(e) => {
                tracing::warn!(error = %e, "enabled_models: failed to build glob set");
                CompiledScope::AllInvalid
            }
        }
    }

    fn matches(&self, key: &str, model_slug: &str) -> bool {
        self.0.is_match(key) || self.0.is_match(model_slug)
    }
}

/// Whether `text` matches any enabled pattern (empty patterns → true).
#[cfg(test)]
fn matches_enabled(patterns: &[String], key: &str, model_slug: &str) -> bool {
    match ModelGlobSet::compile(patterns) {
        CompiledScope::Unrestricted => true,
        CompiledScope::Restricted(set) => set.matches(key, model_slug),
        CompiledScope::AllInvalid => false,
    }
}

/// Usable (not credential-locked) models, filtered by `enabled_models` when set.
///
/// - Empty shortlist config → all usable models.
/// - Non-empty but entirely invalid globs → **no** candidates (never broaden to all).
///
/// Catalog order is preserved (IndexMap iteration order).
pub(crate) fn cycle_candidates(models: &ModelState, patterns: &[String]) -> Vec<acp::ModelId> {
    let scope = ModelGlobSet::compile(patterns);
    models
        .available
        .iter()
        .filter(|(id, info)| {
            if platform_lock(info).is_some() {
                return false;
            }
            let key = id.0.as_ref();
            match &scope {
                CompiledScope::Unrestricted => true,
                CompiledScope::Restricted(s) => s.matches(key, key),
                CompiledScope::AllInvalid => false,
            }
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// Next (or previous) model in the scoped cycle list.
///
/// Returns `None` when there is nothing to switch to (empty list, or single
/// entry already current).
pub(crate) fn adjacent_in_cycle(
    list: &[acp::ModelId],
    current: Option<&acp::ModelId>,
    reverse: bool,
) -> Option<acp::ModelId> {
    if list.is_empty() {
        return None;
    }
    if list.len() == 1 {
        return match current {
            Some(c) if c == &list[0] => None,
            _ => Some(list[0].clone()),
        };
    }
    let idx = current
        .and_then(|c| list.iter().position(|id| id == c))
        .unwrap_or(if reverse { 0 } else { list.len() - 1 });
    let next = if reverse {
        if idx == 0 { list.len() - 1 } else { idx - 1 }
    } else {
        (idx + 1) % list.len()
    };
    Some(list[next].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn mid(s: &str) -> acp::ModelId {
        acp::ModelId::new(Arc::from(s))
    }

    fn state(ids: &[&str], current: Option<&str>) -> ModelState {
        let mut models = ModelState::default();
        for id in ids {
            let model_id = mid(id);
            models.available.insert(
                model_id.clone(),
                acp::ModelInfo::new(model_id, id.to_string()),
            );
        }
        models.current = current.map(mid);
        models
    }

    #[test]
    fn globset_basics() {
        assert!(matches_enabled(&["grok-*".into()], "grok-4.5", "grok-4.5"));
        assert!(matches_enabled(
            &["openrouter/*".into()],
            "openrouter/anthropic/claude",
            "openrouter/anthropic/claude"
        ));
        assert!(!matches_enabled(&["grok-*".into()], "gpt-5", "gpt-5"));
        assert!(matches_enabled(&["exact".into()], "exact", "exact"));
        assert!(matches_enabled(&["a?c".into()], "abc", "abc"));
        assert!(!matches_enabled(&["a?c".into()], "abbc", "abbc"));
        assert!(matches_enabled(&["*".into()], "anything", "anything"));
        assert!(matches_enabled(
            &["grok-[34]*".into()],
            "grok-4.5",
            "grok-4.5"
        ));
        assert!(!matches_enabled(&["grok-[34]*".into()], "grok-2", "grok-2"));
    }

    #[test]
    fn empty_patterns_match_all() {
        assert!(matches_enabled(&[], "anything", "anything"));
    }

    #[test]
    fn all_invalid_patterns_match_nothing() {
        // Unclosed character class is invalid for globset.
        assert!(!matches_enabled(&["grok[".into()], "grok-4.5", "grok-4.5"));
        let models = state(&["grok-4.5", "openai/gpt-5"], Some("grok-4.5"));
        let cands = cycle_candidates(&models, &["grok[".into()]);
        assert!(
            cands.is_empty(),
            "invalid-only shortlist must not broaden to all models"
        );
    }

    #[test]
    fn validate_glob_pattern_rejects_bad_syntax() {
        assert!(validate_glob_pattern("grok-*").is_ok());
        assert!(validate_glob_pattern("grok[").is_err());
    }

    #[test]
    fn cycle_forward_and_back() {
        let list = vec![mid("a"), mid("b"), mid("c")];
        assert_eq!(
            adjacent_in_cycle(&list, Some(&mid("a")), false).as_ref(),
            Some(&mid("b"))
        );
        assert_eq!(
            adjacent_in_cycle(&list, Some(&mid("c")), false).as_ref(),
            Some(&mid("a"))
        );
        assert_eq!(
            adjacent_in_cycle(&list, Some(&mid("a")), true).as_ref(),
            Some(&mid("c"))
        );
        assert_eq!(
            adjacent_in_cycle(&list, None, false).as_ref(),
            Some(&mid("a"))
        );
    }

    #[test]
    fn patterns_filter_candidates() {
        let models = state(
            &["grok-4.5", "openai/gpt-5", "openrouter/x"],
            Some("grok-4.5"),
        );
        let all = cycle_candidates(&models, &[]);
        assert_eq!(all.len(), 3);
        let scoped = cycle_candidates(&models, &["grok-*".into(), "openai/*".into()]);
        assert_eq!(
            scoped.iter().map(|m| m.0.as_ref()).collect::<Vec<_>>(),
            ["grok-4.5", "openai/gpt-5"]
        );
    }
}
