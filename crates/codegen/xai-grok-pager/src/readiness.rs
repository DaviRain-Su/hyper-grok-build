//! `/readiness` — assess whether a repository is ready for agent autonomy.
//!
//! Inspired by Factory Droid's "Agent Readiness" (see
//! `docs/competitive-analysis.md` A6): an agent can only work autonomously on
//! a repo when it can build, verify, and orient itself there. This module
//! runs a fast, filesystem-only probe set (no builds, no network) and renders
//! a localized report pushed to the scrollback by the `/readiness` command.
//!
//! v1 is deliberately a *static* approximation: it checks for the presence of
//! orientation and verification infrastructure but does NOT run build/test
//! commands (Droid's readiness executes them; we trade certainty for speed).
//!
//! Verdict model (oracle-reviewed): `Not ready` requires a hard capability
//! gap — no recognizable build/tooling manifest, or no orientation source at
//! all (neither AGENTS.md nor README). Missing tests or a missing AGENTS.md
//! alone downgrade to `Mostly ready`, since plenty of repos are agent-friendly
//! without them.

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
    /// Recognizable build/tooling manifest (Cargo.toml, package.json, …).
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
    /// `Not ready` is reserved for hard capability gaps (oracle review): the
    /// agent can't build/run anything (no tooling manifest), or it can't
    /// orient at all (neither AGENTS.md nor README exists). Missing tests or
    /// a missing AGENTS.md on their own are improvement suggestions, not
    /// autonomy blockers.
    pub fn verdict_key(&self) -> &'static str {
        let status_of = |id: CheckId| self.checks.iter().find(|c| c.id == id).map(|c| c.status);
        let no_tooling = status_of(CheckId::BuildSystem) == Some(ReadinessStatus::Missing);
        let no_orientation = status_of(CheckId::AgentsMd) == Some(ReadinessStatus::Missing)
            && status_of(CheckId::Readme) == Some(ReadinessStatus::Missing);
        if no_tooling || no_orientation {
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

/// Git probe outcome, injectable for tests (the live path shells out to git).
#[derive(Clone, Debug)]
pub enum GitProbe {
    /// Inside a work tree; carries `git status --porcelain` lines.
    Repo(Vec<String>),
    /// Definitely not inside a git repository.
    NotARepo,
    /// Git binary missing, or the command failed/timed out.
    Unavailable,
}

/// Run all probes against `workspace` (live git detection included).
pub fn assess(workspace: &Path) -> ReadinessReport {
    let probe = git_probe(workspace);
    assess_in(workspace, &probe)
}

/// Injectable core of [`assess`]. Tests drive it with fixture directories and
/// a synthetic [`GitProbe`] instead of real repositories.
pub fn assess_in(workspace: &Path, git: &GitProbe) -> ReadinessReport {
    ReadinessReport {
        checks: vec![
            check_agents_md(workspace),
            check_build_system(workspace),
            check_tests(workspace),
            check_ci(workspace),
            check_lint_format(workspace),
            check_git(git),
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
/// Command-ish keywords marking "real" build/test guidance inside AGENTS.md.
/// Only counted on lines containing a backtick, so prose like "tests are not
/// available" doesn't false-positive (oracle review).
const AGENTS_MD_HINTS: &[&str] = &[
    "test", "build", "cargo", "npm", "pnpm", "yarn", "pytest", "make", "just", "gradle", "mvn",
    "dotnet", "mix", "swift", "bazel", "deno", "bun", "vitest", "jest", "tsc", "ruff", "lint",
    "fmt",
];

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
    let has_hints = content.lines().any(|line| {
        let lower = line.to_lowercase();
        line.contains('`') && AGENTS_MD_HINTS.iter().any(|hint| lower.contains(hint))
    });
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

/// Recognized build/tooling manifests (presence-only).
const BUILD_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "go.mod",
    "Makefile",
    "makefile",
    "justfile",
    "Justfile",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "gradlew",
    "mvnw",
    "CMakeLists.txt",
    "meson.build",
    "build.ninja",
    "WORKSPACE",
    "WORKSPACE.bazel",
    "MODULE.bazel",
    "BUILD",
    "BUILD.bazel",
    "build.sbt",
    "Gemfile",
    "composer.json",
    "deno.json",
    "deno.jsonc",
    "Package.swift",
    "mix.exs",
    "build.zig",
];

fn check_build_system(workspace: &Path) -> ReadinessCheck {
    let found: Vec<&str> = BUILD_MANIFESTS
        .iter()
        .filter(|name| workspace.join(name).is_file())
        .copied()
        .collect();
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

/// The npm placeholder test command, normalized (case/whitespace-insensitive).
fn is_npm_placeholder_test(cmd: &str) -> bool {
    cmd.to_lowercase().contains("no test specified")
}

/// `scripts.test` from package.json, structurally parsed.
fn npm_test_script(workspace: &Path) -> Option<String> {
    let content = std::fs::read_to_string(workspace.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    json.get("scripts")?
        .get("test")?
        .as_str()
        .map(str::to_string)
}

fn check_tests(workspace: &Path) -> ReadinessCheck {
    let mut found: Vec<String> = Vec::new();
    // An empty tests/ directory is not test infrastructure.
    let tests_dir = workspace.join("tests");
    if tests_dir.is_dir()
        && std::fs::read_dir(&tests_dir)
            .map(|mut e| e.next().is_some())
            .unwrap_or(false)
    {
        found.push("tests/".to_string());
    }
    if let Some(cmd) = npm_test_script(workspace)
        && !is_npm_placeholder_test(&cmd)
    {
        found.push("npm test".to_string());
    }
    for name in ["pytest.ini", "tox.ini"] {
        if workspace.join(name).is_file() {
            found.push(name.to_string());
        }
    }
    if let Ok(pyproject) = std::fs::read_to_string(workspace.join("pyproject.toml"))
        && (pyproject.contains("[tool.pytest") || pyproject.contains("pytest"))
    {
        found.push("pytest".to_string());
    }
    if has_go_test_files(workspace) {
        found.push("*_test.go".to_string());
    }
    // Rust unit tests live in src/ as `#[cfg(test)]` modules; scan a bounded,
    // deterministic set of files shallowly rather than walking the tree.
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

/// Sorted directory entries so scan caps are deterministic (oracle review).
fn sorted_dirs(path: &Path, cap: usize) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs.truncate(cap);
    dirs
}

fn has_go_test_files(workspace: &Path) -> bool {
    // Root + two levels covers cmd/, pkg/, internal/ layouts without a walk.
    let mut dirs = vec![workspace.to_path_buf()];
    let level1 = sorted_dirs(workspace, 12);
    for dir in &level1 {
        dirs.push(dir.clone());
    }
    for dir in &level1 {
        dirs.extend(sorted_dirs(dir, 4));
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
    let mut files: Vec<PathBuf> = std::fs::read_dir(&src)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "rs"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files.truncate(12);
    files.iter().any(|path| {
        std::fs::read(path)
            .map(|bytes| {
                let head = &bytes[..bytes.len().min(64 * 1024)];
                String::from_utf8_lossy(head).contains("#[cfg(test)]")
            })
            .unwrap_or(false)
    })
}

const CI_SIGNALS: &[&str] = &[
    ".gitlab-ci.yml",
    ".circleci/config.yml",
    "azure-pipelines.yml",
    ".woodpecker.yml",
    "Jenkinsfile",
    ".drone.yml",
    "bitbucket-pipelines.yml",
    ".travis.yml",
    "appveyor.yml",
];

fn check_ci(workspace: &Path) -> ReadinessCheck {
    for dir in [
        ".github/workflows",
        ".gitea/workflows",
        ".forgejo/workflows",
    ] {
        let workflows = workspace.join(dir);
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
                    evidence: format!("{dir} ({count})"),
                    suggestion_key: None,
                };
            }
        }
    }
    for name in CI_SIGNALS {
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
    "eslint.config.js",
    "eslint.config.mjs",
    "eslint.config.cjs",
    ".prettierrc",
    ".prettierrc.json",
    ".prettierrc.yml",
    ".prettierrc.yaml",
    ".prettierrc.js",
    "biome.json",
    "biome.jsonc",
    "ruff.toml",
    ".ruff.toml",
    ".golangci.yml",
    ".golangci.yaml",
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
        && (pyproject.contains("[tool.ruff]")
            || pyproject.contains("[tool.black]")
            || pyproject.contains("[tool.mypy]")
            || pyproject.contains("[tool.pylint]"))
    {
        found.push("ruff/black/mypy");
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

/// More uncommitted files than this escalates the dirty-worktree suggestion.
const GIT_DIRTY_WARN_THRESHOLD: usize = 20;

fn check_git(probe: &GitProbe) -> ReadinessCheck {
    match probe {
        GitProbe::NotARepo => ReadinessCheck {
            id: CheckId::Git,
            status: ReadinessStatus::Missing,
            evidence: ".git ✗".to_string(),
            suggestion_key: Some("readiness.suggest.git_repo"),
        },
        GitProbe::Unavailable => ReadinessCheck {
            id: CheckId::Git,
            status: ReadinessStatus::Warn,
            evidence: "git status ✗".to_string(),
            suggestion_key: Some("readiness.suggest.git_unknown"),
        },
        GitProbe::Repo(lines) => {
            let dirty = lines.iter().filter(|line| !line.trim().is_empty()).count();
            if dirty == 0 {
                ReadinessCheck {
                    id: CheckId::Git,
                    status: ReadinessStatus::Good,
                    evidence: rust_i18n::t!("readiness.evidence.clean").into_owned(),
                    suggestion_key: None,
                }
            } else {
                // The check is named "checkpoints": uncommitted work means the
                // agent has nothing to diff/revert against yet (oracle review).
                ReadinessCheck {
                    id: CheckId::Git,
                    status: ReadinessStatus::Warn,
                    evidence: format!("git status: {dirty}"),
                    suggestion_key: Some(if dirty > GIT_DIRTY_WARN_THRESHOLD {
                        "readiness.suggest.git_dirty"
                    } else {
                        "readiness.suggest.git_dirty_few"
                    }),
                }
            }
        }
    }
}

const LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
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
// Git detection (live path)
// ---------------------------------------------------------------------------

/// Repository membership is decided by git itself (`rev-parse`), not by
/// looking for a `.git` entry in the assessed directory — sessions commonly
/// run from a subdirectory, and worktrees/submodules use a `.git` FILE rather
/// than a directory (oracle review).
fn git_probe(workspace: &Path) -> GitProbe {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output();
    let Ok(output) = output else {
        return GitProbe::Unavailable; // git binary missing
    };
    if !output.status.success() {
        return GitProbe::NotARepo;
    }
    let inside = String::from_utf8_lossy(&output.stdout);
    if !inside.trim().eq_ignore_ascii_case("true") {
        return GitProbe::NotARepo;
    }
    match git_status_lines_bounded(workspace) {
        Some(lines) => GitProbe::Repo(lines),
        None => GitProbe::Unavailable,
    }
}

/// `git status --porcelain` with a hard 3s cap: huge worktrees can make an
/// unbounded synchronous status stall the dispatch path. On timeout/failure
/// the probe degrades to [`GitProbe::Unavailable`].
fn git_status_lines_bounded(workspace: &Path) -> Option<Vec<String>> {
    let workspace = workspace.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&workspace)
            .args(["status", "--porcelain"])
            .output();
        let _ = tx.send(output);
    });
    match rx.recv_timeout(std::time::Duration::from_secs(3)) {
        Ok(Ok(output)) if output.status.success() => Some(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::to_string)
                .collect(),
        ),
        // On timeout the spawned git process finishes on its own; we simply
        // stop waiting rather than blocking the UI.
        _ => None,
    }
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

    fn repo(lines: &[&str]) -> GitProbe {
        GitProbe::Repo(lines.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn empty_repo_is_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let report = assess_in(tmp.path(), &GitProbe::NotARepo);
        for id in [CheckId::AgentsMd, CheckId::BuildSystem, CheckId::Tests] {
            assert_eq!(
                check(&report, id).status,
                ReadinessStatus::Missing,
                "{id:?}"
            );
        }
        assert_eq!(report.verdict_key(), "readiness.verdict.not_ready");
        let rendered = render(&report);
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
            &format!("{} `cargo test` and `cargo build`", "x".repeat(300)),
        );
        touch(root, "Cargo.toml", "[package]\nname = \"x\"\n");
        touch(root, "tests/smoke.rs", "#[test] fn t() {}");
        touch(root, ".github/workflows/ci.yml", "on: push\n");
        touch(root, "rustfmt.toml", "edition = \"2024\"\n");
        touch(root, "Cargo.lock", "# lock\n");
        touch(root, "README.md", "# x\n");
        let report = assess_in(root, &repo(&[]));
        let (good, warn, missing) = report.counts();
        assert_eq!((good, warn, missing), (8, 0, 0), "{report:?}");
        assert_eq!(report.verdict_key(), "readiness.verdict.ready");
        assert!(check(&report, CheckId::Git).evidence.contains("clean"));
    }

    #[test]
    fn stub_agents_md_warns() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "AGENTS.md", "todo");
        let report = assess_in(tmp.path(), &GitProbe::NotARepo);
        let c = check(&report, CheckId::AgentsMd);
        assert_eq!(c.status, ReadinessStatus::Warn);
        assert_eq!(c.suggestion_key, Some("readiness.suggest.agents_md_stub"));
    }

    #[test]
    fn agents_md_prose_without_command_lines_is_not_guidance() {
        let tmp = tempfile::tempdir().unwrap();
        // Long prose that *mentions* tests but shows no command: not guidance.
        touch(
            tmp.path(),
            "AGENTS.md",
            &format!(
                "Tests are not available in this project. {}",
                "y".repeat(300)
            ),
        );
        let report = assess_in(tmp.path(), &GitProbe::NotARepo);
        assert_eq!(
            check(&report, CheckId::AgentsMd).status,
            ReadinessStatus::Warn
        );
    }

    #[test]
    fn package_json_scripts_are_parsed_structurally() {
        let tmp = tempfile::tempdir().unwrap();
        // `test` OUTSIDE scripts must not count.
        touch(tmp.path(), "package.json", r#"{"test": {"nested": true}}"#);
        let report = assess_in(tmp.path(), &GitProbe::NotARepo);
        assert_eq!(
            check(&report, CheckId::Tests).status,
            ReadinessStatus::Missing
        );
        // The manifest itself is a recognized tooling signal regardless.
        assert_eq!(
            check(&report, CheckId::BuildSystem).status,
            ReadinessStatus::Good
        );

        // npm placeholder variants (case/whitespace) are rejected.
        for placeholder in [
            r#"{"scripts": {"test": "echo \"Error: no test specified\" && exit 1"}}"#,
            r#"{"scripts": {"test": "echo \"No Test Specified\""}}"#,
        ] {
            touch(tmp.path(), "package.json", placeholder);
            let report = assess_in(tmp.path(), &GitProbe::NotARepo);
            assert_eq!(
                check(&report, CheckId::Tests).status,
                ReadinessStatus::Missing,
                "{placeholder}"
            );
        }

        touch(
            tmp.path(),
            "package.json",
            r#"{"scripts": {"build": "tsc", "test": "vitest"}}"#,
        );
        let report = assess_in(tmp.path(), &GitProbe::NotARepo);
        assert_eq!(check(&report, CheckId::Tests).status, ReadinessStatus::Good);
    }

    #[test]
    fn empty_tests_directory_does_not_count() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("tests")).unwrap();
        let report = assess_in(tmp.path(), &GitProbe::NotARepo);
        assert_eq!(
            check(&report, CheckId::Tests).status,
            ReadinessStatus::Missing
        );
        touch(tmp.path(), "tests/smoke.rs", "");
        let report = assess_in(tmp.path(), &GitProbe::NotARepo);
        assert_eq!(check(&report, CheckId::Tests).status, ReadinessStatus::Good);
    }

    #[test]
    fn any_dirty_worktree_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let report = assess_in(tmp.path(), &repo(&[" M a.rs", "?? b.rs"]));
        let c = check(&report, CheckId::Git);
        assert_eq!(c.status, ReadinessStatus::Warn);
        assert_eq!(c.suggestion_key, Some("readiness.suggest.git_dirty_few"));

        let lines: Vec<String> = (0..25).map(|i| format!(" M file{i}.rs")).collect();
        let report = assess_in(tmp.path(), &GitProbe::Repo(lines));
        let c = check(&report, CheckId::Git);
        assert_eq!(c.status, ReadinessStatus::Warn);
        assert_eq!(c.suggestion_key, Some("readiness.suggest.git_dirty"));
    }

    #[test]
    fn git_unavailable_warns_not_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let report = assess_in(tmp.path(), &GitProbe::Unavailable);
        let c = check(&report, CheckId::Git);
        assert_eq!(c.status, ReadinessStatus::Warn);
        assert_eq!(c.suggestion_key, Some("readiness.suggest.git_unknown"));
    }

    #[test]
    fn rust_unit_tests_detected_via_cfg_test() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "src/lib.rs", "#[cfg(test)]\nmod tests {}\n");
        let report = assess_in(tmp.path(), &GitProbe::NotARepo);
        assert_eq!(check(&report, CheckId::Tests).status, ReadinessStatus::Good);
    }

    #[test]
    fn verdict_missing_trio_but_oriented_is_mostly_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // No tests, no AGENTS.md — but build manifest + README keep it out of
        // the "not ready" bucket (oracle: absence of tests alone must not
        // hard-fail).
        touch(root, "Cargo.toml", "[package]\nname = \"x\"\n");
        touch(root, "README.md", "# x\n");
        let report = assess_in(root, &GitProbe::NotARepo);
        assert_eq!(report.verdict_key(), "readiness.verdict.mostly_ready");
    }

    #[test]
    fn verdict_no_orientation_source_is_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        // Build manifest exists, but neither AGENTS.md nor README.
        touch(tmp.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        let report = assess_in(tmp.path(), &GitProbe::NotARepo);
        assert_eq!(report.verdict_key(), "readiness.verdict.not_ready");
    }

    #[test]
    fn render_shows_verdict_and_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let report = assess_in(tmp.path(), &GitProbe::NotARepo);
        let rendered = render(&report);
        assert!(rendered.contains("Repo readiness"));
        assert!(rendered.contains("Not ready"));
    }

    /// Live git detection against a real temp repository, including a nested
    /// cwd (oracle's High finding). Skipped when git is unavailable.
    #[test]
    fn git_probe_detects_repo_from_nested_subdirectory() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("git unavailable; skipping live probe test");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("init")
            .output()
            .unwrap();
        assert!(init.status.success());
        let nested = root.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        // Nested cwd must still resolve as a repo (the old `.git`-in-cwd check
        // failed here). Fresh init with an untracked src/ → dirty → Warn.
        assert!(matches!(git_probe(root), GitProbe::Repo(_)));
        assert!(matches!(git_probe(&nested), GitProbe::Repo(_)));
        // A non-repo directory must come back NotARepo.
        let plain = tempfile::tempdir().unwrap();
        assert!(matches!(git_probe(plain.path()), GitProbe::NotARepo));
    }
}
