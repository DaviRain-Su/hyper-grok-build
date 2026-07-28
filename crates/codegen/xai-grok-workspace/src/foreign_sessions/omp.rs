use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::{
    ApprovedRoot, ForeignSessionSource, ForeignSessionSummary, ForeignSessionTool, MAX_SESSION_AGE,
    MAX_SESSIONS_PER_TOOL, RecentCandidate, RecentProbe, approved_root_for_recent,
    finish_tool_scan, is_within, normalize_title, retain_top_k_by,
};

const MAX_DIRECTORY_ENTRIES: usize = 4096;
const MAX_METADATA_READS: usize = 128;
const MAX_RECENT_METADATA_READS: usize = 16;
const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_HEAD_RECORDS: usize = 64;

#[derive(Clone)]
struct Candidate {
    root: ApprovedRoot,
    path: PathBuf,
    modified: SystemTime,
}

#[derive(Default)]
struct HeadMetadata {
    id: Option<String>,
    cwd: Option<String>,
    title_slot: Option<String>,
    header_title: Option<String>,
    first_user_message: Option<String>,
    short_summary: Option<String>,
}

pub(super) fn scan(cwd: &Path, now: SystemTime) -> Vec<ForeignSessionSummary> {
    let Some(sessions_root) = sessions_root() else {
        return Vec::new();
    };
    scan_in_sessions_root(&sessions_root, cwd, now)
}

pub(super) fn most_recent(
    cwd: &Path,
    now: SystemTime,
    within: Duration,
) -> RecentProbe<RecentCandidate> {
    let Some(sessions_root) = sessions_root() else {
        return RecentProbe::Complete(None);
    };
    most_recent_in_sessions_root(&sessions_root, cwd, now, within)
}

fn scan_in_sessions_root(
    sessions_root: &Path,
    cwd: &Path,
    now: SystemTime,
) -> Vec<ForeignSessionSummary> {
    let Some(root) = ApprovedRoot::new(sessions_root) else {
        return Vec::new();
    };
    let candidates = collect_candidates(&root, cwd, now, MAX_SESSION_AGE, MAX_METADATA_READS);
    let sessions = candidates
        .into_iter()
        .filter_map(|candidate| read_candidate(candidate, cwd))
        .collect();
    finish_tool_scan(sessions)
}

fn most_recent_in_sessions_root(
    sessions_root: &Path,
    cwd: &Path,
    now: SystemTime,
    within: Duration,
) -> RecentProbe<RecentCandidate> {
    let root = match approved_root_for_recent(sessions_root) {
        Ok(Some(root)) => root,
        Ok(None) => return RecentProbe::Complete(None),
        Err(()) => return RecentProbe::Incomplete,
    };
    let Some((candidates, truncated)) = collect_recent_candidates(&root, cwd, now, within) else {
        return RecentProbe::Incomplete;
    };
    let candidate = candidates.into_iter().find_map(|candidate| {
        let metadata = read_head(&candidate.root, &candidate.path)?;
        let native_id = metadata.id?;
        (metadata.cwd.as_deref() == Some(cwd.to_string_lossy().as_ref())).then_some(
            RecentCandidate {
                tool: ForeignSessionTool::Omp,
                source: ForeignSessionSource::OmpCli,
                native_id,
                updated_at: candidate.modified,
            },
        )
    });
    if candidate.is_none() && truncated {
        RecentProbe::Incomplete
    } else {
        RecentProbe::Complete(candidate)
    }
}

fn collect_candidates(
    root: &ApprovedRoot,
    cwd: &Path,
    now: SystemTime,
    within: Duration,
    limit: usize,
) -> Vec<Candidate> {
    let mut candidates = Vec::with_capacity(limit);
    for directory_name in session_directory_names(cwd) {
        let directory_path = root.join(&directory_name);
        let Some(directory_root) = root.subroot(&directory_path) else {
            continue;
        };
        let mut directory_candidates = Vec::with_capacity(limit);
        let outcome = directory_root.for_each_entry_bounded(MAX_DIRECTORY_ENTRIES, |name| {
            let path = directory_root.join(&name);
            if !is_session_filename(&path) {
                return;
            }
            let Some((path, metadata)) = directory_root.resolve_regular_file(&path) else {
                return;
            };
            if metadata.len() == 0 {
                return;
            }
            let Ok(modified) = metadata.modified() else {
                return;
            };
            if !is_within(modified, now, within) {
                return;
            }
            retain_top_k_by(
                &mut directory_candidates,
                Candidate {
                    root: directory_root.clone(),
                    path,
                    modified,
                },
                limit,
                candidate_order,
            );
        });
        if !outcome.complete {
            continue;
        }
        for candidate in directory_candidates {
            retain_top_k_by(&mut candidates, candidate, limit, candidate_order);
        }
    }
    candidates
}

fn collect_recent_candidates(
    root: &ApprovedRoot,
    cwd: &Path,
    now: SystemTime,
    within: Duration,
) -> Option<(Vec<Candidate>, bool)> {
    let mut candidates = Vec::with_capacity(MAX_RECENT_METADATA_READS);
    let mut qualifying = 0;
    for directory_name in session_directory_names(cwd) {
        let directory_path = root.join(&directory_name);
        let Some(directory_root) = root.subroot(&directory_path) else {
            continue;
        };
        let outcome = directory_root.for_each_entry_bounded(MAX_DIRECTORY_ENTRIES, |name| {
            let path = directory_root.join(&name);
            if !is_session_filename(&path) {
                return;
            }
            let Some((path, metadata)) = directory_root.resolve_regular_file(&path) else {
                return;
            };
            if metadata.len() == 0 {
                return;
            }
            let Ok(modified) = metadata.modified() else {
                return;
            };
            if !is_within(modified, now, within) {
                return;
            }
            qualifying += 1;
            retain_top_k_by(
                &mut candidates,
                Candidate {
                    root: directory_root.clone(),
                    path,
                    modified,
                },
                MAX_RECENT_METADATA_READS,
                candidate_order,
            );
        });
        if !outcome.complete {
            return None;
        }
    }
    Some((candidates, qualifying > MAX_RECENT_METADATA_READS))
}

fn candidate_order(left: &Candidate, right: &Candidate) -> std::cmp::Ordering {
    right
        .modified
        .cmp(&left.modified)
        .then_with(|| left.path.cmp(&right.path))
}

fn read_candidate(candidate: Candidate, requested_cwd: &Path) -> Option<ForeignSessionSummary> {
    let metadata = read_head(&candidate.root, &candidate.path)?;
    let stored_cwd = metadata.cwd?;
    if Path::new(&stored_cwd) != requested_cwd {
        return None;
    }
    let native_id = metadata.id?;
    let title = [
        metadata.title_slot,
        metadata.header_title,
        metadata.short_summary,
        metadata.first_user_message,
    ]
    .into_iter()
    .flatten()
    .find_map(|value| normalize_title(&value))
    .or_else(|| normalize_title(&native_id))?;
    Some(ForeignSessionSummary {
        tool: ForeignSessionTool::Omp,
        source: ForeignSessionSource::OmpCli,
        native_id,
        title,
        cwd: PathBuf::from(stored_cwd),
        updated_at: candidate.modified,
        branch: None,
    })
}

fn read_head(root: &ApprovedRoot, path: &Path) -> Option<HeadMetadata> {
    let file = root.open_regular_file(path)?.file;
    let mut reader = BufReader::new(file.take(MAX_HEAD_BYTES as u64));
    let mut metadata = HeadMetadata::default();
    let mut line = String::new();
    for _ in 0..MAX_HEAD_RECORDS {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("title") => {
                metadata.title_slot = string_field(&record, "title");
            }
            Some("session") => {
                metadata.id = string_field(&record, "id");
                metadata.cwd = string_field(&record, "cwd");
                metadata.header_title = string_field(&record, "title");
            }
            Some("message") if metadata.first_user_message.is_none() => {
                let Some(message) = record.get("message") else {
                    continue;
                };
                if message.get("role").and_then(Value::as_str) == Some("user") {
                    metadata.first_user_message = message
                        .get("content")
                        .and_then(text_content)
                        .and_then(|text| normalize_title(&text));
                }
            }
            Some("compaction") => {
                if let Some(summary) = string_field(&record, "shortSummary") {
                    metadata.short_summary = Some(summary);
                }
            }
            _ => {}
        }
    }
    metadata.id.as_ref()?;
    metadata.cwd.as_ref()?;
    Some(metadata)
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

fn text_content(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let text = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn is_session_filename(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
}

fn session_directory_names(cwd: &Path) -> Vec<String> {
    let resolved_cwd = dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let current = current_session_directory_name(&resolved_cwd);
    let legacy = legacy_session_directory_name(&resolved_cwd);
    if current == legacy {
        vec![current]
    } else {
        vec![current, legacy]
    }
}

fn current_session_directory_name(cwd: &Path) -> String {
    let home = dirs::home_dir().and_then(|path| dunce::canonicalize(path).ok());
    if let Some(relative) = home.as_ref().and_then(|home| cwd.strip_prefix(home).ok()) {
        return encode_relative_directory("-", relative);
    }
    let temporary = dunce::canonicalize(std::env::temp_dir()).ok();
    if let Some(relative) = temporary
        .as_ref()
        .and_then(|temporary| cwd.strip_prefix(temporary).ok())
    {
        return encode_relative_directory("-tmp", relative);
    }
    legacy_session_directory_name(cwd)
}

fn encode_relative_directory(prefix: &str, relative: &Path) -> String {
    let encoded = encode_path(relative);
    if encoded.is_empty() {
        prefix.to_owned()
    } else if prefix.ends_with('-') {
        format!("{prefix}{encoded}")
    } else {
        format!("{prefix}-{encoded}")
    }
}

fn legacy_session_directory_name(cwd: &Path) -> String {
    let path = cwd.to_string_lossy();
    let trimmed = path.trim_start_matches(['/', '\\']);
    format!("--{}--", encode_path(Path::new(trimmed)))
}

fn encode_path(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' => '-',
            other => other,
        })
        .collect()
}

fn sessions_root() -> Option<PathBuf> {
    let profile = profile_name();
    let home = dirs::home_dir()?;
    let config_name = std::env::var_os("PI_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| ".omp".into());
    if profile.is_none()
        && let Some(agent_dir) =
            std::env::var_os("PI_CODING_AGENT_DIR").filter(|value| !value.is_empty())
    {
        let agent_dir = resolve_from_current_dir(PathBuf::from(agent_dir))?;
        if !is_suppressed_profile_agent_dir(&agent_dir, &home, &config_name) {
            return Some(agent_dir.join("sessions"));
        }
    }
    let mut config_root = home.join(&config_name);
    if let Some(profile) = profile.as_deref() {
        config_root = config_root.join("profiles").join(profile);
    }
    if cfg!(any(target_os = "linux", target_os = "macos"))
        && let Some(xdg_root) = xdg_sessions_root(profile.as_deref())
    {
        return Some(xdg_root);
    }
    Some(config_root.join("agent").join("sessions"))
}

fn xdg_sessions_root(profile: Option<&str>) -> Option<PathBuf> {
    let xdg_data_home = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty())?;
    let mut xdg_root = PathBuf::from(xdg_data_home).join("omp");
    if let Some(profile) = profile {
        xdg_root = xdg_root.join("profiles").join(profile);
    }
    // OMP checks for existence. Requiring a directory is an intentional
    // safety tightening for this read-only foreign-session scanner.
    xdg_root.is_dir().then(|| xdg_root.join("sessions"))
}

fn resolve_from_current_dir(path: PathBuf) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path)
    } else {
        Some(std::env::current_dir().ok()?.join(path))
    }
}

fn is_suppressed_profile_agent_dir(agent_dir: &Path, home: &Path, config_name: &OsStr) -> bool {
    let Some(omp_profile) = std::env::var_os("OMP_PROFILE") else {
        return false;
    };
    let omp_profile = omp_profile.to_string_lossy();
    let omp_profile = omp_profile.trim();
    if !omp_profile.is_empty() && omp_profile != "default" {
        return false;
    }
    let Some(pi_profile) = std::env::var_os("PI_PROFILE") else {
        return false;
    };
    let pi_profile = pi_profile.to_string_lossy();
    let pi_profile = pi_profile.trim();
    if pi_profile.is_empty() || pi_profile == "default" || !valid_profile_name(pi_profile) {
        return false;
    }
    let inherited = home
        .join(config_name)
        .join("profiles")
        .join(pi_profile.as_ref() as &str)
        .join("agent");
    agent_dir == inherited
}

fn profile_name() -> Option<String> {
    let value = std::env::var_os("OMP_PROFILE").or_else(|| std::env::var_os("PI_PROFILE"))?;
    let value = value.to_string_lossy();
    let value = value.trim();
    if value.is_empty() || value == "default" || !valid_profile_name(value) {
        return None;
    }
    Some(value.to_owned())
}

fn valid_profile_name(value: &str) -> bool {
    if value.len() > 64 || value == "." || value == ".." || value.ends_with('.') {
        return false;
    }
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && characters.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '_' | '-')
        })
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use filetime::FileTime;

    use super::*;

    fn write_session(path: &Path, id: &str, cwd: &Path, title: &str, modified: SystemTime) {
        let parent = path.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": id,
            "timestamp": "2026-07-27T00:00:00Z",
            "cwd": cwd,
            "title": title,
        });
        let message = serde_json::json!({
            "type": "message",
            "id": "entry001",
            "parentId": null,
            "timestamp": "2026-07-27T00:00:01Z",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": "first request"}],
                "timestamp": 1,
            },
        });
        std::fs::write(path, format!("{header}\n{message}\n")).unwrap();
        filetime::set_file_mtime(path, FileTime::from_system_time(modified)).unwrap();
    }

    #[test]
    fn directory_names_match_home_temp_and_legacy_layouts() {
        let home = dirs::home_dir().unwrap();
        let home_cwd = home.join("Projects").join("repo");
        assert_eq!(current_session_directory_name(&home_cwd), "-Projects-repo");

        let temp_cwd = std::env::temp_dir().join("work").join("repo");
        assert!(current_session_directory_name(&temp_cwd).starts_with("-tmp-"));

        let root = PathBuf::from(std::path::MAIN_SEPARATOR.to_string());
        assert_eq!(
            legacy_session_directory_name(&root.join("workspace")),
            "--workspace--"
        );
    }

    #[test]
    fn scan_reads_current_title_and_filters_other_workspaces() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("repo");
        let other = root.path().join("other");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let sessions_root = root.path().join("sessions");
        let bucket = sessions_root.join(session_directory_names(&cwd)[0].clone());
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        write_session(
            &bucket.join("new_session-id.jsonl"),
            "session-id",
            &cwd,
            "Current OMP title",
            now,
        );
        write_session(
            &bucket.join("other_other-id.jsonl"),
            "other-id",
            &other,
            "Other workspace",
            now - Duration::from_secs(1),
        );

        let sessions = scan_in_sessions_root(&sessions_root, &cwd, now);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].native_id, "session-id");
        assert_eq!(sessions[0].title, "Current OMP title");
        assert_eq!(sessions[0].tool, ForeignSessionTool::Omp);
        assert_eq!(sessions[0].source, ForeignSessionSource::OmpCli);
    }

    #[test]
    fn scan_skips_malformed_message_record_without_dropping_session() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let sessions_root = root.path().join("sessions");
        let bucket = sessions_root.join(session_directory_names(&cwd)[0].clone());
        std::fs::create_dir_all(&bucket).unwrap();
        let path = bucket.join("broken_session-id.jsonl");
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "session-id",
            "cwd": cwd,
            "title": "Still visible",
        });
        let malformed_message = serde_json::json!({
            "type": "message",
            "id": "broken-entry",
        });
        std::fs::write(&path, format!("{header}\n{malformed_message}\n")).unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        filetime::set_file_mtime(&path, FileTime::from_system_time(now)).unwrap();

        let sessions = scan_in_sessions_root(&sessions_root, &cwd, now);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].native_id, "session-id");
        assert_eq!(sessions[0].title, "Still visible");
    }

    #[test]
    fn recent_probe_returns_newest_matching_session() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let sessions_root = root.path().join("sessions");
        let bucket = sessions_root.join(session_directory_names(&cwd)[0].clone());
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        write_session(
            &bucket.join("old_old-id.jsonl"),
            "old-id",
            &cwd,
            "Old",
            now - Duration::from_secs(20),
        );
        write_session(
            &bucket.join("new_new-id.jsonl"),
            "new-id",
            &cwd,
            "New",
            now - Duration::from_secs(5),
        );

        let recent =
            most_recent_in_sessions_root(&sessions_root, &cwd, now, Duration::from_secs(60))
                .unwrap();
        assert_eq!(recent.native_id, "new-id");
        assert_eq!(recent.tool, ForeignSessionTool::Omp);
        assert_eq!(recent.updated_at, now - Duration::from_secs(5));
    }

    /// Serialize every `sessions_root()` env test against the crate-wide env
    /// lock: the hazard is the global `environ` array under `unsafe set_var`,
    /// so even disjoint vars must serialize. Hold the lock FIRST so it drops
    /// LAST, after the `TestEnvGuard` restores run.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn sessions_root_honors_pi_coding_agent_dir_without_profile() {
        let _lock = env_lock();
        // No profile must be active so the PI_CODING_AGENT_DIR short-circuit
        // fires before `dirs::home_dir()`.
        let _omp_profile = crate::TestEnvGuard::unset("OMP_PROFILE");
        let _pi_profile = crate::TestEnvGuard::unset("PI_PROFILE");
        let temp = tempfile::tempdir().unwrap();
        let agent_dir = temp.path().to_path_buf();
        let _pi_coding_agent_dir = crate::TestEnvGuard::set("PI_CODING_AGENT_DIR", &agent_dir);

        let root = sessions_root().expect("PI_CODING_AGENT_DIR should resolve a root");
        assert_eq!(root, agent_dir.join("sessions"));
    }

    #[test]
    fn sessions_root_skips_empty_pi_coding_agent_dir_and_falls_back() {
        let _lock = env_lock();
        let _omp_profile = crate::TestEnvGuard::unset("OMP_PROFILE");
        let _pi_profile = crate::TestEnvGuard::unset("PI_PROFILE");
        // An empty PI_CODING_AGENT_DIR must NOT short-circuit: the filter
        // rejects it, so `sessions_root` falls through to the config path.
        let _pi_coding_agent_dir =
            crate::TestEnvGuard::set("PI_CODING_AGENT_DIR", std::path::Path::new(""));
        // Pin XDG at a non-directory so the XDG branch cannot fire either.
        let _xdg_data_home =
            crate::TestEnvGuard::set("XDG_DATA_HOME", std::path::Path::new("/nonexistent-xdg"));

        let root = sessions_root().expect("empty PI_CODING_AGENT_DIR should fall back");
        assert!(
            root != PathBuf::from("").join("sessions"),
            "empty PI_CODING_AGENT_DIR must not yield a relative sessions root, \
             got {root:?}",
        );
    }

    #[test]
    fn sessions_root_ignores_pi_coding_agent_dir_when_profile_set() {
        let _lock = env_lock();
        let temp = tempfile::tempdir().unwrap();
        let agent_dir = temp.path().to_path_buf();
        let _pi_coding_agent_dir = crate::TestEnvGuard::set("PI_CODING_AGENT_DIR", &agent_dir);
        // A non-default profile must disable the PI_CODING_AGENT_DIR branch.
        let _omp_profile = crate::TestEnvGuard::set("OMP_PROFILE", std::path::Path::new("work"));
        let _pi_profile = crate::TestEnvGuard::unset("PI_PROFILE");
        let _xdg_data_home = crate::TestEnvGuard::unset("XDG_DATA_HOME");

        // Without HOME isolation we cannot pin the absolute path, but the
        // profile suffix must NOT equal the PI_CODING_AGENT_DIR layout.
        let root = sessions_root();
        assert!(
            root.is_some(),
            "sessions_root should still resolve via the home config path"
        );
        assert_ne!(
            root,
            Some(agent_dir.join("sessions")),
            "profile must suppress the PI_CODING_AGENT_DIR short-circuit"
        );
    }

    #[test]
    fn sessions_root_ignores_inherited_profile_agent_dir_for_explicit_default() {
        let _lock = env_lock();
        let home = dirs::home_dir().unwrap();
        let config_name = std::path::Path::new(".omp-profile-test");
        let inherited = home
            .join(config_name)
            .join("profiles")
            .join("work")
            .join("agent");
        let _pi_config_dir = crate::TestEnvGuard::set("PI_CONFIG_DIR", config_name);
        let _omp_profile = crate::TestEnvGuard::set("OMP_PROFILE", std::path::Path::new(""));
        let _pi_profile = crate::TestEnvGuard::set("PI_PROFILE", std::path::Path::new("work"));
        let _pi_coding_agent_dir = crate::TestEnvGuard::set("PI_CODING_AGENT_DIR", &inherited);
        let _xdg_data_home = crate::TestEnvGuard::unset("XDG_DATA_HOME");

        let root = sessions_root().expect("explicit default profile should resolve a root");
        assert_eq!(root, home.join(config_name).join("agent").join("sessions"));
    }

    #[test]
    fn relative_pi_coding_agent_dir_resolves_from_current_directory() {
        let _lock = env_lock();
        let _omp_profile = crate::TestEnvGuard::unset("OMP_PROFILE");
        let _pi_profile = crate::TestEnvGuard::unset("PI_PROFILE");
        let _pi_coding_agent_dir = crate::TestEnvGuard::set(
            "PI_CODING_AGENT_DIR",
            std::path::Path::new("relative-agent"),
        );

        let root = sessions_root().expect("relative agent override should resolve a root");
        assert_eq!(
            root,
            std::env::current_dir()
                .unwrap()
                .join("relative-agent")
                .join("sessions")
        );
    }

    #[test]
    fn xdg_sessions_root_rejects_empty_data_home() {
        let _lock = env_lock();
        let _xdg_data_home = crate::TestEnvGuard::set("XDG_DATA_HOME", std::path::Path::new(""));
        assert_eq!(xdg_sessions_root(None), None);
    }

    #[test]
    fn sessions_root_prefers_xdg_data_home_when_omp_dir_exists() {
        let _lock = env_lock();
        // No profile and no PI_CODING_AGENT_DIR so we reach the XDG branch.
        let _omp_profile = crate::TestEnvGuard::unset("OMP_PROFILE");
        let _pi_profile = crate::TestEnvGuard::unset("PI_PROFILE");
        let _pi_coding_agent_dir = crate::TestEnvGuard::unset("PI_CODING_AGENT_DIR");

        let xdg = tempfile::tempdir().unwrap();
        let omp_root = xdg.path().join("omp");
        std::fs::create_dir_all(&omp_root).unwrap();
        let _xdg_data_home = crate::TestEnvGuard::set("XDG_DATA_HOME", xdg.path());

        let root = sessions_root().expect("XDG branch should resolve a root");
        assert_eq!(root, omp_root.join("sessions"));
    }

    #[test]
    fn sessions_root_falls_back_to_config_root_when_xdg_dir_missing() {
        let _lock = env_lock();
        let _omp_profile = crate::TestEnvGuard::unset("OMP_PROFILE");
        let _pi_profile = crate::TestEnvGuard::unset("PI_PROFILE");
        let _pi_coding_agent_dir = crate::TestEnvGuard::unset("PI_CODING_AGENT_DIR");

        // Point XDG_DATA_HOME at an empty dir so `$XDG_DATA_HOME/omp` is NOT a
        // directory; the fallback config path must be used instead.
        let xdg = tempfile::tempdir().unwrap();
        let _xdg_data_home = crate::TestEnvGuard::set("XDG_DATA_HOME", xdg.path());
        let _pi_config_dir =
            crate::TestEnvGuard::set("PI_CONFIG_DIR", std::path::Path::new(".omp-test"));

        let root = sessions_root().expect("config fallback should resolve a root");
        assert!(
            root.ends_with(".omp-test/agent/sessions"),
            "config root should embed PI_CONFIG_DIR, got {root:?}"
        );
        assert!(
            !root.starts_with(xdg.path()),
            "XDG branch must not fire when the omp dir is absent"
        );
    }

    #[test]
    fn sessions_root_embeds_profile_under_xdg_and_config_paths() {
        let _lock = env_lock();
        let _pi_coding_agent_dir = crate::TestEnvGuard::unset("PI_CODING_AGENT_DIR");
        let _pi_profile = crate::TestEnvGuard::unset("PI_PROFILE");
        let _omp_profile = crate::TestEnvGuard::set("OMP_PROFILE", std::path::Path::new("work"));

        // XDG branch with a profile: `$XDG_DATA_HOME/omp/profiles/<profile>/sessions`.
        let xdg = tempfile::tempdir().unwrap();
        let omp_profile = xdg.path().join("omp").join("profiles").join("work");
        std::fs::create_dir_all(&omp_profile).unwrap();
        let _xdg_data_home = crate::TestEnvGuard::set("XDG_DATA_HOME", xdg.path());

        let root = sessions_root().expect("XDG-with-profile should resolve a root");
        assert_eq!(root, omp_profile.join("sessions"));
    }

    #[test]
    fn profile_name_rejects_default_empty_and_invalid_names() {
        // valid_profile_name and profile_name are pure given the env; exercise
        // the validator directly to avoid env mutation for the rejection cases.
        assert!(!valid_profile_name(""));
        // `default` is syntactically valid but profile_name() treats it as
        // the explicit default-profile sentinel before validation.
        assert!(valid_profile_name("default"));
        assert!(!valid_profile_name("."));
        assert!(!valid_profile_name(".."));
        assert!(!valid_profile_name("work."));
        assert!(!valid_profile_name("Work"));
        assert!(!valid_profile_name("-work"));
        assert!(!valid_profile_name(&"a".repeat(65)));
        assert!(valid_profile_name("work"));
        assert!(valid_profile_name("work-2"));
        assert!(valid_profile_name("a.b_c-d"));
    }

    #[test]
    fn profile_name_uses_omp_profile_then_pi_profile() {
        let _lock = env_lock();
        // Reset both vars to a known-unset baseline before setting, so the
        // restore-on-drop logic cannot leak a stale value from another test.
        let _baseline_omp = crate::TestEnvGuard::unset("OMP_PROFILE");
        let _baseline_pi = crate::TestEnvGuard::unset("PI_PROFILE");
        let _omp = crate::TestEnvGuard::set("OMP_PROFILE", std::path::Path::new("alpha"));
        let _pi = crate::TestEnvGuard::set("PI_PROFILE", std::path::Path::new("beta"));
        assert_eq!(profile_name().as_deref(), Some("alpha"));

        drop(_omp);
        assert_eq!(profile_name().as_deref(), Some("beta"));
    }
}
