use tree_sitter::{Node, Parser, Tree};
use tree_sitter_bash::LANGUAGE as BASH;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BashCommandHighlights {
    pub prefix: Vec<String>,
    // TODO: Whatever the words looked like,, we don't handle the words differently
    pub highlighted_words: Vec<String>,
    pub suffix: Vec<String>,
}

/// Internal representation of a parsed "plain" command:
/// - `words`: the actual command + args (env assignments stripped)
/// - `span_start` / `span_end`: byte range in the original script covering
///   the highlighted command part (command name + args).
#[derive(Debug, Clone)]
pub struct PlainCommand {
    words: Vec<String>,
    span_start: usize,
    span_end: usize,
}

impl PlainCommand {
    /// Returns the command words (command name + args, env assignments stripped).
    pub fn words(&self) -> &[String] {
        &self.words
    }
}

/// Parse the provided bash source using tree-sitter-bash, returning a Tree on
/// success or None if parsing failed.
pub fn try_parse_shell(src: &str) -> Option<Tree> {
    let lang = BASH.into();
    let mut parser = Parser::new();
    #[expect(clippy::expect_used)]
    parser.set_language(&lang).expect("load bash grammar");
    let old_tree: Option<&Tree> = None;
    parser.parse(src, old_tree)
}

/// Parse a script which may contain multiple simple commands joined only by
/// the safe logical/pipe/sequencing operators: `&&`, `||`, `;`, `|`.
///
/// Returns `Some(Vec<PlainCommand>)` if every command is a plain word-only
/// command and the parse tree does not contain disallowed constructs
/// (parentheses, redirections, substitutions, control flow, etc.). Otherwise
/// returns `None`.
pub fn try_parse_word_only_commands_sequence(tree: &Tree, src: &str) -> Option<Vec<PlainCommand>> {
    if tree.root_node().has_error() {
        return None;
    }

    // List of allowed (named) node kinds for a "word only commands sequence".
    // If we encounter a named node that is not in this list we reject.
    const ALLOWED_KINDS: &[&str] = &[
        // top level containers
        "program",
        "list",
        "pipeline",
        // commands & words
        "command",
        "command_name",
        "word",
        "string",
        "string_content",
        "raw_string",
        "number",
        "concatenation",
        // allow simple env var assignments before commands
        "variable_assignment",
        "variable_name",
        // allow redirections (e.g., 2>&1, > file, etc.)
        "redirected_statement",
        "file_redirect",
        "file_descriptor",
        // Comments never execute.
        "comment",
        // Heredoc bodies are stdin data to the (separately classified) head
        // command, not shell-executed text. An unquoted body surfaces
        // `$(...)`/`${...}` as named child nodes outside this allowlist, so
        // substitution smuggling still fails the parse; a `> file` on the same
        // statement stays visible to the write model as a file_redirect.
        // `declaration_command` (`export K=V`) is deliberately ABSENT: it is
        // not a `command` node, so ask-mode segment evaluation (which has no
        // env guard) would never see a PATH/LD_PRELOAD hijack.
        "heredoc_redirect",
        "heredoc_start",
        "heredoc_body",
        "heredoc_content",
        "heredoc_end",
    ];

    // Allow only safe punctuation / operator tokens; anything else causes reject.
    // We add "=" so that VAR=VALUE is allowed.
    // Redirection operators: >, >>, <, >&, &>, etc.
    const ALLOWED_PUNCT_TOKENS: &[&str] = &[
        "&&", "||", ";", "|", "\"", "'", "=", ">", ">>", "<", "<<", ">&", "&>", "&>>",
    ];

    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut stack = vec![root];
    let mut command_nodes = Vec::new();

    while let Some(node) = stack.pop() {
        let kind = node.kind();

        if node.is_named() {
            if !ALLOWED_KINDS.contains(&kind) {
                return None;
            }
            if kind == "command" {
                command_nodes.push(node);
            }
        } else {
            // Reject any punctuation / operator tokens that are not explicitly allowed.
            if kind.chars().any(|c| "&;|".contains(c)) && !ALLOWED_PUNCT_TOKENS.contains(&kind) {
                return None;
            }
            if !(ALLOWED_PUNCT_TOKENS.contains(&kind) || kind.trim().is_empty()) {
                // If it's a quote token or operator it's allowed above; we also allow whitespace tokens.
                // Any other punctuation like parentheses, braces, redirects, backticks, etc are rejected.
                return None;
            }
        }

        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }

    // Walk uses a stack (LIFO), so re-sort by position to restore source order.
    command_nodes.sort_by_key(Node::start_byte);

    let mut commands = Vec::new();
    for node in command_nodes {
        if let Some(cmd) = parse_plain_command_from_node(node, src) {
            commands.push(cmd);
        } else {
            return None;
        }
    }
    Some(commands)
}

/// Classify "setup" or "wrapper" commands you want to skip when looking for the
/// "first important command" (cd/export/sleep/timeout/etc).
pub(crate) fn is_setup_command(cmd: &[String]) -> bool {
    if cmd.is_empty() {
        return true;
    }

    matches!(
        cmd[0].as_str(),
        "cd" | "pushd" | "popd" | "export" | "unset" | "set" | "sleep" | "timeout"
    )
}

/// A GNU `env` `NAME=VALUE` assignment operand (skipped during option scanning).
fn is_env_assignment(tok: &str) -> bool {
    let Some(eq) = tok.find('=') else {
        return false;
    };
    let name = &tok[..eq];
    !name.is_empty()
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Scan a leading `env`'s options/assignments → its `-C`/`--chdir` target (last
/// wins) and the index where the inner command starts. `env` permutes options
/// with `NAME=VALUE`, so both are scanned until the first plain word or `--`.
fn env_scan(cmd: &[String]) -> (Option<&str>, usize) {
    let mut chdir = None;
    let mut i = 1usize;
    while let Some(tok) = cmd.get(i).map(String::as_str) {
        if tok == "--" {
            i += 1;
            break;
        }
        if tok != "-" && tok.starts_with('-') {
            if tok == "-C" || tok == "--chdir" {
                chdir = cmd.get(i + 1).map(String::as_str);
                i += 2;
            } else if let Some(dir) = tok.strip_prefix("--chdir=") {
                chdir = Some(dir);
                i += 1;
            } else if let Some(dir) = tok.strip_prefix("-C").filter(|d| !d.is_empty()) {
                chdir = Some(dir);
                i += 1;
            } else if matches!(tok, "-u" | "--unset" | "-S" | "--split-string") {
                i += 2;
            } else {
                i += 1;
            }
        } else if is_env_assignment(tok) {
            i += 1;
        } else {
            break;
        }
    }
    (chdir, i)
}

/// Strip a leading wrapper (`timeout`/`env`/`nice`/`stdbuf`/`ionice`/`chrt`) and
/// its args, returning the inner command (`None` if not a wrapper). Conservative:
/// under-strip when unsure so `timeout 30 rm -rf /` can't hide behind `timeout`.
pub(crate) fn strip_wrapper_command(cmd: &[String]) -> Option<&[String]> {
    // Basename so a path-qualified wrapper (e.g. `/usr/bin/env`) is still stripped.
    let head = cmd.first()?.rsplit(['/', '\\']).next()?;
    let mut i = 1usize;
    match head {
        // `timeout [OPTIONS] DURATION COMMAND [ARGS]`
        "timeout" => {
            while i < cmd.len() && cmd[i].starts_with('-') {
                if matches!(cmd[i].as_str(), "-k" | "-s" | "--kill-after" | "--signal") {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            // mandatory DURATION token
            if i >= cmd.len() {
                return None;
            }
            i += 1;
        }
        // `nice [OPTIONS] [COMMAND]`
        "nice" => {
            while i < cmd.len() && cmd[i].starts_with('-') {
                if matches!(cmd[i].as_str(), "-n" | "--adjustment") {
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
        // `ionice [OPTIONS] [COMMAND]`
        "ionice" => {
            while i < cmd.len() && cmd[i].starts_with('-') {
                if matches!(
                    cmd[i].as_str(),
                    "-c" | "-n"
                        | "-p"
                        | "-P"
                        | "-u"
                        | "--class"
                        | "--classdata"
                        | "--pid"
                        | "--pgid"
                        | "--uid"
                ) {
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
        // `chrt [OPTIONS] PRIORITY COMMAND [ARGS]`
        "chrt" => {
            while i < cmd.len() && cmd[i].starts_with('-') {
                i += 1;
            }
            // mandatory PRIORITY token
            if i >= cmd.len() {
                return None;
            }
            i += 1;
        }
        // `stdbuf OPTIONS COMMAND [ARGS]`
        "stdbuf" => {
            while i < cmd.len() && cmd[i].starts_with('-') {
                if matches!(cmd[i].as_str(), "-i" | "-o" | "-e") {
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
        // `env [OPTIONS]/[NAME=VALUE] (interspersed) [COMMAND] [ARGS]`
        "env" => i = env_scan(cmd).1,
        _ => return None,
    }
    let inner = cmd.get(i..)?;
    if inner.is_empty() {
        return None;
    }
    Some(inner)
}

/// True if `words`' head is a command [`strip_wrapper_command`] would peel — i.e.
/// its basename is in the canonical wrapper set. Reports membership only (so bare
/// `env`, which `strip_wrapper_command` returns `None` for, still counts). Keep
/// the set in sync with `strip_wrapper_command`'s match arms.
pub(crate) fn is_wrapper_command(words: &[String]) -> bool {
    matches!(
        words.first().and_then(|w| w.rsplit(['/', '\\']).next()),
        Some("timeout" | "nice" | "ionice" | "chrt" | "stdbuf" | "env")
    )
}

/// Repeatedly strip wrapper commands (e.g. `timeout 30 nice -n 10 rm -rf /`).
/// Bounded to avoid pathological loops. Returns the original slice if no
/// wrapper is present.
pub(crate) fn unwrap_wrappers(words: &[String]) -> &[String] {
    let mut current = words;
    for _ in 0..8 {
        match strip_wrapper_command(current) {
            Some(inner) => current = inner,
            None => break,
        }
    }
    current
}

/// Whether the command runs under an `env -C`/`--chdir` (possibly path-qualified,
/// behind other wrappers). Only presence is reported — the caller treats such an
/// invocation's relative operands as unpinnable rather than resolving the dir.
pub(crate) fn wrapper_has_chdir(words: &[String]) -> bool {
    let mut current = words;
    for _ in 0..8 {
        if current.first().and_then(|w| w.rsplit(['/', '\\']).next()) == Some("env")
            && env_scan(current).0.is_some()
        {
            return true;
        }
        match strip_wrapper_command(current) {
            Some(inner) => current = inner,
            None => break,
        }
    }
    false
}

/// Simple shell-like splitter that:
/// - splits on whitespace (outside of quotes)
/// - handles single and double quotes, removing the quotes
/// - handles backslash escapes in a basic way
fn sh_split_simple(s: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut chars = s.chars().peekable();
    let mut in_single = bool::default();
    let mut in_double = bool::default();

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            '\\' => {
                // basic escape: take next char literally
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    result.push(current);
                    current = String::new();
                }
            }
            c => current.push(c),
        }
    }

    if !current.is_empty() {
        result.push(current);
    }

    result
}

/// Given a bash *script string* like:
///
/// ```bash
/// XAI_API_KEY='xai-some-key' cargo run --bin hyper
/// ```
///
/// returns the first "important" command as a `BashCommandHighlights` where:
/// - `prefix` = tokens before the highlighted command (env assignments, setup commands, operators)
/// - `highlighted_words` = the main command + args (like the old return)
/// - `suffix` = tokens after the highlighted command.
///
/// For the above example:
///   prefix: ["XAI_API_KEY=xai-some-key"]
///   highlighted_words: ["cargo", "run", "--bin", "hyper"]
///   suffix: []
pub fn primary_command_from_script(script: &str) -> Option<BashCommandHighlights> {
    let tree = try_parse_shell(script)?;
    let commands = try_parse_word_only_commands_sequence(&tree, script)?;

    let primary = commands.into_iter().find(|c| !is_setup_command(&c.words))?;

    let prefix_str = &script[..primary.span_start];
    let suffix_str = &script[primary.span_end..];

    Some(BashCommandHighlights {
        prefix: sh_split_simple(prefix_str),
        highlighted_words: primary.words,
        suffix: sh_split_simple(suffix_str),
    })
}

/// Parse all commands from a bash script using tree-sitter.
///
/// Returns `Some(Vec<PlainCommand>)` with every command in source order
/// (including "setup" commands like `cd`, `sleep`, etc.), or `None` if
/// the script contains constructs that tree-sitter-bash cannot cleanly
/// decompose into plain word-only commands.
pub fn all_commands_from_script(script: &str) -> Option<Vec<PlainCommand>> {
    let tree = try_parse_shell(script)?;
    try_parse_word_only_commands_sequence(&tree, script)
}

/// Parse a single `command` node into a list of "words", rejecting anything
/// non-trivial (substitutions, complex strings, etc.), and also returning
/// the byte span for the *highlighted* portion (command + args, not env).
fn parse_plain_command_from_node(cmd: Node, src: &str) -> Option<PlainCommand> {
    if cmd.kind() != "command" {
        return None;
    }
    let mut words = Vec::new();
    let mut cursor = cmd.walk();

    let mut span_start: Option<usize> = None;
    let mut span_end: Option<usize> = None;

    for child in cmd.named_children(&mut cursor) {
        match child.kind() {
            // We ignore simple env var assignments in front of the command.
            // Safety of their contents is already enforced by the outer whitelist.
            "variable_assignment" => {
                // no-op, just skip for words & span
            }
            "command_name" => {
                let word_node = child.named_child(0)?;
                if word_node.kind() != "word" {
                    return None;
                }
                let text = word_node.utf8_text(src.as_bytes()).ok()?.to_owned();

                if span_start.is_none() {
                    span_start = Some(word_node.start_byte());
                }
                span_end = Some(word_node.end_byte());

                words.push(text);
            }
            "word" | "number" => {
                let text = child.utf8_text(src.as_bytes()).ok()?.to_owned();

                if span_start.is_none() {
                    span_start = Some(child.start_byte());
                }
                span_end = Some(child.end_byte());

                words.push(text);
            }
            "string" => {
                // Allow only simple double-quoted strings with plain content.
                if child.child_count() == 3
                    && child.child(0)?.kind() == "\""
                    && child.child(1)?.kind() == "string_content"
                    && child.child(2)?.kind() == "\""
                {
                    let content_node = child.child(1)?;
                    let text = content_node.utf8_text(src.as_bytes()).ok()?.to_owned();

                    if span_start.is_none() {
                        // Highlight whole quoted string in the original script,
                        // so span uses the outer node.
                        span_start = Some(child.start_byte());
                    }
                    span_end = Some(child.end_byte());

                    words.push(text);
                } else {
                    // Reject complex / interpolated strings
                    return None;
                }
            }
            "raw_string" => {
                let raw_string = child.utf8_text(src.as_bytes()).ok()?;
                let stripped = raw_string
                    .strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''));
                if let Some(s) = stripped {
                    if span_start.is_none() {
                        span_start = Some(child.start_byte());
                    }
                    span_end = Some(child.end_byte());

                    words.push(s.to_owned());
                } else {
                    return None;
                }
            }
            "concatenation" => {
                // Handle concatenation nodes (e.g., {} in find -exec)
                let text = child.utf8_text(src.as_bytes()).ok()?.to_owned();

                if span_start.is_none() {
                    span_start = Some(child.start_byte());
                }
                span_end = Some(child.end_byte());

                words.push(text);
            }
            _ => return None,
        }
    }

    let span_start = span_start.unwrap_or_else(|| cmd.start_byte());
    let span_end = span_end.unwrap_or_else(|| cmd.end_byte());

    Some(PlainCommand {
        words,
        span_start,
        span_end,
    })
}

// ── Display soft-breaks (permission UI / formatting) ─────────────────

/// Node kinds whose descendants are payload text, not shell control flow.
/// Operators that appear only as *characters* inside these are not real
/// soft-break points (e.g. `&&` in a heredoc body or double-quoted string).
const PAYLOAD_NODE_KINDS: &[&str] = &[
    // Heredoc body / content (not the redirect operator itself on the
    // command line — `cat <<EOF && true` still has a real list `&&`).
    "heredoc_body",
    "simple_heredoc_body",
    "heredoc_content",
    "heredoc_end",
    // Quoted / expansion payload
    "string",
    "raw_string",
    "string_content",
    "ansi_c_string",
    "translated_string",
    // Comments
    "comment",
];

/// True when `kind` is a shell list/pipeline operator we may soft-break after.
fn is_soft_break_operator_kind(kind: &str) -> bool {
    matches!(kind, "&&" | "||" | "|" | ";")
}

/// True when we must not descend into this node looking for operators.
fn is_payload_node_kind(kind: &str) -> bool {
    PAYLOAD_NODE_KINDS.contains(&kind)
}

/// Byte offsets into `script` **after** real shell list/pipeline operators
/// where a display soft-wrap is safe.
///
/// Uses tree-sitter-bash so `&&` / `||` / `|` / `;` that appear only inside
/// strings, heredoc bodies, or comments are **not** returned. The command-line
/// operator in `cat <<EOF && echo after` **is** returned (it is a real `list`
/// operator); the body's `foo && bar` is not.
///
/// Returns an empty vec when the script cannot be parsed at all (caller should
/// fall back to width-only word-wrap, not naive substring splits).
///
/// Offsets are sorted ascending and de-duplicated. Each offset is
/// `operator_node.end_byte()` — i.e. the split keeps the operator on the
/// preceding display row.
pub fn soft_break_offsets_after_operators(script: &str) -> Vec<usize> {
    let Some(tree) = try_parse_shell(script) else {
        return Vec::new();
    };

    let root = tree.root_node();
    // On a broken parse, tree-sitter can still expose `|` / `&&` / `;` nodes
    // that are *not* real shell control flow (e.g. fragments of unclosed
    // strings or half-parsed heredocs). Prefer no soft-breaks over wrong ones.
    if root.has_error() {
        return Vec::new();
    }

    let mut breaks: Vec<usize> = Vec::new();
    let mut stack: Vec<Node> = vec![root];

    while let Some(node) = stack.pop() {
        let kind = node.kind();

        // Do not walk into string / heredoc / comment payload — any operator
        // characters there are not shell syntax nodes we care about, and
        // skipping the whole subtree is cheaper and safer.
        if is_payload_node_kind(kind) {
            continue;
        }

        if is_soft_break_operator_kind(kind) {
            let end = node.end_byte();
            if end > 0 && end <= script.len() && script.is_char_boundary(end) {
                breaks.push(end);
            }
        }

        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }

    breaks.sort_unstable();
    breaks.dedup();
    breaks
}

/// Byte ranges of heredoc *payload* (body / content), not the `<<WORD` opener
/// on the command line.
///
/// Used by the permission overlay so physical lines that are pure heredoc
/// body text are **not** soft-wrapped at spaces (they are free-form payload,
/// not shell syntax). Returns an empty vec when the script cannot be parsed
/// or the tree has errors (same policy as [`soft_break_offsets_after_operators`]:
/// error recovery can invent bogus heredoc spans).
pub fn heredoc_payload_byte_ranges(script: &str) -> Vec<(usize, usize)> {
    let Some(tree) = try_parse_shell(script) else {
        return Vec::new();
    };

    let root = tree.root_node();
    // Match soft-break policy: on a broken parse, tree-sitter error recovery can
    // invent or mis-bound `heredoc_body` nodes. Prefer no payload ranges (normal
    // soft-wrap / no false no-wrap) over wrong spans that overflow or skip wraps.
    if root.has_error() {
        return Vec::new();
    }

    const HEREDOC_PAYLOAD: &[&str] = &["heredoc_body", "simple_heredoc_body", "heredoc_content"];

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut stack: Vec<Node> = vec![root];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if HEREDOC_PAYLOAD.contains(&kind) {
            let start = node.start_byte();
            let end = node.end_byte();
            if start < end && end <= script.len() {
                ranges.push((start, end));
            }
            // Do not walk into children — the outer body/content range is enough.
            continue;
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }

    ranges.sort_unstable();
    ranges.dedup();
    ranges
}

/// True when the half-open byte range `[start, end)` lies entirely inside one
/// of the given (sorted) payload ranges.
pub fn range_fully_inside(start: usize, end: usize, ranges: &[(usize, usize)]) -> bool {
    if end < start {
        return false;
    }
    ranges.iter().any(|&(rs, re)| start >= rs && end <= re)
}

/// Split `line` (a physical line whose first byte is at `line_start` in the
/// full script that produced `breaks`) into contiguous slices at any soft
/// breaks that fall strictly inside the line.
///
/// When there are no applicable breaks, returns a single-element vec with
/// `line` unchanged.
pub fn split_physical_line_at_soft_breaks<'a>(
    line: &'a str,
    line_start: usize,
    breaks: &[usize],
) -> Vec<&'a str> {
    let line_end = line_start + line.len();
    let mut rel: Vec<usize> = breaks
        .iter()
        .copied()
        .filter(|&b| b > line_start && b < line_end)
        .map(|b| b - line_start)
        .filter(|&b| line.is_char_boundary(b))
        .collect();
    rel.dedup();
    if rel.is_empty() {
        return vec![line];
    }

    let mut chunks = Vec::with_capacity(rel.len() + 1);
    let mut start = 0usize;
    for b in rel {
        if b > start {
            chunks.push(&line[start..b]);
            start = b;
        }
    }
    if start < line.len() {
        chunks.push(&line[start..]);
    }
    if chunks.is_empty() {
        chunks.push(line);
    }
    chunks
}

/// Slice of `script` at each soft-break, for tests / debugging. Each slice
/// ends at an operator (inclusive); the final slice is the remainder.
pub fn soft_break_chunks(script: &str) -> Vec<&str> {
    let breaks = soft_break_offsets_after_operators(script);
    if breaks.is_empty() {
        return vec![script];
    }
    let mut out = Vec::with_capacity(breaks.len() + 1);
    let mut start = 0usize;
    for b in breaks {
        if b > start && b <= script.len() {
            out.push(&script[start..b]);
            start = b;
        }
    }
    if start < script.len() {
        out.push(&script[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        BashCommandHighlights, heredoc_payload_byte_ranges, primary_command_from_script,
        range_fully_inside, soft_break_chunks, soft_break_offsets_after_operators,
        split_physical_line_at_soft_breaks, try_parse_shell,
    };

    #[test]
    fn test_parse_plain_commands_from_script() {
        let command = "python3 something.py";
        assert_eq!(
            primary_command_from_script(command),
            Some(BashCommandHighlights {
                prefix: vec![],
                highlighted_words: vec!["python3".to_owned(), "something.py".to_owned()],
                suffix: vec![],
            })
        );

        let environment_key_command = "XAI_API_KEY='xai-some-key' cargo run --bin hyper";
        assert_eq!(
            primary_command_from_script(environment_key_command),
            Some(BashCommandHighlights {
                prefix: vec!["XAI_API_KEY=xai-some-key".to_owned()],
                highlighted_words: vec![
                    "cargo".to_owned(),
                    "run".to_owned(),
                    "--bin".to_owned(),
                    "hyper".to_owned()
                ],
                suffix: vec![],
            })
        );

        let cd_directory_command = "cd something/interesting/wow && python3 -c \"some text\\\" here \" | blah function important";
        assert_eq!(
            primary_command_from_script(cd_directory_command),
            Some(BashCommandHighlights {
                prefix: vec![
                    "cd".to_owned(),
                    "something/interesting/wow".to_owned(),
                    "&&".to_owned()
                ],
                highlighted_words: vec![
                    "python3".to_owned(),
                    "-c".to_owned(),
                    "some text\\\" here ".to_owned()
                ],
                suffix: vec![
                    "|".to_owned(),
                    "blah".to_owned(),
                    "function".to_owned(),
                    "important".to_owned()
                ],
            })
        );

        let redirection_command = "cargo build --bin hyper 2>&1";
        assert_eq!(
            primary_command_from_script(redirection_command),
            Some(BashCommandHighlights {
                prefix: vec![],
                highlighted_words: vec![
                    "cargo".to_owned(),
                    "build".to_owned(),
                    "--bin".to_owned(),
                    "hyper".to_owned(),
                ],
                suffix: vec!["2>&1".to_owned(),],
            })
        );

        let long_command = "grep -r \"ToolKind\" ~/.cargo/registry/src/ 2>/dev/null | grep \"enum\" | head -5 || find ~/.cargo -name \"*.rs\" -exec grep -l \"enum ToolKind\" {} \\; 2>/dev/null | head -5";
        assert_eq!(
            primary_command_from_script(long_command),
            Some(BashCommandHighlights {
                prefix: vec![],
                highlighted_words: vec![
                    "grep".to_owned(),
                    "-r".to_owned(),
                    "ToolKind".to_owned(),
                    "~/.cargo/registry/src/".to_owned(),
                ],
                suffix: vec![
                    "2>/dev/null".to_owned(),
                    "|".to_owned(),
                    "grep".to_owned(),
                    "enum".to_owned(),
                    "|".to_owned(),
                    "head".to_owned(),
                    "-5".to_owned(),
                    "||".to_owned(),
                    "find".to_owned(),
                    "~/.cargo".to_owned(),
                    "-name".to_owned(),
                    "*.rs".to_owned(),
                    "-exec".to_owned(),
                    "grep".to_owned(),
                    "-l".to_owned(),
                    "enum ToolKind".to_owned(),
                    "{}".to_owned(),
                    ";".to_owned(),
                    "2>/dev/null".to_owned(),
                    "|".to_owned(),
                    "head".to_owned(),
                    "-5".to_owned(),
                ],
            })
        );

        let another_long_command = "cargo test --package xai-grok-shell --lib -- permission::bash_command_splitting::tests::test_parse_plain_commands_from_script --exact --nocapture 2>&1";
        assert_eq!(
            primary_command_from_script(another_long_command),
            Some(BashCommandHighlights {
                prefix: vec![],
                highlighted_words: vec![
                    "cargo".to_owned(),
                    "test".to_owned(),
                    "--package".to_owned(),
                    "xai-grok-shell".to_owned(),
                    "--lib".to_owned(),
                    "--".to_owned(),
                    "permission::bash_command_splitting::tests::test_parse_plain_commands_from_script".to_owned(),
                    "--exact".to_owned(),
                    "--nocapture".to_owned(),
                ],
                suffix: vec!["2>&1".to_owned(),],
            })
        );

        let another_long_command_python = r#"python -c "
        import os
        os.environ.get("something")
        ""#;
        assert_eq!(
            primary_command_from_script(another_long_command_python),
            Some(BashCommandHighlights {
                prefix: vec![],
                highlighted_words: vec![
                    "python".to_owned(),
                    "-c".to_owned(),
                    "\"\n        import os\n        os.environ.get(\"something\")\n        \""
                        .to_owned()
                ],
                suffix: vec![],
            })
        );
    }

    #[test]
    fn test_sleep_and_timeout_commands_skipped() {
        // sleep 5 && foo should extract "foo" as the primary command
        let sleep_command = "sleep 5 && foo --bar";
        assert_eq!(
            primary_command_from_script(sleep_command),
            Some(BashCommandHighlights {
                prefix: vec!["sleep".to_owned(), "5".to_owned(), "&&".to_owned()],
                highlighted_words: vec!["foo".to_owned(), "--bar".to_owned()],
                suffix: vec![],
            })
        );

        // timeout 60 && foo should extract "foo" as the primary command
        let timeout_command = "timeout 60 && foo --bar";
        assert_eq!(
            primary_command_from_script(timeout_command),
            Some(BashCommandHighlights {
                prefix: vec!["timeout".to_owned(), "60".to_owned(), "&&".to_owned()],
                highlighted_words: vec!["foo".to_owned(), "--bar".to_owned()],
                suffix: vec![],
            })
        );

        // sleep 5 && timeout 60 foo should extract "foo" as the primary command
        // Note: "timeout 60 foo" is parsed as one command where timeout takes 60 and foo as args
        // So the primary should skip sleep and get to the timeout command, but timeout is also skipped
        let sleep_timeout_command = "sleep 5 && timeout 60 && foo --bar";
        assert_eq!(
            primary_command_from_script(sleep_timeout_command),
            Some(BashCommandHighlights {
                prefix: vec![
                    "sleep".to_owned(),
                    "5".to_owned(),
                    "&&".to_owned(),
                    "timeout".to_owned(),
                    "60".to_owned(),
                    "&&".to_owned()
                ],
                highlighted_words: vec!["foo".to_owned(), "--bar".to_owned()],
                suffix: vec![],
            })
        );

        // Combined with cd: cd /path && sleep 5 && git status
        let cd_sleep_command = "cd /some/path && sleep 5 && git status";
        assert_eq!(
            primary_command_from_script(cd_sleep_command),
            Some(BashCommandHighlights {
                prefix: vec![
                    "cd".to_owned(),
                    "/some/path".to_owned(),
                    "&&".to_owned(),
                    "sleep".to_owned(),
                    "5".to_owned(),
                    "&&".to_owned()
                ],
                highlighted_words: vec!["git".to_owned(), "status".to_owned()],
                suffix: vec![],
            })
        );

        // timeout with wrapped command: timeout 60 cargo test
        // Here timeout is the first command with args [60, cargo, test]
        // Since we skip timeout, we should look for the next command, but there isn't one
        // So this would return None or timeout as primary - let's see the actual behavior
        let timeout_wrapped = "timeout 60 cargo test";
        let result = primary_command_from_script(timeout_wrapped);
        // timeout is the only command here, so if we skip it, there's nothing else
        // The parsing treats "timeout 60 cargo test" as a single command
        // Since timeout is a setup command, it gets skipped, and there's no next command
        assert_eq!(result, None);

        // But if we chain it properly with &&, we can extract the real command
        let timeout_chained = "timeout 60 && cargo test";
        assert_eq!(
            primary_command_from_script(timeout_chained),
            Some(BashCommandHighlights {
                prefix: vec!["timeout".to_owned(), "60".to_owned(), "&&".to_owned()],
                highlighted_words: vec!["cargo".to_owned(), "test".to_owned()],
                suffix: vec![],
            })
        );

        // Test sleep with no following command just returns None (no primary command)
        let sleep_only = "sleep 5";
        assert_eq!(primary_command_from_script(sleep_only), None);
    }

    // ── soft_break_offsets_after_operators ────────────────────────────

    /// Helper: operators present at soft-break points (suffix of each prefix).
    fn break_operator_suffixes(script: &str) -> Vec<String> {
        soft_break_offsets_after_operators(script)
            .into_iter()
            .map(|b| {
                let prefix = &script[..b];
                // Take the trailing operator token (&&, ||, |, ;).
                if prefix.ends_with("&&") {
                    "&&".to_owned()
                } else if prefix.ends_with("||") {
                    "||".to_owned()
                } else if prefix.ends_with('|') {
                    "|".to_owned()
                } else if prefix.ends_with(';') {
                    ";".to_owned()
                } else {
                    prefix
                        .chars()
                        .rev()
                        .take(2)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect()
                }
            })
            .collect()
    }

    #[test]
    fn soft_break_simple_and_and_or_pipe_semi() {
        let script = "git status && cargo test || true; echo done | cat";
        let ops = break_operator_suffixes(script);
        assert_eq!(ops, vec!["&&", "||", ";", "|"]);
        let chunks = soft_break_chunks(script);
        assert!(chunks[0].ends_with("&&"));
        assert!(chunks.last().unwrap().contains("cat"));
    }

    #[test]
    fn soft_break_ignores_and_inside_double_quotes() {
        let script = r#"echo "a && b" && echo real"#;
        let ops = break_operator_suffixes(script);
        // Only the real list operator after the closing quote.
        assert_eq!(ops, vec!["&&"]);
        let chunks = soft_break_chunks(script);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].contains(r#""a && b""#));
        assert!(chunks[0].ends_with("&&"));
        assert!(chunks[1].contains("echo real"));
    }

    #[test]
    fn soft_break_ignores_and_inside_single_quotes() {
        let script = "echo 'x || y' || echo real";
        let ops = break_operator_suffixes(script);
        assert_eq!(ops, vec!["||"]);
    }

    #[test]
    fn soft_break_ignores_and_inside_heredoc_body() {
        let script = "cat <<EOF && echo after\nfoo && bar\nbaz || qux\nEOF";
        let ops = break_operator_suffixes(script);
        // Only the command-line list operator — not body `&&` / `||`.
        assert_eq!(
            ops,
            vec!["&&"],
            "body operators must not create soft-breaks; got {ops:?} for {script:?}"
        );
        // The single break must sit on the first physical line (after `&&`).
        let breaks = soft_break_offsets_after_operators(script);
        assert_eq!(breaks.len(), 1);
        let first_nl = script.find('\n').unwrap();
        assert!(
            breaks[0] <= first_nl,
            "break at {} should be on the opener line (nl at {first_nl})",
            breaks[0]
        );
        assert!(script[..breaks[0]].ends_with("&&"));
    }

    #[test]
    fn soft_break_ignores_and_in_heredoc_with_quoted_delimiter() {
        // Quoted delimiter still produces a heredoc body node.
        let script = "cat <<'END' && true\nkeep && this\nEND";
        let ops = break_operator_suffixes(script);
        assert_eq!(ops, vec!["&&"]);
    }

    #[test]
    fn soft_break_ignores_and_in_dash_heredoc_body() {
        // `<<-` (tab-stripping heredoc) must behave like `<<`: only the
        // command-line list operator is a break, not body operators.
        let script = "cat <<-EOF && echo after\n\tfoo && bar\n\tbaz | qux\n\tEOF";
        let ops = break_operator_suffixes(script);
        assert_eq!(
            ops,
            vec!["&&"],
            "dash-heredoc body operators must not break; got {ops:?}"
        );
    }

    #[test]
    fn soft_break_ignores_operators_in_comments() {
        let script = "echo hi # not && a break\necho real && echo also";
        let ops = break_operator_suffixes(script);
        assert_eq!(ops, vec!["&&"]);
        let breaks = soft_break_offsets_after_operators(script);
        // The break is on the second line.
        assert!(script[..breaks[0]].contains("echo real"));
    }

    #[test]
    fn soft_break_pipeline_only() {
        let script = "ps aux | grep foo | head -n 5";
        assert_eq!(break_operator_suffixes(script), vec!["|", "|"]);
    }

    #[test]
    fn soft_break_no_operators_returns_empty() {
        assert!(soft_break_offsets_after_operators("echo hello world").is_empty());
        assert_eq!(
            soft_break_chunks("echo hello world"),
            vec!["echo hello world"]
        );
    }

    #[test]
    fn soft_break_empty_script() {
        assert!(soft_break_offsets_after_operators("").is_empty());
    }

    #[test]
    fn soft_break_tight_operators_without_spaces() {
        // tree-sitter still emits `&&` nodes without surrounding spaces.
        let script = "true&&false||true";
        assert_eq!(break_operator_suffixes(script), vec!["&&", "||"]);
    }

    #[test]
    fn soft_break_does_not_split_on_ampersand_background() {
        // trailing `&` for background is not `&&` — must not produce a break.
        let script = "sleep 1 &";
        assert!(
            soft_break_offsets_after_operators(script).is_empty(),
            "background & is not a soft-break operator"
        );
    }

    #[test]
    fn split_physical_line_filters_breaks_to_line_range() {
        let script = "a && b\nc && d";
        let breaks = soft_break_offsets_after_operators(script);
        assert_eq!(breaks.len(), 2);

        let line0 = "a && b";
        let chunks0 = split_physical_line_at_soft_breaks(line0, 0, &breaks);
        assert_eq!(chunks0.len(), 2);
        assert!(chunks0[0].ends_with("&&"));
        assert_eq!(chunks0[1].trim(), "b");

        let line1_start = script.find('\n').unwrap() + 1;
        let line1 = "c && d";
        let chunks1 = split_physical_line_at_soft_breaks(line1, line1_start, &breaks);
        assert_eq!(chunks1.len(), 2);
        assert!(chunks1[0].ends_with("&&"));
        assert_eq!(chunks1[1].trim(), "d");
    }

    #[test]
    fn soft_break_nested_command_sub_still_sees_outer_list() {
        // Operators inside $() may or may not be soft-breaks depending on
        // whether we treat command_substitution as payload. We intentionally
        // still allow breaks inside $() (they're real shell ops for that
        // subshell) — only string/heredoc/comment are payload. Assert outer
        // list op is present either way.
        let script = "echo $(true && false) && echo outer";
        let ops = break_operator_suffixes(script);
        assert!(
            ops.iter().any(|o| o == "&&"),
            "expected at least the outer &&, got {ops:?}"
        );
        // Outer break: last one should be the list joining to `echo outer`.
        assert_eq!(ops.last().map(String::as_str), Some("&&"));
    }

    #[test]
    fn soft_break_multiline_backslash_continuation_with_and() {
        // Physical layout with `\` continuations; real `&&` between commands.
        let script = "cargo test \\\n  --all && \\\n  cargo clippy";
        let ops = break_operator_suffixes(script);
        assert_eq!(ops, vec!["&&"]);
    }

    #[test]
    fn soft_break_empty_on_parse_error() {
        // Broken scripts must not emit soft-breaks from half-parsed ops.
        // Unclosed quote / paren typically marks the tree as has_error().
        let broken = r#"echo "unclosed && true | false"#;
        let tree = try_parse_shell(broken).expect("tree-sitter still returns a tree");
        assert!(
            tree.root_node().has_error(),
            "expected error node for unclosed quote"
        );
        assert!(
            soft_break_offsets_after_operators(broken).is_empty(),
            "must not soft-break on a broken parse"
        );
    }

    #[test]
    fn heredoc_payload_empty_on_parse_error() {
        // Same policy as soft_break_empty_on_parse_error: error recovery must
        // not invent bogus heredoc payload spans.
        let broken = r#"echo "unclosed && true | false"#;
        let tree = try_parse_shell(broken).expect("tree-sitter still returns a tree");
        assert!(
            tree.root_node().has_error(),
            "expected error node for unclosed quote"
        );
        assert!(
            heredoc_payload_byte_ranges(broken).is_empty(),
            "must not emit heredoc payload ranges on a broken parse"
        );
        // Broken heredoc-looking input: unclosed quote + << can confuse recovery.
        let broken_heredocish = "cat <<EOF && echo \"unclosed\nbody\nEOF";
        if let Some(tree) = try_parse_shell(broken_heredocish)
            && tree.root_node().has_error()
        {
            assert!(
                heredoc_payload_byte_ranges(broken_heredocish).is_empty(),
                "broken heredoc-ish script must not invent payload ranges"
            );
        }
    }

    #[test]
    fn heredoc_payload_ranges_cover_body_not_opener() {
        let script = "cat <<EOF && echo after\nbody line with spaces here\nEOF";
        let ranges = heredoc_payload_byte_ranges(script);
        assert!(!ranges.is_empty(), "expected heredoc payload range");
        let body = "body line with spaces here";
        let body_start = script.find(body).unwrap();
        assert!(
            range_fully_inside(body_start, body_start + body.len(), &ranges),
            "body must be inside payload ranges: {ranges:?}"
        );
        // Opener command line is not fully inside payload.
        assert!(!range_fully_inside(0, script.find('\n').unwrap(), &ranges));
    }

    #[test]
    fn soft_break_gh_jq_pipeline_inside_single_quotes_is_not_a_break() {
        // Regression: `jq '.[] | ...'` must not soft-break at the `|` — it is
        // inside a single-quoted raw_string, not a shell pipeline.
        let script = r#"gh api user --jq '.login' && echo "---" && gh search prs --author=@me --sort=updated --limit=15 --json number,title,url,state,updatedAt,repository,isDraft --jq '.[] | "\(.state)\t#\(.number)\t\(.updatedAt)\t\(.repository.nameWithOwner)\t\(.title)\t\(.url)"'"#;
        let ops = break_operator_suffixes(script);
        assert_eq!(
            ops,
            vec!["&&", "&&"],
            "only the two real list operators; got {ops:?}"
        );
        assert!(
            !ops.iter().any(|o| *o == "|"),
            "jq `|` inside quotes must not be a soft-break"
        );
    }
}
