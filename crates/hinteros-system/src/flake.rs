//! The device flake (`flake.lock`, `flake.nix`) and the update policy shipped in the running
//! generation (`release.json`, `update/channels.json`, `update/allowed_signers`).
//!
//! The channel lives in `flake.nix` as the `ref` of the `hinteros` input; the locked revision
//! in `flake.lock`. Channel URLs come only from the running generation's `channels.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use hinteros_api::system::{Channel, is_rev};
use serde::Deserialize;
use serde_json::Value;

/// The `hinteros` input as locked in `flake.lock`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockedInput {
    /// `original.ref`: the branch followed.
    pub branch: Option<String>,
    /// `locked.rev`.
    pub rev: Option<String>,
}

/// Read the `hinteros` input from a `flake.lock` (missing file: nothing locked).
pub fn read_lock(path: &Path) -> Result<LockedInput> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(LockedInput::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    parse_lock(&text)
}

pub fn parse_lock(text: &str) -> Result<LockedInput> {
    let v: Value = serde_json::from_str(text).context("flake.lock is not JSON")?;
    let root = v["root"].as_str().unwrap_or("root");
    let node_name = match &v["nodes"][root]["inputs"]["hinteros"] {
        Value::String(s) => s.clone(),
        Value::Null => return Ok(LockedInput::default()),
        _ => bail!("flake.lock: the hinteros input follows another input"),
    };
    let node = &v["nodes"][node_name.as_str()];
    let rev = node["locked"]["rev"]
        .as_str()
        .filter(|r| is_rev(r))
        .map(str::to_owned);
    let branch = node["original"]["ref"]
        .as_str()
        .filter(|b| is_branch(b))
        .map(str::to_owned);
    Ok(LockedInput { branch, rev })
}

/// A git branch name we accept from configuration: no `..`, no leading `-`, a small charset.
pub fn is_branch(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with('-')
        && !s.contains("..")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
}

/// `update/channels.json` of the running generation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelTable {
    /// The system repo (https; `file://` for tests and local mirrors).
    pub repo: String,
    /// Channel → branch.
    pub branches: BTreeMap<Channel, String>,
}

impl ChannelTable {
    pub fn load(policy_dir: &Path) -> Result<Self> {
        let path = policy_dir.join("update/channels.json");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading the channel table {}", path.display()))?;
        let table: ChannelTable =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        table.validate()?;
        Ok(table)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.scheme().is_some(),
            "channel table: repo must be an https:// or file:// URL"
        );
        ensure!(
            !self.repo.contains(['?', '#', ' ', '"', '\\', '\n']),
            "channel table: repo URL must not contain a query, fragment, quotes or spaces"
        );
        for b in self.branches.values() {
            ensure!(is_branch(b), "channel table: bad branch {b:?}");
        }
        Ok(())
    }

    /// `https` or `file` (the only git transports jobs allow).
    pub fn scheme(&self) -> Option<&'static str> {
        if self.repo.starts_with("https://") && self.repo.len() > 8 {
            Some("https")
        } else if self.repo.starts_with("file:///") {
            Some("file")
        } else {
            None
        }
    }

    pub fn host(&self) -> String {
        self.repo
            .strip_prefix("https://")
            .and_then(|r| r.split(['/', ':']).next())
            .unwrap_or("localhost")
            .to_owned()
    }

    pub fn branch(&self, channel: Channel) -> Option<&str> {
        self.branches.get(&channel).map(String::as_str)
    }

    pub fn channel_of(&self, branch: &str) -> Option<Channel> {
        self.branches
            .iter()
            .find(|(_, b)| b.as_str() == branch)
            .map(|(c, _)| *c)
    }

    /// The flake input URL for a branch (and optionally a locked revision).
    pub fn input_url(&self, branch: &str, rev: Option<&str>) -> String {
        match rev {
            Some(rev) => format!("git+{}?ref={branch}&rev={rev}", self.repo),
            None => format!("git+{}?ref={branch}", self.repo),
        }
    }
}

/// The channel the box follows: the locked branch, named through the channel table (or, without
/// a table, by its own name).
pub fn current_channel(lock: &LockedInput, table: Option<&ChannelTable>) -> Option<Channel> {
    let branch = lock.branch.as_deref()?;
    match table {
        Some(t) => t.channel_of(branch),
        None => Channel::from_name(branch),
    }
}

/// Point `flake.nix`'s hinteros input at another branch: only the string of
/// `inputs.hinteros.url` changes, however the file is laid out (one line, `inputs = { ... }`,
/// comments). That string must be `from`; anything else is an error and the file stays.
pub fn retarget_flake_nix(text: &str, from: &str, to: &str) -> Result<String> {
    ensure!(
        !to.contains(['"', '\\', '$', '\n']),
        "the new hinteros input URL is not a plain string"
    );
    let mut found = Vec::new();
    NixAttrs::new(text).file(&mut found)?;
    let [url] = found.as_slice() else {
        bail!(
            "flake.nix defines inputs.hinteros.url {} times, not once",
            found.len()
        );
    };
    let Some(url) = url.clone() else {
        bail!("inputs.hinteros.url in flake.nix is not a plain string");
    };
    let current = &text[url.clone()];
    ensure!(
        current == from,
        "inputs.hinteros.url in flake.nix is {current}, not {from}"
    );
    Ok(format!("{}{to}{}", &text[..url.start], &text[url.end..]))
}

/// The attribute this module rewrites.
const INPUT_URL: [&str; 3] = ["inputs", "hinteros", "url"];

/// As much of Nix as finds [`INPUT_URL`] in a flake: the file is one attribute set, and only the
/// attribute sets on the way to the URL are entered. Every other value is skipped, with its
/// strings, comments and brackets.
struct NixAttrs<'a> {
    text: &'a [u8],
    at: usize,
}

impl<'a> NixAttrs<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text: text.as_bytes(),
            at: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn looking_at(&self, s: &str) -> bool {
        self.text[self.at..].starts_with(s.as_bytes())
    }

    /// Whitespace and comments.
    fn skip_trivia(&mut self) -> Result<()> {
        loop {
            match self.peek() {
                Some(b) if b.is_ascii_whitespace() => self.at += 1,
                Some(b'#') => {
                    while self.peek().is_some_and(|b| b != b'\n') {
                        self.at += 1;
                    }
                }
                Some(b'/') if self.looking_at("/*") => {
                    let rest = &self.text[self.at..];
                    let end = rest.windows(2).position(|w| w == b"*/");
                    self.at += end.context("flake.nix: a comment does not end")? + 2;
                }
                _ => return Ok(()),
            }
        }
    }

    fn expect(&mut self, b: u8) -> Result<()> {
        self.skip_trivia()?;
        ensure!(
            self.peek() == Some(b),
            "flake.nix: expected {:?} at byte {}",
            b as char,
            self.at
        );
        self.at += 1;
        Ok(())
    }

    /// A `"..."` string, from its opening quote. `Some`: where its text is, when that is plain
    /// (no escape, no `${`).
    fn string(&mut self) -> Result<Option<std::ops::Range<usize>>> {
        self.at += 1;
        let start = self.at;
        let mut plain = true;
        loop {
            match self.peek() {
                None => bail!("flake.nix: a string does not end"),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(plain.then_some(start..self.at - 1));
                }
                Some(b'\\') => {
                    plain = false;
                    self.at += 2;
                }
                Some(b'$') if self.looking_at("${") => {
                    plain = false;
                    self.at += 2;
                    self.value(b'}')?;
                }
                Some(_) => self.at += 1,
            }
        }
    }

    /// A `''...''` string, from its opening quotes.
    fn indented_string(&mut self) -> Result<()> {
        self.at += 2;
        loop {
            if self.at >= self.text.len() {
                bail!("flake.nix: a string does not end");
            } else if self.looking_at("'''") || self.looking_at("''$") {
                self.at += 3;
            } else if self.looking_at("''\\") {
                self.at += 4;
            } else if self.looking_at("''") {
                self.at += 2;
                return Ok(());
            } else if self.looking_at("${") {
                self.at += 2;
                self.value(b'}')?;
            } else {
                self.at += 1;
            }
        }
    }

    /// Skip code up to and including `end`, outside any bracket.
    fn value(&mut self, end: u8) -> Result<()> {
        let mut open: Vec<u8> = Vec::new();
        loop {
            self.skip_trivia()?;
            let Some(b) = self.peek() else {
                bail!("flake.nix ends inside a value");
            };
            match b {
                b'"' => {
                    self.string()?;
                    continue;
                }
                b'\'' if self.looking_at("''") => {
                    self.indented_string()?;
                    continue;
                }
                // An identifier may hold a `'`.
                _ if is_ident_start(b) => {
                    self.ident();
                    continue;
                }
                b'{' => open.push(b'}'),
                b'(' => open.push(b')'),
                b'[' => open.push(b']'),
                _ if open.last() == Some(&b) => {
                    open.pop();
                }
                _ if b == end && open.is_empty() => {
                    self.at += 1;
                    return Ok(());
                }
                b'}' | b')' | b']' => bail!("flake.nix: unmatched {:?}", b as char),
                _ => {}
            }
            self.at += 1;
        }
    }

    fn ident(&mut self) -> &'a str {
        let start = self.at;
        while self.peek().is_some_and(is_ident_byte) {
            self.at += 1;
        }
        std::str::from_utf8(&self.text[start..self.at]).unwrap_or_default()
    }

    /// `a.b.c`, empty when the next thing is not a plain attribute name.
    fn attr_path(&mut self) -> Result<Vec<&'a str>> {
        let mut path = Vec::new();
        loop {
            self.skip_trivia()?;
            if !self.peek().is_some_and(is_ident_start) {
                return Ok(path);
            }
            path.push(self.ident());
            self.skip_trivia()?;
            if self.peek() != Some(b'.') {
                return Ok(path);
            }
            self.at += 1;
        }
    }

    /// The bindings of the attribute set at `path`, up to its closing brace. `found` gets one
    /// entry per [`INPUT_URL`] binding: `None` when its value is not a plain string.
    fn bindings(
        &mut self,
        path: &[&str],
        found: &mut Vec<Option<std::ops::Range<usize>>>,
    ) -> Result<()> {
        loop {
            self.skip_trivia()?;
            match self.peek() {
                None => bail!("flake.nix ends inside an attribute set"),
                Some(b'}') => {
                    self.at += 1;
                    return Ok(());
                }
                _ => {}
            }
            let names = self.attr_path()?;
            if names == ["inherit"] {
                self.value(b';')?;
                continue;
            }
            if names.is_empty() {
                // A quoted or computed name: not one this module writes.
                self.value(b'=')?;
            } else {
                self.expect(b'=')?;
            }
            let here = [path, &names].concat();
            self.skip_trivia()?;
            if here == INPUT_URL {
                if self.peek() == Some(b'"') {
                    let url = self.string()?;
                    self.skip_trivia()?;
                    let alone = self.peek() == Some(b';');
                    found.push(url.filter(|_| alone));
                } else {
                    found.push(None);
                }
                self.value(b';')?;
            } else if !names.is_empty()
                && here.len() < INPUT_URL.len()
                && INPUT_URL.starts_with(&here)
                && self.peek() == Some(b'{')
            {
                self.at += 1;
                self.bindings(&here, found)?;
                self.expect(b';')?;
            } else {
                self.value(b';')?;
            }
        }
    }

    /// The whole file: one attribute set.
    fn file(&mut self, found: &mut Vec<Option<std::ops::Range<usize>>>) -> Result<()> {
        self.expect(b'{')?;
        self.bindings(&[], found)?;
        self.skip_trivia()?;
        ensure!(self.peek().is_none(), "flake.nix: text after the flake");
        Ok(())
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'\'' | b'-')
}

/// `release.json` of the running generation: `{"version": "...", "dev": bool}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Release {
    pub version: String,
    #[serde(default)]
    pub dev: bool,
}

impl Release {
    pub fn load(policy_dir: &Path) -> Self {
        let parsed = std::fs::read_to_string(policy_dir.join("release.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Release>(&t).ok())
            .filter(|r| {
                (1..=64).contains(&r.version.len())
                    && r.version.bytes().all(|b| b.is_ascii_graphic())
            });
        parsed.unwrap_or(Release {
            version: "unknown".into(),
            dev: false,
        })
    }
}

/// The allowed signers file of the running generation.
pub fn allowed_signers(policy_dir: &Path) -> PathBuf {
    policy_dir.join("update/allowed_signers")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"{
      "nodes": {
        "hinteros": {
          "locked": { "lastModified": 1, "narHash": "sha256-x", "ref": "refs/heads/stable",
                      "rev": "0f3c5a9e2b7d4c1a8e6f9b0d2c4e6a8b1d3f5a7c", "type": "git",
                      "url": "https://git.hinteros.org/hinteros.git" },
          "original": { "ref": "stable", "type": "git",
                        "url": "https://git.hinteros.org/hinteros.git" }
        },
        "root": { "inputs": { "hinteros": "hinteros" } }
      },
      "root": "root",
      "version": 7
    }"#;

    #[test]
    fn reads_the_lock() {
        let l = parse_lock(LOCK).unwrap();
        assert_eq!(l.branch.as_deref(), Some("stable"));
        assert_eq!(
            l.rev.as_deref(),
            Some("0f3c5a9e2b7d4c1a8e6f9b0d2c4e6a8b1d3f5a7c")
        );
        let none = parse_lock(r#"{"nodes":{"root":{}},"root":"root","version":7}"#).unwrap();
        assert_eq!(none, LockedInput::default());
        assert!(parse_lock("nope").is_err());
        let bad = LOCK.replace("0f3c5a9e2b7d4c1a8e6f9b0d2c4e6a8b1d3f5a7c", "HEAD");
        assert_eq!(parse_lock(&bad).unwrap().rev, None);
    }

    #[test]
    fn channel_table() {
        let t: ChannelTable = serde_json::from_str(
            r#"{"repo":"https://git.hinteros.org/hinteros.git","branches":{"stable":"stable","beta":"beta"}}"#,
        )
        .unwrap();
        t.validate().unwrap();
        assert_eq!(t.host(), "git.hinteros.org");
        assert_eq!(t.channel_of("beta"), Some(Channel::Beta));
        assert_eq!(
            t.input_url("stable", Some("ab")),
            "git+https://git.hinteros.org/hinteros.git?ref=stable&rev=ab"
        );
        for repo in [
            "http://x/r.git",
            "ext::sh -c x",
            "https://x/r.git?ref=main",
            "file://relative",
            "ssh://git@x/r",
        ] {
            let t = ChannelTable {
                repo: repo.into(),
                branches: BTreeMap::new(),
            };
            assert!(t.validate().is_err(), "{repo}");
        }
        for b in ["", "-x", "a..b", "a b", "a;b"] {
            assert!(!is_branch(b), "{b}");
        }
        assert!(is_branch("release/1.0"));
    }

    const STABLE: &str = "git+https://h/r.git?ref=stable";
    const BETA: &str = "git+https://h/r.git?ref=beta";

    #[test]
    fn retargets_flake_nix() {
        let text = format!(
            r#"# /etc/hinteros/flake.nix, generated by the installer.
{{
  inputs.hinteros.url = "{STABLE}";

  outputs = {{ hinteros, ... }}: hinteros.lib.deviceOutputs ./.;
}}
"#
        );
        let out = retarget_flake_nix(&text, STABLE, BETA).unwrap();
        assert_eq!(out, text.replace(STABLE, BETA));
        // Not what the file says: an error.
        let e = retarget_flake_nix(&text, BETA, STABLE).unwrap_err();
        assert!(e.to_string().contains("not git+https"), "{e}");
    }

    #[test]
    fn every_layout_of_the_url_changes_it_alone() {
        for text in [
            // The one-line form.
            r#"{ inputs.hinteros.url = "@"; outputs = { hinteros, ... }: hinteros.lib.deviceOutputs ./.; }"#,
            // Spacing.
            "{\n\tinputs . hinteros\n  . url=\"@\" ;\noutputs=x;}",
            // The older multi-line template.
            r#"{
  description = "A HinterOS box";
  inputs = {
    hinteros.url = "@";
    other = { url = "github:a/b"; flake = false; };
  };
  outputs = { self, hinteros }: {
    nixosConfigurations.hinteros = hinteros.lib.mkDevice { path = ./.; };
  };
}"#,
            // Nested all the way, and flake = true beside it.
            r#"{ inputs = { hinteros = { url = "@"; flake = true; }; }; outputs = x; }"#,
            r#"{ inputs.hinteros = { url = "@"; }; outputs = x; }"#,
            // Comments and strings that look like it.
            r##"# inputs.hinteros.url = "git+https://old";
{
  /* inputs.hinteros.url = "git+https://older"; */
  description = "inputs.hinteros.url = \"x\"; ${"}"}";
  note = ''
    inputs.hinteros.url = "y"; ''${z} '''
  '';
  inputs.hinteros.url = /* the channel */ "@"; # was stable
  outputs = { ... }: { a = "@"; b = [ 1 2 ]; c = (x: x); };
}"##,
            // `inherit`, a name with a quote, another attribute with the same last names.
            r#"{ inherit (x) y; a'b = 1; other.hinteros.url = "z"; "quoted" = 1; inputs.hinteros.url = "@"; }"#,
        ] {
            let text = text.replace('@', STABLE);
            let out =
                retarget_flake_nix(&text, STABLE, BETA).unwrap_or_else(|e| panic!("{e}: {text}"));
            // The input alone: an `outputs` string with the same URL stays.
            let at = text.find(&format!("\"{STABLE}\"")).unwrap();
            let want = format!(
                "{}\"{BETA}\"{}",
                &text[..at],
                &text[at + STABLE.len() + 2..]
            );
            if text.contains("/* the channel */") {
                let at = text.rfind("/* the channel */").unwrap();
                let (head, tail) = text.split_at(at);
                assert_eq!(out, format!("{head}{}", tail.replacen(STABLE, BETA, 1)));
            } else {
                assert_eq!(out, want);
            }
        }
    }

    #[test]
    fn what_cannot_be_rewritten_is_an_error() {
        for (text, why) in [
            ("", "expected"),
            ("{ }", "0 times"),
            ("{ outputs = x; }", "0 times"),
            // Only in a comment.
            ("{ # inputs.hinteros.url = \"@\";\n}", "0 times"),
            (
                r#"{ inputs.hinteros.url = "@"; inputs.hinteros.url = "@"; }"#,
                "2 times",
            ),
            (r#"{ inputs.hinteros.url = "a${b}"; }"#, "plain"),
            (r#"{ inputs.hinteros.url = "a\"b"; }"#, "plain"),
            (r#"{ inputs.hinteros.url = ''@''; }"#, "plain"),
            (r#"{ inputs.hinteros.url = "@" + x; }"#, "plain"),
            (r#"{ inputs.hinteros.url = x; }"#, "plain"),
            // What this module does not enter.
            (r#"{ inputs = import ./x.nix; }"#, "0 times"),
            (
                r#"let x = 1; in { inputs.hinteros.url = "@"; }"#,
                "expected",
            ),
            // Broken files.
            ("{ a = 1", "ends"),
            ("{ a = \"x", "string"),
            ("{ /* x", "comment"),
            ("{ a = ); }", "unmatched"),
            (r#"{ inputs.hinteros.url = "@"; } x"#, "after"),
        ] {
            let text = text.replace('@', STABLE);
            let e = retarget_flake_nix(&text, STABLE, BETA)
                .unwrap_err()
                .to_string();
            assert!(e.contains(why), "{text}: {e}");
        }
        let text = format!(r#"{{ inputs.hinteros.url = "{STABLE}"; }}"#);
        assert!(retarget_flake_nix(&text, STABLE, "x\"; evil = \"").is_err());
    }
}
