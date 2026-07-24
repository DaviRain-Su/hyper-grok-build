//! `/readiness` — assess whether a repository is ready for agent autonomy.
//!
//! Inspired by Factory Droid's "Agent Readiness" (see
//! `docs/competitive-analysis.md` A6): an agent can only work autonomously on
//! a repo when it can build, verify, and orient itself there. This module
//! runs a fast, filesystem-only probe set (no builds, no network) and renders
//! a localized report pushed to the scrollback by the `/readiness` command.
//!
//! Probe set: AGENTS.md, build manifest, test infrastructure, CI workflows,
//! lint/format config, git checkpoint health, lockfile, README.

use std::path::{Path, PathBuf};

/// Outcome of a single readiness probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadinessStatus {
    /// The repo covers this capability.
    Good,
    /// Partially covered — works, but should be improved.
    Warn,
    /// Not covered — a real gap for agent autonomy.
    Missing,
}

/// Stable identifier for each probe (drives i18n keys).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckId {
    /// Root `AGENTS.md` with real build/test guidance.
    AgentsMd,
    /// Recognizable build manifest (Cargo.toml, package.json, …).
    BuildSystem,
    /// Any test infrastructure (tests/ dir, test script, pytest, …).
    Tests,
    /// CI workflow files — verification is scriptable.
    Ci,
    /// Linter/formatter configuration.
    LintFormat,
    /// Git repository with a reasonably checkpointed worktree.
    Git,
    /// Dependency lockfile for reproducible installs.
    Lockfile,
    /// README giving agents project context.
    Readme,
}

impl CheckId {
    /// i18n key for the check's display name.
    pub fn name_key(self) -> &'static str {
        match self {
            Self::AgentsMd => "readiness.check.agents_md",
            Self::BuildSystem => "readiness.check.build_system",
            Self::Tests => "readiness.check.tests",
            Self::Ci => "readiness.check.ci",
            Self::LintFormat => "readiness.check.lint_format",
            Self::Git => "readiness.check.git",
            Self::Lockfile => "readiness.check.lockfile",
            Self::Readme => "readiness.check.readme",
        }
    }
}

/// One probe result: status plus locale-neutral evidence (paths, counts,
/// script names) and an i18n suggestion key when the check is not `Good`.
#[derive(Clone, Debug)]
pub struct ReadinessCheck {
    /// Which probe produced this result.
    pub id: CheckId,
    /// Probe outcome.
    pub status: ReadinessStatus,
    /// Locale-neutral evidence string (file names, byte counts, script names).
    pub evidence: String,
    /// i18n key of the remediation suggestion (only when `status != Good`).
    pub suggestion_key: Option<&'static str>,
}

/// The full readiness assessment.
#[derive(Clone, Debug)]
pub struct ReadinessReport {
    /// One entry per probe, in display order.
    pub checks: Vec<ReadinessCheck>,
}

impl ReadinessReport {
    /// `(good, warn, missing)` counts.
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        for check in &self.checks {
            match check.status {
                ReadinessStatus::Good => counts.0 += 1,
                ReadinessStatus::Warn => counts.1 += 1,
                ReadinessStatus::Missing => counts.2 += 1,
            }
        }
        counts
    }

    /// i18n key of the overall verdict.
    ///
    /// The autonomy-critical trio is AGENTS.md + build + tests: an agent
    /// can't orient, compile, or verify without them, so any gap there is a
    /// hard "not ready". Everything else is polish.
    pub fn verdict_key(&self) -> &'static str {
        let critical_missing = self.checks.iter().any(|c| {
            c.status == ReadinessStatus::Missing
                && matches!(
                    c.id,
                    CheckId::AgentsMd | CheckId::BuildSystem | CheckId::Tests
                )
        });
        if critical_missing {
            return "readiness.verdict.not_ready";
        }
        let (_, warn, missing) = self.counts();
        if warn == 0 && missing == 0 {
            "readiness.verdict.ready"
        } else {
            "readiness.verdict.mostly_ready"
        }
    }
}

/// Run all probes against `workspace` (live git status included).
pub fn assess(workspace: &Path) -> ReadinessReport {
    let git_lines = git_status_lines(workspace);
    assess_in(workspace, git_lines.as_deref())
}

/// Injectable core of [`assess`]: `git_status` is the output of
/// `git status --porcelain` (`None` when git is unavailable or the directory
/// is not a repository). Tests drive it with fixtures instead of real repos.
pub fn assess_in(workspace: &Path, git_status: Option<&[String]>) -> ReadinessReport {
    ReadinessReport {
        checks: vec![
            check_agents_md(workspace),
            check_build_system(workspace),
            check_tests(workspace),
            check_ci(workspace),
            check_lint_format(workspace),
            check_git(workspace, git_status),
            check_lockfile(workspace),
            check_readme(workspace),
        ],
    }
}

/// Render the report as localized plain text for the scrollback.
pub fn render(report: &ReadinessReport) -> String {
    let mut out = String::new();
    out.push_str(&rust_i18n::t!("readiness.title"));
    out.push_str("\n\n");
    for check in &report.checks {
        let icon = match check.status {
            ReadinessStatus::Good => "✓",
            ReadinessStatus::Warn => "⚠",
            ReadinessStatus::Missing => "✗",
        };
        let name = rust_i18n::t!(check.id.name_key());
        out.push_str(&format!("{icon} {name} — {}", check.evidence));
        if let Some(key) = check.suggestion_key {
            out.push_str(&format!(" — {}", rust_i18n::t!(key)));
        }
        out.push('\n');
    }
    let (good, warn, missing) = report.counts();
    out.push('\n');
    out.push_str(&format!(
        "{} — {}",
        rust_i18n::t!(report.verdict_key()),
        rust_i18n::t!(
            "readiness.summary",
            good = good,
            warn = warn,
            missing = missing
        )
    ));
    out
}

// ---------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------

/// AGENTS.md shorter than this is a stub, not agent guidance.
const AGENTS_MD_MIN_BYTES: u64 = 200;
/// Substrings marking "real" build/test guidance inside AGENTS.md.
const AGENTS_MD_HINTS: &[&str] = &[
    "test", "build", "cargo", "npm", "pnpm", "yarn", "pytest", "make", "just", "gradle", "mvn",
    "go test",
];
/// More uncommitted files than this means the worktree is not checkpointed.
const GIT_DIRTY_WARN_THRESHOLD: usize = 20;

fn check_agents_md(workspace: &Path) -> ReadinessCheck {
    let path = workspace.join("AGENTS.md");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return ReadinessCheck {
            id: CheckId::AgentsMd,
            status: ReadinessStatus::Missing,
            evidence: "AGENTS.md ✗".to_string(),
            suggestion_key: Some("readiness.suggest.agents_md"),
        };
    };
    let bytes = content.len() as u64;
    let lower = content.to_lowercase();
    let has_hints = AGENTS_MD_HINTS.iter().any(|hint| lower.contains(hint));
    let (status, suggestion) = if bytes >= AGENTS_MD_MIN_BYTES && has_hints {
        (ReadinessStatus::Good, None)
    } else {
        (
            ReadinessStatus::Warn,
            Some("readiness.suggest.agents_md_stub"),
        )
    };
    ReadinessCheck {
        id: CheckId::AgentsMd,
        status,
        evidence: format!("AGENTS.md · {}", human_bytes(bytes)),
        suggestion_key: suggestion,
    }
}

/// Recognized build manifests and what to look for inside them.
const BUILD_MANIFESTS: &[(&str, Option<&str>)] = &[
    ("Cargo.toml", None),
    ("package.json", Some("\"build\"")),
    ("pyproject.toml", None),
    ("setup.py", None),
    ("go.mod", None),
    ("Makefile", None),
    ("makefile", None),
    ("justfile", None),
    ("Justfile", None),
    ("pom.xml", None),
    ("build.gradle", None),
    ("build.gradle.kts", None),
    ("CMakeLists.txt", None),
];

fn check_build_system(workspace: &Path) -> ReadinessCheck {
    let mut found: Vec<&str> = Vec::new();
    for (name, needle) in BUILD_MANIFESTS {
        let path = workspace.join(name);
        if !path.is_file() {
            continue;
        }
        if let Some(needle) = needle {
            // package.json only counts when it declares a build script.
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            if !content.contains(needle) {
                continue;
            }
        }
        found.push(name);
    }
    if found.is_empty() {
        ReadinessCheck {
            id: CheckId::BuildSystem,
            status: ReadinessStatus::Missing,
            evidence: "—".to_string(),
            suggestion_key: Some("readiness.suggest.build"),
        }
    } else {
        ReadinessCheck {
            id: CheckId::BuildSystem,
            status: ReadinessStatus::Good,
            evidence: found.join(" · "),
            suggestion_key: None,
        }
    }
}

fn check_tests(workspace: &Path) -> ReadinessCheck {
    let mut found: Vec<String> = Vec::new();
    if workspace.join("tests").is_dir() {
        found.push("tests/".to_string());
    }
    if let Ok(pkg) = std::fs::read_to_string(workspace.join("package.json"))
        && pkg.contains("\"test\"")
        && !pkg.contains("no test specified")
    {
        found.push("package.json test".to_string());
    }
    for name in ["pytest.ini", "tox.ini"] {
        if workspace.join(name).is_file() {
            found.push(name.to_string());
        }
    }
    if let Ok(pyproject) = std::fs::read_to_string(workspace.join("pyproject.toml"))
        && pyproject.contains("pytest")
    {
        found.push("pytest".to_string());
    }
    if has_go_test_files(workspace) {
        found.push("*_test.go".to_string());
    }
    // Rust unit tests live in src/ as `#[cfg(test)]` modules; scan a few
    // files shallowly rather than walking the tree.
    if found.is_empty() && has_rust_cfg_test(workspace) {
        found.push("#[cfg(test)]".to_string());
    }
    if found.is_empty() {
        ReadinessCheck {
            id: CheckId::Tests,
            status: ReadinessStatus::Missing,
            evidence: "—".to_string(),
            suggestion_key: Some("readiness.suggest.tests"),
        }
    } else {
        ReadinessCheck {
            id: CheckId::Tests,
            status: ReadinessStatus::Good,
            evidence: found.join(" · "),
            suggestion_key: None,
        }
    }
}

fn has_go_test_files(workspace: &Path) -> bool {
    let mut dirs = vec![workspace.to_path_buf()];
    // One level down covers cmd/, pkg/, internal/ layouts without a walk.
    if let Ok(entries) = std::fs::read_dir(workspace) {
        dirs.extend(
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .take(8),
        );
    }
    dirs.iter().any(|dir| {
        std::fs::read_dir(dir)
            .map(|mut entries| {
                entries.any(|e| {
                    e.ok()
                        .is_some_and(|e| e.file_name().to_string_lossy().ends_with("_test.go"))
                })
            })
            .unwrap_or(false)
    })
}

fn has_rust_cfg_test(workspace: &Path) -> bool {
    let src = workspace.join("src");
    let Ok(entries) = std::fs::read_dir(&src) else {
        return false;
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "rs"))
        .take(6)
        .any(|path| {
            std::fs::read(&path)
                .map(|bytes| {
                    let head = &bytes[..bytes.len().min(64 * 1024)];
                    String::from_utf8_lossy(head).contains("#[cfg(test)]")
                })
                .unwrap_or(false)
        })
}

fn check_ci(workspace: &Path) -> ReadinessCheck {
    let workflows = workspace.join(".github/workflows");
    if let Ok(entries) = std::fs::read_dir(&workflows) {
        let count = entries
            .filter_map(Result::ok)
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.ends_with(".yml") || name.ends_with(".yaml")
            })
            .count();
        if count > 0 {
            return ReadinessCheck {
                id: CheckId::Ci,
                status: ReadinessStatus::Good,
                evidence: format!(".github/workflows ({count})"),
                suggestion_key: None,
            };
        }
    }
    for name in [
        ".gitlab-ci.yml",
        ".circleci/config.yml",
        "azure-pipelines.yml",
        ".woodpecker.yml",
    ] {
        if workspace.join(name).is_file() {
            return ReadinessCheck {
                id: CheckId::Ci,
                status: ReadinessStatus::Good,
                evidence: name.to_string(),
                suggestion_key: None,
            };
        }
    }
    ReadinessCheck {
        id: CheckId::Ci,
        status: ReadinessStatus::Missing,
        evidence: "—".to_string(),
        suggestion_key: Some("readiness.suggest.ci"),
    }
}

const LINT_CONFIGS: &[&str] = &[
    "rustfmt.toml",
    ".rustfmt.toml",
    "clippy.toml",
    ".clippy.toml",
    ".eslintrc",
    ".eslintrc.json",
    ".eslintrc.js",
    ".eslintrc.yml",
    ".prettierrc",
    ".prettierrc.json",
    "biome.json",
    "ruff.toml",
    ".ruff.toml",
    ".golangci.yml",
    ".editorconfig",
];

fn check_lint_format(workspace: &Path) -> ReadinessCheck {
    let mut found: Vec<&str> = LINT_CONFIGS
        .iter()
        .filter(|name| workspace.join(name).is_file())
        .copied()
        .collect();
    if found.is_empty()
        && let Ok(pyproject) = std::fs::read_to_string(workspace.join("pyproject.toml"))
        && (pyproject.contains("[tool.ruff]") || pyproject.contains("[tool.black]"))
    {
        found.push("ruff/black");
    }
    if found.is_empty() {
        ReadinessCheck {
            id: CheckId::LintFormat,
            status: ReadinessStatus::Missing,
            evidence: "—".to_string(),
            suggestion_key: Some("readiness.suggest.lint"),
        }
    } else {
        ReadinessCheck {
            id: CheckId::LintFormat,
            status: ReadinessStatus::Good,
            evidence: found.join(" · "),
            suggestion_key: None,
        }
    }
}

fn check_git(workspace: &Path, git_status: Option<&[String]>) -> ReadinessCheck {
    if !workspace.join(".git").exists() {
        return ReadinessCheck {
            id: CheckId::Git,
            status: ReadinessStatus::Missing,
            evidence: ".git ✗".to_string(),
            suggestion_key: Some("readiness.suggest.git_repo"),
        };
    }
    let Some(lines) = git_status else {
        return ReadinessCheck {
            id: CheckId::Git,
            status: ReadinessStatus::Warn,
            evidence: "git status ✗".to_string(),
            suggestion_key: Some("readiness.suggest.git_unknown"),
        };
    };
    let dirty = lines.iter().filter(|line| !line.trim().is_empty()).count();
    if dirty > GIT_DIRTY_WARN_THRESHOLD {
        ReadinessCheck {
            id: CheckId::Git,
            status: ReadinessStatus::Warn,
            evidence: format!("git status: {dirty}"),
            suggestion_key: Some("readiness.suggest.git_dirty"),
        }
    } else if dirty == 0 {
        ReadinessCheck {
            id: CheckId::Git,
            status: ReadinessStatus::Good,
            evidence: rust_i18n::t!("readiness.evidence.clean").into_owned(),
            suggestion_key: None,
        }
    } else {
        ReadinessCheck {
            id: CheckId::Git,
            status: ReadinessStatus::Good,
            evidence: format!("git status: {dirty}"),
            suggestion_key: None,
        }
    }
}

const LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "poetry.lock",
    "Pipfile.lock",
    "uv.lock",
    "go.sum",
    "composer.lock",
    "Gemfile.lock",
];

fn check_lockfile(workspace: &Path) -> ReadinessCheck {
    let found: Vec<&str> = LOCKFILES
        .iter()
        .filter(|name| workspace.join(name).is_file())
        .copied()
        .collect();
    if found.is_empty() {
        ReadinessCheck {
            id: CheckId::Lockfile,
            status: ReadinessStatus::Missing,
            evidence: "—".to_string(),
            suggestion_key: Some("readiness.suggest.lockfile"),
        }
    } else {
        ReadinessCheck {
            id: CheckId::Lockfile,
            status: ReadinessStatus::Good,
            evidence: found.join(" · "),
            suggestion_key: None,
        }
    }
}

fn check_readme(workspace: &Path) -> ReadinessCheck {
    let found = std::fs::read_dir(workspace)
        .map(|entries| {
            entries.filter_map(Result::ok).any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .to_lowercase()
                    .starts_with("readme")
            })
        })
        .unwrap_or(false);
    if found {
        ReadinessCheck {
            id: CheckId::Readme,
            status: ReadinessStatus::Good,
            evidence: "README".to_string(),
            suggestion_key: None,
        }
    } else {
        ReadinessCheck {
            id: CheckId::Readme,
            status: ReadinessStatus::Missing,
            evidence: "—".to_string(),
            suggestion_key: Some("readiness.suggest.readme"),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `git status --porcelain` lines, or `None` when git is unavailable/fails.
fn git_status_lines(workspace: &Path) -> Option<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["-C"])
        .arg(workspace)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    })
}

/// Human-readable byte count for evidence strings.
fn human_bytes(bytes: u64) -> String {
    if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Convenience for tests: build a file under `root` with `content`.
#[cfg(test)]
fn touch(root: &Path, rel: &str, content: &str) -> PathBuf {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check<'a>(report: &'a ReadinessReport, id: CheckId) -> &'a ReadinessCheck {
        report.checks.iter().find(|c| c.id == id).unwrap()
    }

    #[test]
    fn empty_repo_is_not_ready_with_missing_trio() {
        let tmp = tempfile::tempdir().unwrap();
        let report = assess_in(tmp.path(), None);
        for id in [CheckId::AgentsMd, CheckId::BuildSystem, CheckId::Tests] {
            assert_eq!(
                check(&report, id).status,
                ReadinessStatus::Missing,
                "{id:?}"
            );
        }
        assert_eq!(report.verdict_key(), "readiness.verdict.not_ready");
        let rendered = render(&report);
        // Every suggestion key renders as English text under the default locale.
        assert!(rendered.contains("AGENTS.md"));
        assert!(!rendered.contains("readiness.suggest"));
    }

    #[test]
    fn fully_equipped_repo_is_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(
            root,
            "AGENTS.md",
            &format!("{} cargo test build", "x".repeat(300)),
        );
        touch(root, "Cargo.toml", "[package]\nname = \"x\"\n");
        touch(root, "tests/smoke.rs", "#[test] fn t() {}");
        touch(root, ".github/workflows/ci.yml", "on: push\n");
        touch(root, "rustfmt.toml", "edition = \"2024\"\n");
        touch(root, ".git/HEAD", "ref: refs/heads/main\n");
        touch(root, "Cargo.lock", "# lock\n");
        touch(root, "README.md", "# x\n");
        let report = assess_in(root, Some(&[]));
        let (good, warn, missing) = report.counts();
        assert_eq!((good, warn, missing), (8, 0, 0), "{report:?}");
        assert_eq!(report.verdict_key(), "readiness.verdict.ready");
        assert!(check(&report, CheckId::Git).evidence.contains("clean"));
    }

    #[test]
    fn stub_agents_md_warns() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "AGENTS.md", "todo");
        let report = assess_in(tmp.path(), None);
        let c = check(&report, CheckId::AgentsMd);
        assert_eq!(c.status, ReadinessStatus::Warn);
        assert_eq!(c.suggestion_key, Some("readiness.suggest.agents_md_stub"));
    }

    #[test]
    fn package_json_needs_a_real_build_and_test_script() {
        let tmp = tempfile::tempdir().unwrap();
        touch(
            tmp.path(),
            "package.json",
            r#"{"scripts": {"test": "echo \"Error: no test specified\" && exit 1"}}"#,
        );
        let report = assess_in(tmp.path(), None);
        assert_eq!(
            check(&report, CheckId::BuildSystem).status,
            ReadinessStatus::Missing
        );
        assert_eq!(
            check(&report, CheckId::Tests).status,
            ReadinessStatus::Missing
        );

        touch(
            tmp.path(),
            "package.json",
            r#"{"scripts": {"build": "tsc", "test": "vitest"}}"#,
        );
        let report = assess_in(tmp.path(), None);
        assert_eq!(
            check(&report, CheckId::BuildSystem).status,
            ReadinessStatus::Good
        );
        assert_eq!(check(&report, CheckId::Tests).status, ReadinessStatus::Good);
    }

    #[test]
    fn dirty_worktree_warns_above_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), ".git/HEAD", "ref: refs/heads/main\n");
        let lines: Vec<String> = (0..25).map(|i| format!(" M file{i}.rs")).collect();
        let report = assess_in(tmp.path(), Some(&lines));
        let c = check(&report, CheckId::Git);
        assert_eq!(c.status, ReadinessStatus::Warn);
        assert_eq!(c.suggestion_key, Some("readiness.suggest.git_dirty"));
    }

    #[test]
    fn rust_unit_tests_detected_via_cfg_test() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "src/lib.rs", "#[cfg(test)]\nmod tests {}\n");
        let report = assess_in(tmp.path(), None);
        assert_eq!(check(&report, CheckId::Tests).status, ReadinessStatus::Good);
    }

    #[test]
    fn render_shows_verdict_and_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let report = assess_in(tmp.path(), None);
        let rendered = render(&report);
        assert!(rendered.contains("Repo readiness"));
        assert!(rendered.contains("Not ready"));
    }
}
