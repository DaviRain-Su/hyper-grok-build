//! TTSR-lite — time-traveling stream rules (gated, default off).
//!
//! Loads project rules with a `condition` / `ttsr_trigger` regex and optional
//! `interruptMode`. On each streaming text chunk, if a rule matches the
//! accumulated assistant prose, the stream is cancelled and the rule body is
//! injected once as a system reminder for a single continuation.
//!
//! Full OMP TTSR (AST conditions, tool-only modes, multi-retry budgets) is out
//! of scope for this lite port.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use regex::Regex;

/// Global enable flag (config `ttsr.enabled`, default false).
#[derive(Debug, Clone)]
pub struct TtsrConfig {
    pub enabled: bool,
    /// Max one retry per turn after a trigger.
    pub max_retries_per_turn: u32,
}

impl Default for TtsrConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_retries_per_turn: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptMode {
    /// Never interrupt (rule is informational only).
    Never,
    /// Interrupt on prose (default when condition is set).
    Always,
}

impl InterruptMode {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "never" => Self::Never,
            "always" | "immediate" | "prose-only" | "prose" => Self::Always,
            // tool-only / wait → treat as never for lite (no tool-stream path yet)
            _ => Self::Never,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TtsrRule {
    pub name: String,
    pub path: PathBuf,
    pub condition: Regex,
    pub interrupt_mode: InterruptMode,
    pub body: String,
}

#[derive(Debug, Clone, Default)]
pub struct TtsrEngine {
    pub config: TtsrConfig,
    pub rules: Vec<TtsrRule>,
    /// Already fired once this turn (single retry).
    fired_this_turn: Arc<AtomicBool>,
}

impl TtsrEngine {
    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn load(config: TtsrConfig, project_cwd: &Path) -> Self {
        if !config.enabled {
            return Self {
                config,
                rules: Vec::new(),
                fired_this_turn: Arc::new(AtomicBool::new(false)),
            };
        }
        let rules = load_rules_from_project(project_cwd);
        tracing::info!(
            count = rules.len(),
            cwd = %project_cwd.display(),
            "ttsr-lite: loaded stream rules"
        );
        Self {
            config,
            rules,
            fired_this_turn: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn reset_turn(&self) {
        self.fired_this_turn.store(false, Ordering::SeqCst);
    }

    /// Check accumulated assistant text. Returns rule body to inject if the
    /// stream should be cancelled and retried once.
    pub fn check_stream_text(&self, accumulated: &str) -> Option<TtsrHit> {
        if !self.config.enabled || self.rules.is_empty() {
            return None;
        }
        if self.fired_this_turn.load(Ordering::SeqCst) {
            return None;
        }
        for rule in &self.rules {
            if rule.interrupt_mode == InterruptMode::Never {
                continue;
            }
            if rule.condition.is_match(accumulated) {
                self.fired_this_turn.store(true, Ordering::SeqCst);
                return Some(TtsrHit {
                    rule_name: rule.name.clone(),
                    rule_path: rule.path.clone(),
                    injection: format!(
                        "<ttsr-rule name=\"{}\">\n{}\n</ttsr-rule>\n\n\
                         A stream rule matched your draft. Revise accordingly and continue \
                         (one automatic retry).",
                        rule.name, rule.body
                    ),
                });
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct TtsrHit {
    pub rule_name: String,
    pub rule_path: PathBuf,
    pub injection: String,
}

fn load_rules_from_project(cwd: &Path) -> Vec<TtsrRule> {
    let mut rules = Vec::new();
    let candidates = [
        cwd.join(".grok").join("rules"),
        cwd.join(".cursor").join("rules"),
    ];
    for dir in candidates {
        if !dir.is_dir() {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            let path = ent.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Some(rule) = parse_rule_file(&path) {
                rules.push(rule);
            }
        }
    }
    rules
}

fn parse_rule_file(path: &Path) -> Option<TtsrRule> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (fm, body) = split_frontmatter(&raw)?;
    let condition = fm
        .get("condition")
        .or_else(|| fm.get("ttsr_trigger"))
        .or_else(|| fm.get("ttsrTrigger"))
        .and_then(|v| v.as_str())?;
    let pattern = translate_inline_flags(condition);
    let re = Regex::new(&pattern).ok()?;
    let interrupt_mode = fm
        .get("interruptMode")
        .or_else(|| fm.get("interrupt_mode"))
        .and_then(|v| v.as_str())
        .map(InterruptMode::parse)
        .unwrap_or(InterruptMode::Always);
    let name = fm
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("rule")
                .to_string()
        });
    Some(TtsrRule {
        name,
        path: path.to_path_buf(),
        condition: re,
        interrupt_mode,
        body: body.trim().to_string(),
    })
}

fn split_frontmatter(raw: &str) -> Option<(serde_json::Map<String, serde_json::Value>, String)> {
    let raw = raw.trim_start_matches('\u{feff}');
    if !raw.starts_with("---") {
        return None;
    }
    let after = raw.strip_prefix("---")?;
    let after = after.strip_prefix('\n').or_else(|| after.strip_prefix("\r\n"))?;
    let end = after.find("\n---").or_else(|| after.find("\r\n---"))?;
    let yaml = &after[..end];
    let body = after[end..]
        .trim_start_matches(|c| c == '\r' || c == '\n' || c == '-')
        .to_string();
    // Prefer JSON-compatible YAML via serde_yaml if available; fall back to
    // simple key: value lines.
    let map = parse_simple_yaml_map(yaml);
    if map.is_empty() {
        return None;
    }
    Some((map, body))
}

fn parse_simple_yaml_map(yaml: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for line in yaml.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k = k.trim().to_string();
        let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
        map.insert(k, serde_json::Value::String(v));
    }
    map
}

/// Translate a leading `(?i)` / `(?m)` / `(?s)` group into a form `regex` accepts
/// (`(?i)` is already valid in the `regex` crate).
fn translate_inline_flags(pattern: &str) -> String {
    pattern.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_fires_once() {
        let eng = TtsrEngine {
            config: TtsrConfig {
                enabled: true,
                max_retries_per_turn: 1,
            },
            rules: vec![TtsrRule {
                name: "no-todo".into(),
                path: PathBuf::from("r.md"),
                condition: Regex::new(r"(?i)\bTODO\b").unwrap(),
                interrupt_mode: InterruptMode::Always,
                body: "Do not leave TODOs.".into(),
            }],
            fired_this_turn: Arc::new(AtomicBool::new(false)),
        };
        assert!(eng.check_stream_text("hello").is_none());
        let hit = eng.check_stream_text("I will leave a TODO here").unwrap();
        assert_eq!(hit.rule_name, "no-todo");
        assert!(eng.check_stream_text("another TODO").is_none());
        eng.reset_turn();
        assert!(eng.check_stream_text("TODO again").is_some());
    }

    #[test]
    fn parse_frontmatter_rule() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.md");
        std::fs::write(
            &path,
            "---\nname: demo\ncondition: \"(?i)password\"\ninterruptMode: always\n---\nNever print secrets.\n",
        )
        .unwrap();
        let rule = parse_rule_file(&path).unwrap();
        assert_eq!(rule.name, "demo");
        assert!(rule.condition.is_match("Password: x"));
        assert!(rule.body.contains("Never print"));
    }
}
