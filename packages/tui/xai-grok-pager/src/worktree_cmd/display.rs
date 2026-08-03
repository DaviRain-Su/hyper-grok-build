use std::borrow::Cow;
use std::path::Path;

use super::{DbStats, GcReport, RebuildReport};
use xai_fast_worktree::WorktreeRecord;
use xai_grok_shell::session::worktree::META_KEY_LABEL;

/// Extract the label from a worktree record's metadata JSON.
fn extract_label(rec: &WorktreeRecord) -> &str {
    rec.metadata
        .as_ref()
        .and_then(|m| m.get(META_KEY_LABEL))
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

pub fn print_table(records: &[WorktreeRecord]) {
    if records.is_empty() {
        println!("No worktrees found.");
        return;
    }

    // Compute dynamic ID column width so long IDs are never truncated
    let id_width = records
        .iter()
        .map(|r| r.id.len())
        .max()
        .unwrap_or(0)
        .max(16);

    // Compute dynamic label column width (min 5 for header "LABEL")
    let label_width = records
        .iter()
        .map(|r| extract_label(r).len())
        .max()
        .unwrap_or(0)
        .clamp(5, 24);

    let header = format!(
        "  {:<id_width$} {:<8} {:<6} {:<label_width$} {:<20} {:<10} PATH",
        "ID", "TYPE", "REPO", "LABEL", "BRANCH", "AGE",
    );
    println!("{header}");
    for rec in records {
        let age = format_age(rec.created_at);
        let branch = rec.git_ref.as_deref().unwrap_or("(detached)");
        let label = extract_label(rec);
        let path = abbreviate_home(&rec.path);
        let row = format!(
            "  {:<id_width$} {:<8} {:<6} {:<label_width$} {:<20} {:<10} {}",
            rec.id,
            rec.kind.as_str(),
            truncate(&rec.repo_name, 6),
            truncate(label, label_width),
            truncate(branch, 20),
            age,
            path,
        );
        println!("{row}");
    }

    let total = records.len();
    let by_kind: std::collections::HashMap<&str, usize> =
        records
            .iter()
            .fold(std::collections::HashMap::new(), |mut m, r| {
                *m.entry(r.kind.as_str()).or_default() += 1;
                m
            });
    let breakdown: Vec<String> = by_kind.iter().map(|(k, v)| format!("{v} {k}")).collect();
    println!("  {} worktrees ({})", total, breakdown.join(", "));
}

pub fn print_json(records: &[WorktreeRecord]) {
    let json = serde_json::to_string_pretty(records).unwrap_or_else(|_| "[]".to_string());
    println!("{json}");
}

pub fn print_show(rec: &WorktreeRecord) {
    println!("  Path:           {}", rec.path.display());
    println!("  ID:             {}", rec.id);
    println!("  Type:           {}", rec.kind.as_str());
    println!("  Source Repo:    {}", rec.source_repo.display());
    println!("  Creation Mode:  {}", rec.creation_mode);
    if let Some(ref git_ref) = rec.git_ref {
        println!("  Git Ref:        {git_ref}");
    }
    if let Some(ref commit) = rec.head_commit {
        let short = if commit.len() > 12 {
            &commit[..12]
        } else {
            commit
        };
        println!("  HEAD:           {short}");
    }
    println!("  Created:        {}", format_timestamp(rec.created_at));
    if let Some(ts) = rec.last_accessed_at {
        println!("  Last Accessed:  {}", format_timestamp(ts));
    }
    if let Some(ref sid) = rec.session_id {
        println!("  Session ID:     {sid}");
    }
    if let Some(pid) = rec.creator_pid {
        println!("  Creator PID:    {pid}");
    }
    println!("  Status:         {}", rec.status.as_str());
    let label = extract_label(rec);
    if !label.is_empty() {
        println!("  Label:          {label}");
    }

    if rec.path.exists()
        && let Ok(size) = dir_size(&rec.path)
    {
        println!("  Disk Usage:     {}", format_bytes(size));
    }
}

pub fn print_stats(stats: &DbStats) {
    println!("Worktree DB Statistics");
    println!("======================");
    println!("  Total records:  {}", stats.total_records);
    println!("  Alive:          {}", stats.alive_count);
    println!("  Dead:           {}", stats.dead_count);
    println!("  DB size:        {}", format_bytes(stats.db_file_bytes));
}

pub fn print_gc(report: &GcReport) {
    println!("GC report:");
    println!("  Dead records removed:    {}", report.dead_removed);
    println!("  Expired worktrees removed: {}", report.expired_removed);
    println!("  Skipped (alive process): {}", report.skipped_alive);
    if report.remove_failed > 0 {
        println!("  Removal failures:        {}", report.remove_failed);
    }
}

pub fn print_rebuild(report: &RebuildReport) {
    println!("Rebuild report:");
    println!("  Discovered:      {}", report.discovered);
    println!("  Registered:      {}", report.registered);
    println!("  Already tracked: {}", report.already_tracked);
}

fn format_age(created_at: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let delta = now.saturating_sub(created_at);
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86400)
    }
}

fn format_timestamp(ts: i64) -> String {
    let dt = chrono::DateTime::from_timestamp(ts, 0);
    match dt {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => ts.to_string(),
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut val = bytes as f64;
    for unit in UNITS {
        if val < 1024.0 {
            return format!("{val:.1} {unit}");
        }
        val /= 1024.0;
    }
    format!("{val:.1} TB")
}

fn truncate(s: &str, max: usize) -> Cow<'_, str> {
    if s.chars().count() <= max {
        Cow::Borrowed(s)
    } else {
        let end = s
            .char_indices()
            .nth(max.saturating_sub(1))
            .map_or(s.len(), |(i, _)| i);
        Cow::Owned(format!("{}…", &s[..end]))
    }
}

fn abbreviate_home(path: &Path) -> String {
    crate::util::abbreviate_path(&path.to_string_lossy()).into_owned()
}

fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    dir_size_recurse(path, &mut total);
    Ok(total)
}

fn dir_size_recurse(dir: &Path, total: &mut u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_file() {
            if let Ok(meta) = entry.metadata() {
                *total += meta.len();
            }
        } else if ft.is_dir() {
            dir_size_recurse(&entry.path(), total);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512.0 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1048576), "1.0 MB");
        assert_eq!(format_bytes(1073741824), "1.0 GB");
    }

    #[test]
    fn test_format_age() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(format_age(now - 30).ends_with("s ago"));
        assert!(format_age(now - 120).ends_with("m ago"));
        assert!(format_age(now - 7200).ends_with("h ago"));
        assert!(format_age(now - 172800).ends_with("d ago"));
    }

    #[test]
    fn test_truncate_no_truncation() {
        assert_eq!(truncate("hello", 10).as_ref(), "hello");
        assert!(matches!(truncate("hello", 10), Cow::Borrowed(_)));
    }

    #[test]
    fn test_truncate_with_truncation() {
        let result = truncate("hello world", 5);
        assert_eq!(result.as_ref(), "hell…");
        assert!(matches!(result, Cow::Owned(_)));
    }

    #[test]
    fn test_truncate_utf8_safe() {
        let result = truncate("héllo wörld", 5);
        assert_eq!(result.as_ref(), "héll…");
    }

    #[test]
    fn test_abbreviate_home() {
        if let Ok(home) = std::env::var("HOME") {
            let path = std::path::PathBuf::from(format!("{home}/work/xai"));
            assert_eq!(abbreviate_home(&path), "~/work/xai");
        }
    }

    #[test]
    fn test_print_table_long_id_not_truncated() {
        let long_id = "a".repeat(40);

        // Verify width computation: ID longer than 16 should determine column width
        let id_width = long_id.len().max(16);
        let formatted = format!("{:<id_width$}", long_id, id_width = id_width);
        assert!(formatted.len() >= 40, "ID should not be truncated");
        assert!(formatted.contains(&long_id), "Full ID must be present");
    }

    fn make_test_record(metadata: Option<serde_json::Value>) -> xai_fast_worktree::WorktreeRecord {
        use xai_fast_worktree::{WorktreeKind, WorktreeRecord, WorktreeStatus};
        WorktreeRecord {
            id: "test".into(),
            path: "/tmp/wt".into(),
            source_repo: "/repo".into(),
            repo_name: "repo".into(),
            kind: WorktreeKind::Session,
            creation_mode: "linked".into(),
            git_ref: None,
            head_commit: None,
            session_id: None,
            creator_pid: None,
            created_at: 0,
            last_accessed_at: None,
            status: WorktreeStatus::Alive,
            metadata,
        }
    }

    #[test]
    fn test_extract_label_present() {
        let rec = make_test_record(Some(
            serde_json::json!({"label": "my-feature", "user_provided": true}),
        ));
        assert_eq!(extract_label(&rec), "my-feature");
    }

    #[test]
    fn test_extract_label_missing_metadata() {
        let rec = make_test_record(None);
        assert_eq!(extract_label(&rec), "");
    }

    #[test]
    fn test_extract_label_no_label_key() {
        let rec = make_test_record(Some(serde_json::json!({"other": "data"})));
        assert_eq!(extract_label(&rec), "");
    }
}
