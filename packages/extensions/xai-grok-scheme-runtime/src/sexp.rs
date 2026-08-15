//! Minimal strict s-expression codec for the scheme image wire protocol.
//!
//! The grammar is deliberately tiny because both endpoints are written by us
//! (the Rust host here, the embedded Gambit kernel in `kernel/runtime.ss`):
//! proper lists, double-quoted strings, symbols, exact integers, `#t`/`#f`.
//! No dotted pairs, no characters, no vectors, no quote sugar, no comments.
//!
//! String escapes — canonical writer output, strict reader input:
//! `\\`, `\"`, `\n`, `\r`, `\t`, and `\xh+;` (hex, case-insensitive on read,
//! lowercase on write; used for all other C0 control characters). Every other
//! character, including non-ASCII, passes through as raw UTF-8. The kernel's
//! serializer emits exactly the same profile, so there is no codec-dialect
//! corner between the two sides.

use std::fmt;

/// Hard recursion limit for the reader (host frames are shallow).
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sexp {
    List(Vec<Sexp>),
    Str(String),
    Sym(String),
    Int(i64),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SexpError {
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("unexpected character `{0}` at byte {1}")]
    UnexpectedChar(char, usize),
    #[error("invalid string escape at byte {0}")]
    BadEscape(usize),
    #[error("nesting deeper than {MAX_DEPTH}")]
    TooDeep,
    #[error("trailing garbage after datum at byte {0}")]
    TrailingGarbage(usize),
    #[error("invalid integer literal")]
    BadInt,
}

impl Sexp {
    pub fn sym(s: &str) -> Self {
        Sexp::Sym(s.to_string())
    }

    pub fn str(s: impl Into<String>) -> Self {
        Sexp::Str(s.into())
    }

    pub fn list(items: Vec<Sexp>) -> Self {
        Sexp::List(items)
    }

    /// `(key "value")` pair used in wire ctx alists.
    pub fn kv(key: &str, value: Sexp) -> Self {
        Sexp::List(vec![Sexp::sym(key), value])
    }

    /// First element's symbol name when `self` is a non-empty list headed by a symbol.
    pub fn head_sym(&self) -> Option<&str> {
        match self {
            Sexp::List(items) => match items.first() {
                Some(Sexp::Sym(s)) => Some(s.as_str()),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Sexp::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Sexp::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// Element `i` of a list (including the head at index 0).
    pub fn nth(&self, i: usize) -> Option<&Sexp> {
        match self {
            Sexp::List(items) => items.get(i),
            _ => None,
        }
    }

    /// Argument `i` after the head symbol (i.e. element `i + 1`).
    pub fn arg(&self, i: usize) -> Option<&Sexp> {
        self.nth(i + 1)
    }

    /// Render in canonical wire form.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.render_into(&mut out);
        out
    }

    fn render_into(&self, out: &mut String) {
        match self {
            Sexp::List(items) => {
                out.push('(');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    item.render_into(out);
                }
                out.push(')');
            }
            Sexp::Str(s) => {
                out.push('"');
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        c if (c as u32) < 0x20 => {
                            out.push_str(&format!("\\x{:x};", c as u32));
                        }
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            Sexp::Sym(s) => out.push_str(s),
            Sexp::Int(n) => out.push_str(&n.to_string()),
            Sexp::Bool(true) => out.push_str("#t"),
            Sexp::Bool(false) => out.push_str("#f"),
        }
    }

    /// Parse exactly one datum; trailing whitespace is allowed, anything else errors.
    pub fn parse(input: &str) -> Result<Sexp, SexpError> {
        let mut p = Parser {
            chars: input.char_indices().peekable(),
            input,
        };
        p.skip_ws();
        let v = p.datum(0)?;
        p.skip_ws();
        if let Some(&(i, _)) = p.chars.peek() {
            return Err(SexpError::TrailingGarbage(i));
        }
        Ok(v)
    }
}

impl fmt::Display for Sexp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

struct Parser<'a> {
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    input: &'a str,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while let Some(&(_, c)) = self.chars.peek() {
            if c.is_whitespace() {
                self.chars.next();
            } else {
                break;
            }
        }
    }

    fn datum(&mut self, depth: usize) -> Result<Sexp, SexpError> {
        if depth > MAX_DEPTH {
            return Err(SexpError::TooDeep);
        }
        let &(i, c) = self.chars.peek().ok_or(SexpError::UnexpectedEof)?;
        match c {
            '(' => {
                self.chars.next();
                let mut items = Vec::new();
                loop {
                    self.skip_ws();
                    match self.chars.peek() {
                        None => return Err(SexpError::UnexpectedEof),
                        Some(&(_, ')')) => {
                            self.chars.next();
                            return Ok(Sexp::List(items));
                        }
                        Some(_) => items.push(self.datum(depth + 1)?),
                    }
                }
            }
            ')' => Err(SexpError::UnexpectedChar(')', i)),
            '"' => self.string(),
            '#' => {
                self.chars.next();
                match self.chars.next() {
                    Some((_, 't')) => Ok(Sexp::Bool(true)),
                    Some((_, 'f')) => Ok(Sexp::Bool(false)),
                    Some((j, c)) => Err(SexpError::UnexpectedChar(c, j)),
                    None => Err(SexpError::UnexpectedEof),
                }
            }
            _ => self.atom(i),
        }
    }

    fn string(&mut self) -> Result<Sexp, SexpError> {
        self.chars.next(); // opening quote
        let mut out = String::new();
        loop {
            let (i, c) = self.chars.next().ok_or(SexpError::UnexpectedEof)?;
            match c {
                '"' => return Ok(Sexp::Str(out)),
                '\\' => {
                    let (j, e) = self.chars.next().ok_or(SexpError::UnexpectedEof)?;
                    match e {
                        '\\' => out.push('\\'),
                        '"' => out.push('"'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'x' | 'X' => {
                            let mut hex = String::new();
                            loop {
                                let (k, h) = self.chars.next().ok_or(SexpError::UnexpectedEof)?;
                                if h == ';' {
                                    break;
                                }
                                if !h.is_ascii_hexdigit() || hex.len() >= 6 {
                                    return Err(SexpError::BadEscape(k));
                                }
                                hex.push(h);
                            }
                            if hex.is_empty() {
                                return Err(SexpError::BadEscape(j));
                            }
                            let code =
                                u32::from_str_radix(&hex, 16).map_err(|_| SexpError::BadEscape(j))?;
                            let c = char::from_u32(code).ok_or(SexpError::BadEscape(j))?;
                            out.push(c);
                        }
                        _ => return Err(SexpError::BadEscape(j)),
                    }
                }
                _ => {
                    let _ = i;
                    out.push(c);
                }
            }
        }
    }

    fn atom(&mut self, start: usize) -> Result<Sexp, SexpError> {
        let mut end = start;
        while let Some(&(i, c)) = self.chars.peek() {
            if c.is_whitespace() || c == '(' || c == ')' || c == '"' {
                break;
            }
            end = i + c.len_utf8();
            self.chars.next();
        }
        let token = &self.input[start..end];
        if token.is_empty() {
            return Err(SexpError::UnexpectedEof);
        }
        let first = token.chars().next().unwrap();
        let looks_numeric = first.is_ascii_digit()
            || ((first == '-' || first == '+')
                && token.len() > 1
                && token.chars().nth(1).is_some_and(|c| c.is_ascii_digit()));
        if looks_numeric {
            return token.parse::<i64>().map(Sexp::Int).map_err(|_| SexpError::BadInt);
        }
        Ok(Sexp::Sym(token.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: &Sexp) {
        let rendered = v.render();
        let parsed = Sexp::parse(&rendered).unwrap_or_else(|e| {
            panic!("failed to parse rendered form {rendered:?}: {e}");
        });
        assert_eq!(&parsed, v, "roundtrip mismatch for {rendered:?}");
    }

    #[test]
    fn renders_and_parses_basic_forms() {
        roundtrip(&Sexp::List(vec![]));
        roundtrip(&Sexp::sym("hello-ok"));
        roundtrip(&Sexp::Int(42));
        roundtrip(&Sexp::Int(-7));
        roundtrip(&Sexp::Bool(true));
        roundtrip(&Sexp::Bool(false));
        roundtrip(&Sexp::list(vec![
            Sexp::sym("dispatch"),
            Sexp::sym("pre-tool-use"),
            Sexp::str("my-plugin"),
            Sexp::list(vec![
                Sexp::kv("tool-name", Sexp::str("shell")),
                Sexp::kv("tool-input", Sexp::str(r#"{"cmd":"ls"}"#)),
            ]),
        ]));
    }

    #[test]
    fn string_escapes_roundtrip() {
        for s in [
            "",
            "plain",
            "with \"quotes\" and \\backslash\\",
            "newline\nreturn\rtab\t",
            "control\u{01}\u{02}\u{1f}",
            "unicode: 中文 émoji 🦀",
            "mixed \" \\ \n \u{07} 你好",
        ] {
            roundtrip(&Sexp::str(s));
        }
    }

    #[test]
    fn parses_case_insensitive_hex_escapes() {
        assert_eq!(
            Sexp::parse(r#""\x41;\x1F;\X0a;""#).unwrap(),
            Sexp::str("A\u{1f}\n")
        );
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(Sexp::parse("(unclosed").is_err());
        assert!(Sexp::parse(")").is_err());
        assert!(Sexp::parse(r#""bad \q escape""#).is_err());
        assert!(Sexp::parse(r#""\x;""#).is_err());
        assert!(Sexp::parse(r#""\xzz;""#).is_err());
        assert!(Sexp::parse("(a) trailing").is_err());
        assert!(Sexp::parse("").is_err());
        // Deep nesting bomb.
        let bomb = "(".repeat(100) + &")".repeat(100);
        assert_eq!(Sexp::parse(&bomb), Err(SexpError::TooDeep));
    }

    #[test]
    fn helpers_work() {
        let v = Sexp::parse(r#"(deny "not allowed")"#).unwrap();
        assert_eq!(v.head_sym(), Some("deny"));
        assert_eq!(v.arg(0).and_then(Sexp::as_str), Some("not allowed"));
        assert_eq!(Sexp::parse("(ok 3)").unwrap().arg(0).and_then(Sexp::as_int), Some(3));
    }
}
