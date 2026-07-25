//! Load `config.toml` as a [`toml_edit::DocumentMut`] for in-place edits.
//! A non-empty file that does not parse is left untouched (`None`).

use std::path::Path;

#[must_use]
pub(crate) fn read_config_document_for_edit(path: &Path) -> Option<toml_edit::DocumentMut> {
    #[allow(clippy::manual_unwrap_or_default)]
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => String::new(),
    };
    match content.parse() {
        Ok(d) => Some(d),
        Err(e) => {
            if content.is_empty() {
                return Some(toml_edit::DocumentMut::new());
            }
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "config.toml is not valid TOML; refusing to overwrite"
            );
            None
        }
    }
}

/// Set `[hints].<key>` to `value` in `~/.grok/config.toml`, preserving every
/// other key and table. Creates the file and parent dir when missing, and
/// no-ops when the existing file is non-empty but unparseable (so a malformed
/// config is never clobbered). Performs blocking I/O.
pub(crate) fn set_hint(key: &str, value: impl Into<toml_edit::Value>) -> std::io::Result<()> {
    let path = xai_grok_tools::util::grok_home::grok_home().join("config.toml");
    set_hint_at(&path, key, value)
}

/// Read `[models].hidden_models` from `~/.grok/config.toml` (absent → empty).
///
/// Entries are glob patterns matched against the catalog key or model slug;
/// the picker's hide action writes exact catalog keys, which are valid globs.
pub(crate) fn hidden_model_ids() -> Vec<String> {
    let path = xai_grok_tools::util::grok_home::grok_home().join("config.toml");
    hidden_model_ids_at(&path)
}

/// Path-injectable core of [`hidden_model_ids`].
fn hidden_model_ids_at(path: &Path) -> Vec<String> {
    models_string_array_at(path, "hidden_models")
}

/// Write the full `[models].hidden_models` array back to `~/.grok/config.toml`
/// (empty removes the key). The shell's config watcher hot-reloads the model
/// catalog, so hidden rows drop out of the projection on their own.
pub(crate) fn set_hidden_model_ids(ids: &[String]) -> std::io::Result<()> {
    let path = xai_grok_tools::util::grok_home::grok_home().join("config.toml");
    set_hidden_model_ids_at(&path, ids)
}

/// Path-injectable core of [`set_hidden_model_ids`].
fn set_hidden_model_ids_at(path: &Path, ids: &[String]) -> std::io::Result<()> {
    set_models_string_array_at(path, "hidden_models", ids)
}

/// Read `[models].enabled_models` (Pi scoped shortlist; absent → empty).
///
/// Also accepts Pi camelCase `enabledModels` so shell serde aliases and pager
/// cycle shortlists stay aligned. Snake_case wins if both keys are present.
pub(crate) fn enabled_model_ids() -> Vec<String> {
    let path = xai_grok_tools::util::grok_home::grok_home().join("config.toml");
    enabled_model_ids_at(&path)
}

/// Path-injectable core of [`enabled_model_ids`].
fn enabled_model_ids_at(path: &Path) -> Vec<String> {
    let Some(doc) = read_config_document_for_edit(path) else {
        return Vec::new();
    };
    let Some(models) = doc.get("models") else {
        return Vec::new();
    };
    // Prefer canonical snake_case; fall back to Pi camelCase.
    for key in ["enabled_models", "enabledModels"] {
        if let Some(ids) = models.get(key).and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        }) {
            return ids;
        }
    }
    Vec::new()
}

/// Write the full `[models].enabled_models` array (empty removes the key).
///
/// Always writes snake_case and removes a leftover `enabledModels` key so the
/// file cannot hold two serde-equivalent fields after a `/scoped-models` edit.
pub(crate) fn set_enabled_model_ids(ids: &[String]) -> std::io::Result<()> {
    let path = xai_grok_tools::util::grok_home::grok_home().join("config.toml");
    set_enabled_model_ids_at(&path, ids)
}

/// Path-injectable core of [`set_enabled_model_ids`].
fn set_enabled_model_ids_at(path: &Path, ids: &[String]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let Some(mut doc) = read_config_document_for_edit(path) else {
        return Ok(());
    };
    if let Some(models) = doc.get_mut("models").and_then(|m| m.as_table_mut()) {
        models.remove("enabledModels");
        if ids.is_empty() {
            models.remove("enabled_models");
        } else {
            let mut arr = toml_edit::Array::default();
            for id in ids {
                arr.push(id.as_str());
            }
            models["enabled_models"] = toml_edit::value(arr);
        }
    } else if !ids.is_empty() {
        let mut arr = toml_edit::Array::default();
        for id in ids {
            arr.push(id.as_str());
        }
        doc["models"]["enabled_models"] = toml_edit::value(arr);
    }
    std::fs::write(path, doc.to_string())
}

fn models_string_array_at(path: &Path, key: &str) -> Vec<String> {
    let Some(doc) = read_config_document_for_edit(path) else {
        return Vec::new();
    };
    doc.get("models")
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn set_models_string_array_at(path: &Path, key: &str, ids: &[String]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let Some(mut doc) = read_config_document_for_edit(path) else {
        return Ok(());
    };
    if ids.is_empty() {
        if let Some(models) = doc.get_mut("models").and_then(|m| m.as_table_mut()) {
            models.remove(key);
        }
    } else {
        let mut arr = toml_edit::Array::default();
        for id in ids {
            arr.push(id.as_str());
        }
        doc["models"][key] = toml_edit::value(arr);
    }
    std::fs::write(path, doc.to_string())
}

/// Path-injectable core of [`set_hint`].
fn set_hint_at(path: &Path, key: &str, value: impl Into<toml_edit::Value>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let Some(mut doc) = read_config_document_for_edit(path) else {
        return Ok(());
    };
    doc["hints"][key] = toml_edit::value(value);
    std::fs::write(path, doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn merge_round_trip_preserves_sibling_tables() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[ui]\ncompact_mode = false\n\n[mcpServers]\nx = \"y\"\n",
        )
        .unwrap();

        let mut doc = read_config_document_for_edit(&path).expect("parse");
        doc["ui"]["show_timestamps"] = toml_edit::value(false);
        fs::write(&path, doc.to_string()).unwrap();

        let body = fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("show_timestamps") && body.contains("mcpServers"),
            "expected merged TOML, got:\n{body}"
        );
    }

    #[test]
    fn nonempty_unparseable_returns_none_and_leaves_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let bad = "this is [not valid toml\n";
        fs::write(&path, bad).unwrap();

        assert!(read_config_document_for_edit(&path).is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), bad);
    }

    #[test]
    fn missing_file_is_editable_empty_doc() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("absent.toml");
        let doc = read_config_document_for_edit(&path).expect("editable");
        assert!(!doc.contains_key("ui"));
    }

    #[test]
    fn set_hint_at_round_trips_and_preserves_siblings() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[ui]\ncompact_mode = false\n").unwrap();

        set_hint_at(&path, "project_picker_disabled", true).unwrap();

        let doc = read_config_document_for_edit(&path).expect("reparse");
        assert_eq!(
            doc.get("hints")
                .and_then(|h| h.get("project_picker_disabled"))
                .and_then(|v| v.as_bool()),
            Some(true),
        );
        assert!(
            fs::read_to_string(&path).unwrap().contains("compact_mode"),
            "sibling [ui] should be preserved"
        );
    }

    #[test]
    fn set_hint_at_creates_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");
        set_hint_at(&path, "project_picker_disabled", true).unwrap();
        assert!(
            path.exists(),
            "missing file and parent dir should be created"
        );
    }

    #[test]
    fn set_hint_write_then_read_back_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[ui]\ntheme = \"dark\"\n").unwrap();

        set_hint_at(&path, "project_picker_disabled", true).unwrap();

        let doc = read_config_document_for_edit(&path).expect("reparse");
        let disabled = doc
            .get("hints")
            .and_then(|h| h.get("project_picker_disabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(disabled, "should read back true after set_hint write");
    }

    #[test]
    fn set_hint_at_leaves_unparseable_file_untouched() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let bad = "this is [not valid toml\n";
        fs::write(&path, bad).unwrap();

        // No-op (no write, no clobber) when the existing file cannot be parsed.
        set_hint_at(&path, "project_picker_disabled", true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), bad);
    }

    #[test]
    fn vim_mode_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[ui]\ncompact_mode = false\n").unwrap();

        let mut doc = read_config_document_for_edit(&path).expect("parse");
        doc["ui"]["vim_mode"] = toml_edit::value(true);
        fs::write(&path, doc.to_string()).unwrap();

        let doc2 = read_config_document_for_edit(&path).expect("reparse");
        let enabled = doc2
            .get("ui")
            .and_then(|h| h.get("vim_mode"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(enabled, "expected vim_mode = true after round-trip");

        let body = fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("compact_mode"),
            "sibling [ui] keys should be preserved"
        );
    }

    #[test]
    fn hidden_models_round_trip_preserves_siblings() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[models]\ndefault = \"grok-4.5\"\n\n[ui]\ncompact_mode = true\n",
        )
        .unwrap();

        assert!(hidden_model_ids_at(&path).is_empty());
        set_hidden_model_ids_at(
            &path,
            &[
                "deepseek/deepseek-v4-flash".to_string(),
                "gpt-5".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(
            hidden_model_ids_at(&path),
            ["deepseek/deepseek-v4-flash", "gpt-5"]
        );
        let body = fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("default = \"grok-4.5\""),
            "[models] sibling kept"
        );
        assert!(body.contains("compact_mode"), "[ui] table kept");

        // Empty clears the key but leaves the file parseable and siblings intact.
        set_hidden_model_ids_at(&path, &[]).unwrap();
        assert!(hidden_model_ids_at(&path).is_empty());
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("default = \"grok-4.5\""));
        assert!(!body.contains("hidden_models"));
    }

    #[test]
    fn hidden_models_set_creates_missing_table_and_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");
        set_hidden_model_ids_at(&path, &["grok-4.5".to_string()]).unwrap();
        assert_eq!(hidden_model_ids_at(&path), ["grok-4.5"]);
    }

    #[test]
    fn enabled_models_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[models]\ndefault = \"grok-4.5\"\n").unwrap();
        set_enabled_model_ids_at(
            &path,
            &["grok-*".to_string(), "openai/gpt-5".to_string()],
        )
        .unwrap();
        assert_eq!(
            enabled_model_ids_at(&path),
            ["grok-*", "openai/gpt-5"]
        );
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("default = \"grok-4.5\""));
        set_enabled_model_ids_at(&path, &[]).unwrap();
        assert!(enabled_model_ids_at(&path).is_empty());
        assert!(!fs::read_to_string(&path).unwrap().contains("enabled_models"));
    }

    #[test]
    fn enabled_models_reads_pi_camel_case_alias() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[models]\nenabledModels = [\"grok-*\", \"openai/*\"]\n",
        )
        .unwrap();
        assert_eq!(
            enabled_model_ids_at(&path),
            ["grok-*", "openai/*"],
            "pager cycle must honor Pi camelCase enabledModels"
        );
    }

    #[test]
    fn enabled_models_snake_case_wins_over_camel() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[models]\nenabled_models = [\"a\"]\nenabledModels = [\"b\"]\n",
        )
        .unwrap();
        assert_eq!(enabled_model_ids_at(&path), ["a"]);
    }

    #[test]
    fn set_enabled_models_migrates_away_from_camel_case() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[models]\nenabledModels = [\"old\"]\n").unwrap();
        set_enabled_model_ids_at(&path, &["new-*".to_string()]).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("enabled_models"));
        assert!(!body.contains("enabledModels"), "must not leave dual keys");
        assert_eq!(enabled_model_ids_at(&path), ["new-*"]);
    }
}
