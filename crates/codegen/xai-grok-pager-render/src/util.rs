//! Shared utility functions.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use xai_grok_config::grok_home;

/// Path to `$GROK_HOME/pager.toml`.
pub fn pager_toml_path() -> PathBuf {
    grok_home().join("pager.toml")
}

/// User-facing label for the user grok directory (``~/.grok`` or ``$GROK_HOME``).
///
/// Derived from resolved [`grok_home()`] vs `xai_grok_config::default_grok_home()`,
/// not from whether `GROK_HOME` is set in the environment.
pub fn display_grok_home_prefix() -> String {
    if grok_home() == xai_grok_config::default_grok_home() {
        "~/.grok".to_string()
    } else {
        "$GROK_HOME".to_string()
    }
}

/// User-facing path under [`grok_home()`], e.g. ``~/.grok/config.toml``.
pub fn display_user_grok_path(relative: impl AsRef<Path>) -> String {
    let rel = relative.as_ref();
    let prefix = display_grok_home_prefix();
    if rel.as_os_str().is_empty() {
        return prefix;
    }
    format!("{prefix}/{}", rel.display())
}

/// Abbreviate an absolute path for display: prefer [`grok_home()`], then `$HOME`.
pub fn abbreviate_path(path: &str) -> Cow<'_, str> {
    let path_buf = Path::new(path);
    let grok = grok_home();
    if let Ok(rest) = path_buf.strip_prefix(&grok) {
        let prefix = display_grok_home_prefix();
        if rest.as_os_str().is_empty() {
            return Cow::Owned(prefix);
        }
        return Cow::Owned(format!("{prefix}/{}", rest.display()));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
        && let Some(rest) = path.strip_prefix(&home)
    {
        if rest.is_empty() {
            return Cow::Borrowed("~");
        }
        if rest.starts_with('/') {
            return Cow::Owned(format!("~{rest}"));
        }
    }
    Cow::Borrowed(path)
}

/// True when `path` is under user [`grok_home()`] (not project `{cwd}/.grok`).
pub fn is_under_user_grok_home(path: &Path) -> bool {
    path.starts_with(grok_home())
}

/// Format a duration as a compact human-friendly string.
///
/// Uses consistent rounding for visual stability:
/// - Under 10s: `"5.2s"` (one decimal for granularity)
/// - 10-59s: `"32s"` (no decimal)
/// - 1m-59m: `"2m5s"`
/// - 1h+: `"1h2m"`
pub fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    if total_secs < 10 {
        return format!("{:.1}s", d.as_secs_f64());
    }
    if total_secs < 60 {
        return format!("{total_secs}s");
    }
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    if mins < 60 {
        return format!("{mins}m{secs}s");
    }
    let hours = mins / 60;
    let remaining_mins = mins % 60;
    format!("{hours}h{remaining_mins}m")
}

/// Format a duration as a coarse recency string for "time ago" / age
/// displays (e.g. dashboard row age column and peek panel prefix).
///
/// Buckets chosen for the agent dashboard so the column stays compact
/// and doesn't distract with second-level churn:
/// - < 1 minute: `"just now"`
/// - minutes: `"1m"` … `"59m"`
/// - hours: `"1h"` … `"23h"`
/// - days: `"1d"` … `"29d"`
/// - months (≈30d+): `"1mo"` … `"11mo"`
/// - years (≈365d+): `"1y"` …
pub fn format_time_ago(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        return "just now".to_string();
    }
    if secs < 3600 {
        let mins = secs / 60;
        return format!("{mins}m");
    }
    if secs < 86400 {
        let hours = secs / 3600;
        return format!("{hours}h");
    }
    let days = secs / 86400;
    if days < 30 {
        return format!("{days}d");
    }
    if days < 365 {
        let months = days / 30;
        return format!("{months}mo");
    }
    let years = days / 365;
    format!("{years}y")
}

/// Convert unix-epoch millis into a wall-clock [`SystemTime`].
///
/// Used for dashboard recency that originates as a wall-clock timestamp (the
/// leader roster's `last_change_unix_ms`). A non-positive value — the
/// `#[serde(default)]` `0` sentinel for a missing roster timestamp — falls
/// back to "now".
pub fn system_time_from_unix_ms(unix_ms: i64) -> SystemTime {
    if unix_ms <= 0 {
        return SystemTime::now();
    }
    UNIX_EPOCH
        .checked_add(Duration::from_millis(unix_ms as u64))
        .unwrap_or_else(SystemTime::now)
}

/// Project a monotonic [`Instant`] onto the wall clock as the [`SystemTime`]
/// it corresponds to (`SystemTime::now() - instant.elapsed()`).
///
/// The dashboard stores row recency as a wall-clock `SystemTime` so on-disk
/// roster timestamps (which can predate this process — even the machine's
/// boot — and so are unrepresentable as a monotonic `Instant`) sit in the same
/// comparable space as local rows. Local rows hold live `Instant` anchors;
/// this maps them across. A fixed anchor ages correctly because only `now`
/// advances, and the sub-millisecond skew between the two `now()` samples is
/// invisible to the minute-granularity [`format_time_ago`] buckets.
pub fn system_time_from_instant(instant: Instant) -> SystemTime {
    SystemTime::now()
        .checked_sub(instant.elapsed())
        .unwrap_or_else(SystemTime::now)
}

/// Decode common HTML entities (`&amp;`, `&lt;`, `&gt;`, `&quot;`, `&#39;`)
/// that may appear in LLM-generated session summaries.
pub fn decode_html_entities(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('&') {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = s.to_string();
    out = out.replace("&amp;", "&");
    out = out.replace("&lt;", "<");
    out = out.replace("&gt;", ">");
    out = out.replace("&quot;", "\"");
    out = out.replace("&#39;", "'");
    out = out.replace("&#x27;", "'");
    out = out.replace("&apos;", "'");
    std::borrow::Cow::Owned(out)
}

pub fn parse_schedule_interval_secs(human: &str) -> Option<u64> {
    let s = human.trim_start();
    if !s.starts_with("every ") {
        return None;
    }
    let rest = s[6..].trim_start();
    let (num_str, unit) = if let Some(sp) = rest.find(char::is_whitespace) {
        (&rest[..sp], &rest[sp + 1..])
    } else if rest.len() >= 2 {
        let (d, u) = rest.split_at(rest.len() - 1);
        (d, u)
    } else {
        return None;
    };
    let n: u64 = num_str.parse().ok()?;
    let unit = unit.trim();
    let secs_per = match unit {
        "s" | "second" | "seconds" => 1,
        "m" | "minute" | "minutes" => 60,
        "h" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86400,
        _ => return None,
    };
    Some(n * secs_per)
}

/// Group a count's digits with commas for display: `1234567` → `"1,234,567"`.
pub fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_thousands_inserts_separators() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn subsecond() {
        assert_eq!(format_duration(Duration::from_millis(500)), "0.5s");
    }

    #[test]
    fn under_ten_seconds() {
        assert_eq!(format_duration(Duration::from_secs_f64(5.23)), "5.2s");
    }

    #[test]
    fn ten_seconds_no_decimal() {
        assert_eq!(format_duration(Duration::from_secs(10)), "10s");
    }

    #[test]
    fn seconds_no_decimal() {
        assert_eq!(format_duration(Duration::from_secs_f64(12.3)), "12s");
    }

    #[test]
    fn thirty_seconds() {
        assert_eq!(format_duration(Duration::from_secs(30)), "30s");
    }

    #[test]
    fn minutes() {
        assert_eq!(format_duration(Duration::from_secs(125)), "2m5s");
    }

    #[test]
    fn hours() {
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h2m");
    }

    #[test]
    fn time_ago_just_now() {
        assert_eq!(format_time_ago(Duration::from_secs(0)), "just now");
        assert_eq!(format_time_ago(Duration::from_secs(30)), "just now");
        assert_eq!(format_time_ago(Duration::from_secs(59)), "just now");
    }

    #[test]
    fn time_ago_minutes() {
        assert_eq!(format_time_ago(Duration::from_secs(60)), "1m");
        assert_eq!(format_time_ago(Duration::from_secs(125)), "2m");
        assert_eq!(format_time_ago(Duration::from_secs(3599)), "59m");
    }

    #[test]
    fn time_ago_hours() {
        assert_eq!(format_time_ago(Duration::from_secs(3600)), "1h");
        assert_eq!(format_time_ago(Duration::from_secs(7200)), "2h");
        assert_eq!(format_time_ago(Duration::from_secs(86399)), "23h");
    }

    #[test]
    fn time_ago_days() {
        assert_eq!(format_time_ago(Duration::from_secs(86400)), "1d");
        assert_eq!(format_time_ago(Duration::from_secs(172800)), "2d");
        assert_eq!(format_time_ago(Duration::from_secs(2_592_000 - 1)), "29d"); // just under 30d
    }

    #[test]
    fn time_ago_months() {
        assert_eq!(format_time_ago(Duration::from_secs(2_592_000)), "1mo"); // 30d
        assert_eq!(format_time_ago(Duration::from_secs(5_184_000)), "2mo");
        // 359d is still 11mo (359/30=11); 360d would be 12mo.
        assert_eq!(format_time_ago(Duration::from_secs(359 * 86400)), "11mo");
    }

    #[test]
    fn time_ago_years() {
        assert_eq!(format_time_ago(Duration::from_secs(31_536_000)), "1y"); // 365d
        assert_eq!(format_time_ago(Duration::from_secs(63_072_000)), "2y");
    }

    fn now_unix_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    /// A real past timestamp survives the round-trip and renders its true age —
    /// including ages beyond the machine's uptime, which a monotonic `Instant`
    /// could not represent (its floor is system boot).
    #[test]
    fn system_time_from_unix_ms_renders_real_age() {
        let two_hours_ago = now_unix_ms() - 2 * 3_600_000;
        let elapsed = system_time_from_unix_ms(two_hours_ago)
            .elapsed()
            .unwrap_or_default();
        assert_eq!(format_time_ago(elapsed), "2h");

        let forty_five_days_ago = now_unix_ms() - 45 * 86_400_000;
        let elapsed = system_time_from_unix_ms(forty_five_days_ago)
            .elapsed()
            .unwrap_or_default();
        assert_eq!(format_time_ago(elapsed), "1mo");
    }

    /// A zero / missing timestamp (the `#[serde(default)]` sentinel) falls back
    /// to "now" rather than the unix epoch (1970).
    #[test]
    fn system_time_from_unix_ms_zero_falls_back_to_now() {
        let elapsed = system_time_from_unix_ms(0).elapsed().unwrap_or_default();
        assert!(elapsed.as_secs() < 5, "zero sentinel must fall back to now");
    }

    /// A future timestamp (clock skew) renders as "just now": `elapsed()` errors
    /// on a future `SystemTime`, and callers default that to a zero duration.
    #[test]
    fn system_time_from_unix_ms_future_renders_just_now() {
        let future = now_unix_ms() + 10_000_000;
        let elapsed = system_time_from_unix_ms(future)
            .elapsed()
            .unwrap_or_default();
        assert_eq!(format_time_ago(elapsed), "just now");
    }

    /// A fixed `Instant` projects to a stable wall-clock moment, so its age
    /// reflects time-since-anchor (here ~10m) rather than re-anchoring to now.
    #[test]
    fn system_time_from_instant_reflects_elapsed() {
        let ten_min_ago = Instant::now() - Duration::from_secs(600);
        let elapsed = system_time_from_instant(ten_min_ago)
            .elapsed()
            .unwrap_or_default();
        assert_eq!(format_time_ago(elapsed), "10m");
    }

    #[test]
    fn parses_every_5_minutes() {
        assert_eq!(parse_schedule_interval_secs("every 5 minutes"), Some(300));
    }

    #[test]
    fn parses_every_5m_short() {
        assert_eq!(parse_schedule_interval_secs("every 5m"), Some(300));
    }

    #[test]
    fn parses_every_10s() {
        assert_eq!(parse_schedule_interval_secs("every 10s"), Some(10));
    }

    #[test]
    fn parses_every_1_hour() {
        assert_eq!(parse_schedule_interval_secs("every 1 hour"), Some(3600));
    }

    #[test]
    fn parses_every_1_day() {
        assert_eq!(parse_schedule_interval_secs("every 1 day"), Some(86400));
    }

    #[test]
    fn decode_html_entities_no_entities() {
        let s = "hello world";
        let out = decode_html_entities(s);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), s);
    }

    #[test]
    fn decode_html_entities_amp() {
        assert_eq!(decode_html_entities("foo &amp; bar").as_ref(), "foo & bar");
    }

    #[test]
    fn decode_html_entities_multiple() {
        assert_eq!(
            decode_html_entities("1 &lt; 2 &amp;&amp; 3 &gt; 2").as_ref(),
            "1 < 2 && 3 > 2"
        );
    }

    #[test]
    fn decode_html_entities_quotes() {
        assert_eq!(
            decode_html_entities("&quot;hello&quot; &amp; &#39;world&#39;").as_ref(),
            "\"hello\" & 'world'"
        );
    }

    #[test]
    fn unknown_schedule_returns_none() {
        assert_eq!(parse_schedule_interval_secs("foo bar"), None);
        assert_eq!(parse_schedule_interval_secs("every foo"), None);
        assert_eq!(parse_schedule_interval_secs("every 5x"), None);
    }

    #[test]
    fn display_grok_home_prefix_default_install() {
        if std::env::var("GROK_HOME").is_ok() {
            return;
        }
        assert_eq!(display_grok_home_prefix(), "~/.grok");
    }

    #[test]
    fn display_user_grok_path_joins_relative() {
        let path = display_user_grok_path("config.toml");
        assert!(path.ends_with("/config.toml") || path.ends_with("\\config.toml"));
        assert!(path.contains(".grok") || path.contains("$GROK_HOME"));
    }

    #[test]
    fn abbreviate_path_uses_home_when_under_default_grok() {
        if let Ok(home) = std::env::var("HOME") {
            if home.is_empty() {
                return;
            }
            let full = format!("{home}/.grok/memory/MEMORY.md");
            let abbreviated = abbreviate_path(&full);
            assert!(
                abbreviated.contains("memory/MEMORY.md"),
                "got {abbreviated}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn abbreviate_path_empty_home_does_not_fake_tilde() {
        let prev = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", "");
        }
        assert_eq!(abbreviate_path("/foo").as_ref(), "/foo");

        match prev {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}
