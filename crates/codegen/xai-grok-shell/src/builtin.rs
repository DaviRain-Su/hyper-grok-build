//! Built-in files extracted to `~/.grok/` on startup.

const BUILTIN_FILES: &[(&str, &str)] = &[("README.md", include_str!("../README.md"))];

/// Extract built-in metadata files to `~/.grok/` on startup.
///
/// User skills under `~/.grok/skills/` are never managed here. Platform skills
/// are delivered separately through the bundled skill cache.
pub fn extract_builtin_files(grok_home: &std::path::Path) {
    let version = xai_grok_version::VERSION;
    let marker = grok_home.join(".metadata_version");

    if let Ok(existing) = std::fs::read_to_string(&marker)
        && existing.trim() == version
    {
        return;
    }

    let _ = std::fs::create_dir_all(grok_home);

    // Clean up cached changelog files from previous version so
    // /release-notes fetches fresh content for the new version.
    for stale in &["CHANGELOG.json", "CHANGELOG.md"] {
        let _ = std::fs::remove_file(grok_home.join(stale));
    }

    for &(filename, content) in BUILTIN_FILES {
        if let Err(e) = std::fs::write(grok_home.join(filename), content) {
            tracing::debug!(error = %e, filename, "Failed to extract built-in file");
        }
    }

    let _ = std::fs::write(&marker, version);
    tracing::debug!(version, "Extracted built-in files");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_bump_reextracts_metadata_without_touching_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        extract_builtin_files(home);
        std::fs::write(home.join("README.md"), "old").unwrap();
        std::fs::write(home.join(".metadata_version"), "0.0.0-stale").unwrap();

        let skill_names = [
            "help",
            "create-skill",
            "code-review",
            "imagine",
            "check-work",
            "check",
            "best-of-n",
            "docx",
            "pptx",
            "xlsx",
        ];
        for name in skill_names {
            let dir = home.join("skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), format!("custom {name}")).unwrap();
            std::fs::write(dir.join("user-file.txt"), "keep").unwrap();
        }

        extract_builtin_files(home);

        assert_ne!(
            std::fs::read_to_string(home.join("README.md")).unwrap(),
            "old"
        );
        for name in skill_names {
            let dir = home.join("skills").join(name);
            assert_eq!(
                std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
                format!("custom {name}")
            );
            assert_eq!(
                std::fs::read_to_string(dir.join("user-file.txt")).unwrap(),
                "keep"
            );
        }
    }

    #[test]
    fn same_version_does_not_restore_missing_or_delete_legacy_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join("skills/check")).unwrap();
        std::fs::write(home.join("skills/check/SKILL.md"), "custom check").unwrap();
        std::fs::write(home.join(".metadata_version"), xai_grok_version::VERSION).unwrap();

        extract_builtin_files(home);

        assert!(!home.join("skills/help/SKILL.md").exists());
        assert_eq!(
            std::fs::read_to_string(home.join("skills/check/SKILL.md")).unwrap(),
            "custom check"
        );
    }
}
