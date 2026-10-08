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

/// Point `flake.nix`'s hinteros input at another branch: the quoted input URL for `from` must
/// occur exactly once (the template the installer wrote); anything else is an error.
pub fn retarget_flake_nix(text: &str, from: &str, to: &str) -> Result<String> {
    let needle = format!("\"{from}\"");
    let count = text.matches(&needle).count();
    ensure!(
        count == 1,
        "flake.nix does not contain the hinteros input URL {from} exactly once (found {count})"
    );
    Ok(text.replacen(&needle, &format!("\"{to}\""), 1))
}

/// `release.json` of the running generation: `{"version": "...", "dev": bool}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Release {
    pub version: String,
    #[serde(default)]
    pub dev: bool,
    /// The highest `state.json` schema the generation's `state.nix` reads (M7: 2). Absent:
    /// 1.
    #[serde(default, rename = "stateSchema")]
    pub state_schema: Option<u32>,
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
            state_schema: None,
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
                      "url": "https://justsomeshed.com/JSwartz/HinterOS/hinteros.git" },
          "original": { "ref": "stable", "type": "git",
                        "url": "https://justsomeshed.com/JSwartz/HinterOS/hinteros.git" }
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
            r#"{"repo":"https://justsomeshed.com/JSwartz/HinterOS/hinteros.git","branches":{"stable":"stable","beta":"beta"}}"#,
        )
        .unwrap();
        t.validate().unwrap();
        assert_eq!(t.host(), "justsomeshed.com");
        assert_eq!(t.channel_of("beta"), Some(Channel::Beta));
        assert_eq!(
            t.input_url("stable", Some("ab")),
            "git+https://justsomeshed.com/JSwartz/HinterOS/hinteros.git?ref=stable&rev=ab"
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

    #[test]
    fn retargets_flake_nix() {
        let text = "{ inputs.hinteros.url = \"git+https://h/r.git?ref=stable\"; outputs = x; }";
        let out = retarget_flake_nix(
            text,
            "git+https://h/r.git?ref=stable",
            "git+https://h/r.git?ref=beta",
        )
        .unwrap();
        assert!(out.contains("\"git+https://h/r.git?ref=beta\""));
        assert!(retarget_flake_nix(text, "git+https://h/r.git?ref=beta", "x").is_err());
        let twice = format!("{text} \"git+https://h/r.git?ref=stable\"");
        assert!(retarget_flake_nix(&twice, "git+https://h/r.git?ref=stable", "x").is_err());
    }
}
