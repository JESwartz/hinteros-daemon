//! Valve's KeyValues text format ("VDF"), as used by `libraryfolders.vdf` and
//! `appmanifest_<id>.acf`.
//!
//! ```text
//! "AppState"            // a key followed by a block...
//! {
//!     "appid"   "620"   // ...or by a string value
//!     name      Portal  // tokens may be unquoted
//! }
//! ```
//!
//! What this parser accepts: quoted and unquoted tokens, `{ }` blocks, `//` comments, the
//! escapes `\\ \" \n \t` in quoted strings (others are kept as written), a UTF-8 BOM,
//! `[$CONDITION]` suffixes (ignored), and `#include`/`#base` lines (ignored). Keys keep their
//! order and may repeat; lookups are case-insensitive, as in Steam. An unbalanced or truncated
//! document is an error, so a half-written file is skipped rather than misread.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Obj(Obj),
}

/// A block: key/value pairs in file order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Obj(pub Vec<(String, Value)>);

impl Obj {
    /// The first value for `key` (case-insensitive).
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    }

    /// The first string value for `key`.
    pub fn str(&self, key: &str) -> Option<&str> {
        self.0.iter().find_map(|(k, v)| match v {
            Value::Str(s) if k.eq_ignore_ascii_case(key) => Some(s.as_str()),
            _ => None,
        })
    }

    /// The first block for `key`.
    pub fn obj(&self, key: &str) -> Option<&Obj> {
        self.0.iter().find_map(|(k, v)| match v {
            Value::Obj(o) if k.eq_ignore_ascii_case(key) => Some(o),
            _ => None,
        })
    }

    /// A string value parsed as a number; `None` if missing or not a number.
    pub fn num<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
        self.str(key)?.trim().parse().ok()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for Error {}

#[derive(Debug, PartialEq)]
enum Token {
    Str(String),
    Open,
    Close,
}

struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    line: usize,
}

impl Lexer<'_> {
    fn err(&self, message: impl Into<String>) -> Error {
        Error {
            line: self.line,
            message: message.into(),
        }
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.next();
        if c == Some('\n') {
            self.line += 1;
        }
        c
    }

    fn skip_line(&mut self) {
        while let Some(c) = self.bump() {
            if c == '\n' {
                break;
            }
        }
    }

    /// The next token, skipping whitespace, comments, directives and conditionals.
    fn next(&mut self) -> Result<Option<Token>, Error> {
        loop {
            let Some(&c) = self.chars.peek() else {
                return Ok(None);
            };
            match c {
                c if c.is_whitespace() || c == '\u{feff}' => {
                    self.bump();
                }
                '/' => {
                    self.bump();
                    if self.chars.peek() == Some(&'/') {
                        self.skip_line();
                    } else {
                        // A lone slash starts an unquoted token.
                        let rest = self.unquoted();
                        return Ok(Some(Token::Str(format!("/{rest}"))));
                    }
                }
                '#' => self.skip_line(),
                '[' => {
                    // Conditional like [$WIN32]: ignored.
                    loop {
                        match self.chars.peek() {
                            None | Some('\n') => return Err(self.err("unterminated [condition]")),
                            Some(']') => {
                                self.bump();
                                break;
                            }
                            Some(_) => {
                                self.bump();
                            }
                        }
                    }
                }
                '{' => {
                    self.bump();
                    return Ok(Some(Token::Open));
                }
                '}' => {
                    self.bump();
                    return Ok(Some(Token::Close));
                }
                '"' => {
                    self.bump();
                    return self.quoted().map(|s| Some(Token::Str(s)));
                }
                _ => return Ok(Some(Token::Str(self.unquoted()))),
            }
        }
    }

    fn quoted(&mut self) -> Result<String, Error> {
        let start = self.line;
        let mut s = String::new();
        loop {
            match self.bump() {
                None => {
                    return Err(Error {
                        line: start,
                        message: "unterminated string".into(),
                    });
                }
                Some('"') => return Ok(s),
                Some('\\') => match self.chars.peek() {
                    Some('\\') => {
                        self.bump();
                        s.push('\\');
                    }
                    Some('"') => {
                        self.bump();
                        s.push('"');
                    }
                    Some('n') => {
                        self.bump();
                        s.push('\n');
                    }
                    Some('t') => {
                        self.bump();
                        s.push('\t');
                    }
                    _ => s.push('\\'),
                },
                Some(c) => s.push(c),
            }
        }
    }

    fn unquoted(&mut self) -> String {
        let mut s = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_whitespace() || matches!(c, '{' | '}' | '"') {
                break;
            }
            s.push(c);
            self.bump();
        }
        s
    }
}

/// Parse a whole document: a sequence of key/value pairs (usually exactly one root block).
pub fn parse(text: &str) -> Result<Obj, Error> {
    let mut lx = Lexer {
        chars: text.chars().peekable(),
        line: 1,
    };
    let root = block(&mut lx, false)?;
    Ok(root)
}

/// Pairs until `}` (if `nested`) or the end of input.
fn block(lx: &mut Lexer<'_>, nested: bool) -> Result<Obj, Error> {
    let mut pairs = Vec::new();
    loop {
        let key = match lx.next()? {
            None if nested => return Err(lx.err("missing '}' at end of input")),
            None => return Ok(Obj(pairs)),
            Some(Token::Close) if nested => return Ok(Obj(pairs)),
            Some(Token::Close) => return Err(lx.err("unexpected '}'")),
            Some(Token::Open) => return Err(lx.err("'{' where a key was expected")),
            Some(Token::Str(key)) => key,
        };
        let value = match lx.next()? {
            Some(Token::Str(s)) => Value::Str(s),
            Some(Token::Open) => Value::Obj(block(lx, true)?),
            Some(Token::Close) | None => {
                return Err(lx.err(format!("key {key:?} has no value")));
            }
        };
        pairs.push((key, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_like() {
        let doc = parse(
            "\u{feff}\"AppState\"\n{\n\t\"appid\"\t\t\"620\"\n\t\"name\"\t\t\"Portal 2\"\n\
             \t\"InstalledDepots\"\n\t{\n\t\t\"621\"\n\t\t{\n\t\t\t\"size\"\t\t\"123\"\n\t\t}\n\t}\n}\n",
        )
        .unwrap();
        let app = doc.obj("appstate").unwrap();
        assert_eq!(app.str("AppID"), Some("620"));
        assert_eq!(app.num::<u32>("appid"), Some(620));
        assert_eq!(app.str("name"), Some("Portal 2"));
        let depot = app.obj("InstalledDepots").unwrap().obj("621").unwrap();
        assert_eq!(depot.num::<u64>("size"), Some(123));
        assert_eq!(app.str("InstalledDepots"), None, "a block is not a string");
        assert_eq!(app.num::<u32>("name"), None);
    }

    #[test]
    fn tolerant_lexing() {
        let doc = parse(
            r#"// comment at the top
            #base "other.vdf"
            root {
                unquoted value // trailing comment
                "path" "C:\\Program Files (x86)\\Steam"
                "quote" "say \"hi\"\tnow\n"
                "odd" "a\qb"
                "cond" "1" [$WIN32]
                "dup" "first"
                "dup" "second"
                "url" http://example.com/x
                "empty" ""
                "multi" "line one
line two"
            }"#,
        )
        .unwrap();
        let root = doc.obj("ROOT").unwrap();
        assert_eq!(root.str("unquoted"), Some("value"));
        assert_eq!(root.str("path"), Some(r"C:\Program Files (x86)\Steam"));
        assert_eq!(root.str("quote"), Some("say \"hi\"\tnow\n"));
        assert_eq!(root.str("odd"), Some(r"a\qb"));
        assert_eq!(root.str("cond"), Some("1"));
        assert_eq!(root.str("dup"), Some("first"));
        assert_eq!(root.iter().filter(|(k, _)| *k == "dup").count(), 2);
        assert_eq!(root.str("url"), Some("http://example.com/x"));
        assert_eq!(root.str("empty"), Some(""));
        assert_eq!(root.str("multi"), Some("line one\nline two"));
    }

    #[test]
    fn compact_and_empty() {
        let doc = parse(r#""a"{"b""c""d"{}}"#).unwrap();
        let a = doc.obj("a").unwrap();
        assert_eq!(a.str("b"), Some("c"));
        assert_eq!(a.obj("d"), Some(&Obj::default()));
        assert_eq!(parse("").unwrap(), Obj::default());
        assert_eq!(parse("  // nothing\n").unwrap(), Obj::default());
    }

    #[test]
    fn rejects_broken_documents() {
        for (bad, line) in [
            ("\"AppState\"\n{\n\t\"appid\"\t\"620\"\n", 4), // truncated mid-write
            ("\"a\" \"b", 1),
            ("\"a\"", 1),
            ("}", 1),
            ("{ \"a\" \"b\" }", 1),
            ("\"a\" { \"b\" }", 1),
            ("\"a\" {\n \"b\" \"c\" [$X\n}", 2),
        ] {
            let err = parse(bad).unwrap_err();
            assert_eq!(err.line, line, "{bad:?}: {err}");
        }
    }
}
