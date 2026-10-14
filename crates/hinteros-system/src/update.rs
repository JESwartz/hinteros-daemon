//! Update verification: the git side of `update.check`, `update.apply`, `channel.check` and
//! `channel.set`.
//!
//! Policy (threat-model.md, B4): the channel tip must carry an SSH signature by a principal in
//! the allowed signers file shipped in the *running* generation (`git verify-commit` with
//! `gpg.format=ssh`, which runs `ssh-keygen -Y verify`), and must descend from the locked
//! revision (fast-forward only within a channel). `update.apply` locks the verified commit id.
//! A `Security: <low|medium|high|critical>` trailer on any new commit marks the update as a
//! security update (the tip's signature covers its whole history); the newest
//! `Security-Summary:` trailer is the notice's one-line summary. The tip's `release/version`
//! file names the release (falling back to the short revision).
//!
//! [`SeenTips`] keeps the newest verified tip of each channel branch (`channel-tips.json` in the
//! state dir, root only). A tip must also descend from (or equal) that record, also on a channel
//! switch, where it may be older than the locked revision: so a server that serves an older
//! signed release again (one with a known hole) is refused. Update checks verify every channel
//! branch of the table, so the record exists before a switch. Rollbacks and switches never
//! lower it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use hinteros_api::system::{AvailableUpdate, Severity, is_rev};
use hinteros_api::{ErrorCode, ErrorObject};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Most commits read for trailers in one check.
pub const MAX_COMMITS: usize = 2000;

/// Why verification failed (the `reason` of `update_verification_failed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    Unsigned,
    UnknownSigner,
    NotFastForward,
    /// The tip does not descend from the newest verified tip this box saw on the branch.
    OlderThanSeen,
}

impl Rejection {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Unsigned => "unsigned",
            Self::UnknownSigner => "unknown_signer",
            Self::NotFastForward => "not_fast_forward",
            Self::OlderThanSeen => "older_than_seen",
        }
    }

    pub fn error(self) -> ErrorObject {
        let message = match self {
            Self::Unsigned => "the channel tip is not signed",
            Self::UnknownSigner => "the channel tip is not signed by an allowed signer",
            Self::NotFastForward => "the channel tip does not descend from the installed revision",
            Self::OlderThanSeen => {
                "the channel tip is older than a verified tip this box already saw"
            }
        };
        ErrorObject::with_data(
            ErrorCode::UpdateVerificationFailed,
            message,
            json!({ "reason": self.reason() }),
        )
    }
}

/// A parsed commit object (`git cat-file commit`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Commit {
    pub parents: Vec<String>,
    pub committer_time: Option<i64>,
    /// The `gpgsig` header, if any (continuation lines joined).
    pub signature: Option<String>,
    pub message: String,
}

impl Commit {
    pub fn parse(raw: &str) -> Self {
        let (headers, message) = raw.split_once("\n\n").unwrap_or((raw, ""));
        let mut c = Commit {
            message: message.to_owned(),
            ..Default::default()
        };
        let mut in_sig = false;
        for line in headers.lines() {
            if let Some(cont) = line.strip_prefix(' ') {
                if in_sig && let Some(sig) = c.signature.as_mut() {
                    sig.push('\n');
                    sig.push_str(cont);
                }
                continue;
            }
            in_sig = false;
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "parent" => c.parents.push(value.to_owned()),
                "committer" => {
                    // "Name <email> 1700000000 +0100"
                    let mut parts = value.rsplitn(3, ' ');
                    let _tz = parts.next();
                    c.committer_time = parts.next().and_then(|t| t.parse().ok());
                }
                "gpgsig" | "gpgsig-sha256" => {
                    c.signature = Some(value.to_owned());
                    in_sig = true;
                }
                _ => {}
            }
        }
        c
    }

    pub fn subject(&self) -> String {
        let s = self.message.lines().next().unwrap_or("").trim();
        s.chars().take(200).collect()
    }

    pub fn is_ssh_signature(&self) -> bool {
        self.signature
            .as_deref()
            .is_some_and(|s| s.trim_start().starts_with("-----BEGIN SSH SIGNATURE-----"))
    }
}

/// The trailers of a commit message: the last paragraph, if every line of it is a
/// `Token: value` line (continuation lines start with whitespace). A one-paragraph message has
/// none (that paragraph is the subject).
pub fn trailers(message: &str) -> Vec<(String, String)> {
    let paragraphs: Vec<&str> = message
        .split("\n\n")
        .map(|p| p.trim_matches('\n'))
        .filter(|p| !p.trim().is_empty())
        .collect();
    if paragraphs.len() < 2 {
        return Vec::new();
    }
    let last = paragraphs[paragraphs.len() - 1];
    let mut out: Vec<(String, String)> = Vec::new();
    for line in last.lines() {
        if line.starts_with([' ', '\t']) {
            match out.last_mut() {
                Some((_, v)) => {
                    v.push(' ');
                    v.push_str(line.trim());
                }
                None => return Vec::new(),
            }
            continue;
        }
        let Some((token, value)) = line.split_once(':') else {
            return Vec::new();
        };
        let token_ok = !token.is_empty()
            && token.len() <= 64
            && token.as_bytes()[0].is_ascii_alphanumeric()
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !token_ok {
            return Vec::new();
        }
        out.push((token.to_owned(), value.trim().to_owned()));
    }
    out
}

/// The severity a message's `Security:` trailer declares. Unknown values count as `high` (a
/// typo must not hide a security update); `no`/`none` count as none.
pub fn security_severity(message: &str) -> Option<Severity> {
    trailers(message)
        .into_iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("security"))
        .filter_map(|(_, v)| {
            let v = v.to_ascii_lowercase();
            let word = v.split_whitespace().next().unwrap_or("");
            match word {
                "" | "no" | "none" => None,
                "low" => Some(Severity::Low),
                "medium" | "moderate" => Some(Severity::Medium),
                "high" | "yes" | "important" => Some(Severity::High),
                "critical" => Some(Severity::Critical),
                _ => Some(Severity::High),
            }
        })
        .max()
}

/// Longest release version accepted from `release/version` (bytes).
pub const MAX_VERSION_LEN: usize = 32;
/// Longest `Security-Summary:` returned (characters).
pub const MAX_SECURITY_SUMMARY: usize = 120;
/// Length of the short revision reported when a release has no valid version.
pub const SHORT_REV: usize = 12;
/// Largest `release/version` file read (bytes).
const MAX_VERSION_FILE: u64 = 256;

/// The release version in a `release/version` file: its only non-empty line, trimmed, if it
/// matches `^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$` and is at most [`MAX_VERSION_LEN`] bytes.
pub fn parse_version(raw: &str) -> Option<String> {
    let mut lines = raw.lines().map(str::trim).filter(|l| !l.is_empty());
    let v = lines.next()?;
    if lines.next().is_some() || v.len() > MAX_VERSION_LEN {
        return None;
    }
    let (core, pre) = match v.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (v, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    let numeric = parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let pre_ok = pre.is_none_or(|p| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    });
    (numeric && pre_ok).then(|| v.to_owned())
}

/// The version an update reports: the parsed `release/version`, or the short revision.
pub fn version_or_short_rev(raw: Option<&str>, rev: &str) -> String {
    raw.and_then(parse_version)
        .unwrap_or_else(|| rev.chars().take(SHORT_REV).collect())
}

/// The last `Security-Summary:` trailer of a message, made safe to show on one line: control
/// and invisible formatting characters removed, whitespace runs collapsed, at most
/// [`MAX_SECURITY_SUMMARY`] characters. None if there is none or it is empty.
pub fn security_summary(message: &str) -> Option<String> {
    let (_, raw) = trailers(message)
        .into_iter()
        .rev()
        .find(|(k, _)| k.eq_ignore_ascii_case("security-summary"))?;
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !c.is_control() && !is_format_char(*c))
        .collect();
    let line: String = cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_SECURITY_SUMMARY)
        .collect();
    let line = line.trim_end();
    (!line.is_empty()).then(|| line.to_owned())
}

/// Invisible formatting characters that could reorder or hide text (bidi controls, zero-width).
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
    )
}

/// The principal from `git verify-commit`'s (ssh-keygen's) output:
/// `Good "git" signature for <principal> with ED25519 key SHA256:...`.
pub fn signer_from_output(output: &str) -> Option<String> {
    output.lines().find_map(|l| {
        let rest = l.split_once("Good \"git\" signature for ")?.1;
        let principal = rest.split(" with ").next()?.trim();
        (!principal.is_empty() && principal.len() <= 200).then(|| principal.to_owned())
    })
}

/// Errors of the git wrapper.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("{0}")]
    Unreachable(String),
    #[error("{0}")]
    Rejected(Rejection),
    #[error("{0}")]
    Failed(String),
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

/// The newest verified tip of each channel branch (`<state-dir>/channel-tips.json`, root only):
/// `{"<branch>": "<rev>"}`. Only verified revisions go in, and only forward (a revision that
/// descends from the recorded one), so nothing the update server serves later can lower it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SeenTips(BTreeMap<String, String>);

impl SeenTips {
    /// Read the record; a missing file is an empty record. An unreadable one is too (logged):
    /// only jobs write it, atomically, so that is damage, and the locked revision's
    /// fast-forward rule still holds within a channel.
    pub fn load(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!("cannot read {}: {e}", path.display());
                return Self::default();
            }
        };
        match serde_json::from_slice::<SeenTips>(&bytes) {
            Ok(mut s) => {
                s.0.retain(|b, r| crate::flake::is_branch(b) && is_rev(r));
                s
            }
            Err(e) => {
                tracing::warn!("{} is not valid: {e}", path.display());
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        crate::fsutil::write_atomic(path, &bytes, 0o600)
    }

    /// The newest verified tip seen on `branch`.
    pub fn get(&self, branch: &str) -> Option<&str> {
        self.0.get(branch).map(String::as_str)
    }

    /// Record `rev` (already verified) as `branch`'s newest tip if it descends from the
    /// recorded one. Returns whether the record changed.
    pub fn record(&mut self, git: &Git, branch: &str, rev: &str) -> Result<bool, GitError> {
        if !is_rev(rev) {
            return Ok(false);
        }
        if let Some(seen) = self.get(branch)
            && (seen == rev || !git.is_ancestor(seen, rev)?)
        {
            return Ok(false);
        }
        self.0.insert(branch.to_owned(), rev.to_owned());
        Ok(true)
    }
}

/// git, run as root against our own bare repo, with every configuration file ignored and only
/// the channel's transport allowed.
pub struct Git {
    pub git: String,
    pub ssh_keygen: String,
    pub git_dir: PathBuf,
    /// `https` or `file`.
    pub scheme: &'static str,
}

impl Git {
    fn command(&self) -> Command {
        let mut c = Command::new(&self.git);
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("HOME", "/nonexistent")
            .env("LC_ALL", "C")
            .args(["-c", "protocol.allow=never"])
            .arg("-c")
            .arg(format!("protocol.{}.allow=always", self.scheme))
            .arg("--git-dir")
            .arg(&self.git_dir)
            .stdin(Stdio::null());
        if let Ok(cafile) = std::env::var("SSL_CERT_FILE") {
            c.env("SSL_CERT_FILE", cafile);
        }
        c
    }

    fn run(&self, args: &[&str]) -> Result<Output, GitError> {
        self.command()
            .args(args)
            .output()
            .map_err(|e| GitError::Failed(format!("running {}: {e}", self.git)))
    }

    fn ok(&self, args: &[&str]) -> Result<String, GitError> {
        let out = self.run(args)?;
        if !out.status.success() {
            return Err(GitError::Failed(format!(
                "git {} failed: {}",
                args.first().unwrap_or(&""),
                last_line(&out.stderr)
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Create the bare repo if needed.
    pub fn init(&self) -> Result<(), GitError> {
        if self.git_dir.join("HEAD").exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.git_dir)
            .map_err(|e| GitError::Failed(format!("creating {}: {e}", self.git_dir.display())))?;
        let out = Command::new(&self.git)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["init", "--bare", "--quiet"])
            .arg(&self.git_dir)
            .output()
            .map_err(|e| GitError::Failed(format!("running {}: {e}", self.git)))?;
        if !out.status.success() {
            return Err(GitError::Failed(format!(
                "git init failed: {}",
                last_line(&out.stderr)
            )));
        }
        Ok(())
    }

    /// The tip of `branch` on `repo` (`git ls-remote`).
    pub fn ls_remote(&self, repo: &str, branch: &str) -> Result<String, GitError> {
        let refname = format!("refs/heads/{branch}");
        let out = self.run(&["ls-remote", "--", repo, &refname])?;
        if !out.status.success() {
            return Err(GitError::Unreachable(last_line(&out.stderr)));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines()
            .find_map(|l| {
                let (rev, name) = l.split_once('\t')?;
                (name == refname && is_rev(rev)).then(|| rev.to_owned())
            })
            .ok_or_else(|| GitError::Failed(format!("the server has no branch {branch}")))
    }

    /// Fetch `branch` into `refs/hinteros/<branch>`.
    pub fn fetch(&self, repo: &str, branch: &str) -> Result<(), GitError> {
        let refspec = format!("+refs/heads/{branch}:refs/hinteros/{branch}");
        let out = self.run(&["fetch", "--quiet", "--no-tags", "--", repo, &refspec])?;
        if !out.status.success() {
            return Err(GitError::Unreachable(last_line(&out.stderr)));
        }
        Ok(())
    }

    /// Fetch `branch` and return the commit it points to (what was fetched, so no race with a
    /// separate `ls-remote`).
    pub fn fetch_tip(&self, repo: &str, branch: &str) -> Result<String, GitError> {
        self.fetch(repo, branch)?;
        let refname = format!("refs/hinteros/{branch}^{{commit}}");
        let rev = self.ok(&["rev-parse", "--verify", "--quiet", &refname])?;
        let rev = rev.trim();
        if !is_rev(rev) {
            return Err(GitError::Failed(format!(
                "the server has no branch {branch}"
            )));
        }
        Ok(rev.to_owned())
    }

    pub fn has_commit(&self, rev: &str) -> bool {
        is_rev(rev)
            && self
                .run(&["cat-file", "-e", &format!("{rev}^{{commit}}")])
                .is_ok_and(|o| o.status.success())
    }

    pub fn commit(&self, rev: &str) -> Result<Commit, GitError> {
        Ok(Commit::parse(&self.ok(&["cat-file", "commit", rev])?))
    }

    /// Verify `rev`'s signature against `allowed_signers`; returns the signer's principal.
    pub fn verify(&self, rev: &str, allowed_signers: &Path) -> Result<String, GitError> {
        let commit = self.commit(rev)?;
        if commit.signature.is_none() {
            return Err(GitError::Rejected(Rejection::Unsigned));
        }
        if !commit.is_ssh_signature() || !allowed_signers.is_file() {
            return Err(GitError::Rejected(Rejection::UnknownSigner));
        }
        let out = self
            .command()
            .args(["-c", "gpg.format=ssh"])
            .arg("-c")
            .arg(format!("gpg.ssh.program={}", self.ssh_keygen))
            .arg("-c")
            .arg(format!(
                "gpg.ssh.allowedSignersFile={}",
                allowed_signers.display()
            ))
            .args(["verify-commit", "--raw", rev])
            .output()
            .map_err(|e| GitError::Failed(format!("running {}: {e}", self.git)))?;
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        match (out.status.success(), signer_from_output(&text)) {
            (true, Some(signer)) => Ok(signer),
            _ => Err(GitError::Rejected(Rejection::UnknownSigner)),
        }
    }

    /// The `release/version` file of `rev`, if it exists and is small.
    pub fn release_version(&self, rev: &str) -> Option<String> {
        if !is_rev(rev) {
            return None;
        }
        let object = format!("{rev}:release/version");
        let size: u64 = self
            .ok(&["cat-file", "-s", &object])
            .ok()?
            .trim()
            .parse()
            .ok()?;
        if size > MAX_VERSION_FILE {
            return None;
        }
        self.ok(&["cat-file", "blob", &object]).ok()
    }

    /// Whether `ancestor` is an ancestor of (or equal to) `rev`.
    pub fn is_ancestor(&self, ancestor: &str, rev: &str) -> Result<bool, GitError> {
        if !self.has_commit(ancestor) {
            return Ok(false);
        }
        let out = self.run(&["merge-base", "--is-ancestor", ancestor, rev])?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(GitError::Failed(format!(
                "git merge-base failed: {}",
                last_line(&out.stderr)
            ))),
        }
    }

    /// Commits in `from..to` (all of `to`'s history if `from` is unknown), newest first.
    pub fn new_commits(&self, from: Option<&str>, to: &str) -> Result<Vec<String>, GitError> {
        let max = format!("--max-count={MAX_COMMITS}");
        let range = match from.filter(|f| self.has_commit(f)) {
            Some(f) => format!("{f}..{to}"),
            None => to.to_owned(),
        };
        Ok(self
            .ok(&["rev-list", &max, &range])?
            .lines()
            .filter(|l| is_rev(l))
            .map(str::to_owned)
            .collect())
    }

    /// Fetch, verify and describe the tip of `branch`. `current`: the locked revision;
    /// `require_ff`: refuse a tip that does not descend from it. `seen`: the newest verified tip
    /// recorded for `branch` ([`SeenTips`]); the tip must descend from it too.
    #[allow(clippy::too_many_arguments)]
    pub fn check(
        &self,
        repo: &str,
        branch: &str,
        current: Option<&str>,
        require_ff: bool,
        seen: Option<&str>,
        allowed_signers: &Path,
        progress: &mut dyn FnMut(hinteros_api::system::Stage),
    ) -> Result<Option<AvailableUpdate>, GitError> {
        use hinteros_api::system::Stage;
        progress(Stage::Fetching);
        self.init()?;
        let tip = self.ls_remote(repo, branch)?;
        if current == Some(tip.as_str()) {
            return Ok(None);
        }
        self.fetch(repo, branch)?;
        progress(Stage::Verifying);
        self.describe(&tip, current, require_ff, seen, allowed_signers)
            .map(Some)
    }

    /// Verify an already fetched revision and describe it as an update from `current`. `seen`:
    /// the newest verified tip recorded for its branch; `rev` must descend from (or equal) it.
    pub fn describe(
        &self,
        rev: &str,
        current: Option<&str>,
        require_ff: bool,
        seen: Option<&str>,
        allowed_signers: &Path,
    ) -> Result<AvailableUpdate, GitError> {
        if !self.has_commit(rev) {
            return Err(GitError::Failed(format!("{rev} was not fetched")));
        }
        let signer = self.verify(rev, allowed_signers)?;
        if require_ff {
            let ff = match current {
                Some(c) => self.is_ancestor(c, rev)?,
                None => true,
            };
            if !ff {
                return Err(GitError::Rejected(Rejection::NotFastForward));
            }
        }
        if let Some(seen) = seen
            && !self.is_ancestor(seen, rev)?
        {
            return Err(GitError::Rejected(Rejection::OlderThanSeen));
        }
        let tip = self.commit(rev)?;
        let commits = self.new_commits(current, rev)?;
        let mut security = None;
        let mut summary = None;
        // Newest first: the first `Security-Summary:` found is the newest.
        for c in &commits {
            let msg = if c == rev {
                tip.message.clone()
            } else {
                self.commit(c)?.message
            };
            security = security.max(security_severity(&msg));
            if summary.is_none() {
                summary = security_summary(&msg);
            }
        }
        let version = version_or_short_rev(self.release_version(rev).as_deref(), rev);
        Ok(AvailableUpdate {
            rev: rev.to_owned(),
            date: tip.committer_time.unwrap_or(0),
            summary: tip.subject(),
            signer,
            security,
            commits: u32::try_from(commits.len().max(1)).unwrap_or(u32::MAX),
            // Filled in by the job from the target revision's catalog (`runner`).
            new_apps: None,
            removed_apps: None,
            version: Some(version),
            security_summary: summary,
        })
    }
}

fn last_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    line.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commits() {
        let raw = "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
                   parent 0f3c5a9e2b7d4c1a8e6f9b0d2c4e6a8b1d3f5a7c\n\
                   author A <a@x> 1700000000 +0000\n\
                   committer C <c@x> 1700000100 +0200\n\
                   gpgsig -----BEGIN SSH SIGNATURE-----\n \
                   U1NIU0lH\n \
                   -----END SSH SIGNATURE-----\n\
                   \n\
                   Release 0.2\n\
                   \n\
                   Body.\n\
                   \n\
                   Security: high\n";
        let c = Commit::parse(raw);
        assert_eq!(c.parents, ["0f3c5a9e2b7d4c1a8e6f9b0d2c4e6a8b1d3f5a7c"]);
        assert_eq!(c.committer_time, Some(1700000100));
        assert!(c.is_ssh_signature());
        assert!(
            c.signature
                .as_deref()
                .unwrap()
                .ends_with("-----END SSH SIGNATURE-----")
        );
        assert_eq!(c.subject(), "Release 0.2");
        assert_eq!(security_severity(&c.message), Some(Severity::High));
        assert!(!Commit::parse("tree x\n\nmsg").is_ssh_signature());
    }

    #[test]
    fn trailer_rules() {
        let t = trailers("Subject\n\nBody\n\nSecurity: critical\nSigned-off-by: A <a@x>\n");
        assert_eq!(
            t,
            [
                ("Security".to_string(), "critical".to_string()),
                ("Signed-off-by".to_string(), "A <a@x>".to_string())
            ]
        );
        // Continuation lines.
        let t = trailers("S\n\nSecurity: high\n  fixes CVE-1\n");
        assert_eq!(t[0].1, "high fixes CVE-1");
        // Not every line a trailer: no trailers.
        assert!(trailers("S\n\nSecurity: high\nnot a trailer\n").is_empty());
        // The subject is never a trailer block.
        assert!(trailers("Security: high").is_empty());
        assert!(trailers("Security: high\n").is_empty());
        // Only the last paragraph counts.
        assert!(trailers("S\n\nSecurity: high\n\nThe end.\n").is_empty());
        assert!(trailers("").is_empty());
        assert!(trailers("S\n\n bad: x\n").is_empty());
    }

    #[test]
    fn severities() {
        let m = |v: &str| security_severity(&format!("S\n\nSecurity: {v}\n"));
        assert_eq!(m("low"), Some(Severity::Low));
        assert_eq!(m("Medium"), Some(Severity::Medium));
        assert_eq!(m("CRITICAL (remote root)"), Some(Severity::Critical));
        assert_eq!(m("yes"), Some(Severity::High));
        assert_eq!(m("bananas"), Some(Severity::High));
        assert_eq!(m("no"), None);
        assert_eq!(m("none"), None);
        assert_eq!(security_severity("S\n\nBody"), None);
        assert_eq!(
            security_severity("S\n\nsecurity: low\nSECURITY: critical\n"),
            Some(Severity::Critical)
        );
    }

    #[test]
    fn versions() {
        for ok in [
            "0.6.1",
            "10.20.30",
            "0.6.1-rc.1",
            "1.0.0-beta-2",
            "0.6.1\n",
            " 0.6.1 \n\n",
        ] {
            assert_eq!(parse_version(ok).as_deref(), Some(ok.trim()), "{ok:?}");
        }
        for bad in [
            "",
            "\n",
            "0.6",
            "0.6.1.2",
            "v0.6.1",
            "0.6.1-",
            "0.6.1+build",
            "0.6.1-rc 1",
            "0..1",
            "a.b.c",
            "0.6.1\n0.6.2",
            "0.6.1-ü",
        ] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
        // 32 bytes is the limit.
        let max = format!("1.2.3-{}", "a".repeat(MAX_VERSION_LEN - 6));
        assert_eq!(parse_version(&max).as_deref(), Some(max.as_str()));
        assert_eq!(parse_version(&format!("{max}a")), None);
        let rev = "9a8b7c6d5e4f30211a2b3c4d5e6f708192a3b4c5";
        assert_eq!(version_or_short_rev(Some("0.6.1\n"), rev), "0.6.1");
        assert_eq!(version_or_short_rev(Some("garbage"), rev), "9a8b7c6d5e4f");
        assert_eq!(version_or_short_rev(None, rev), "9a8b7c6d5e4f");
    }

    #[test]
    fn security_summaries() {
        let m = "HinterOS 0.6.3\n\nFirefox 131.0.3.\n\nSecurity: high\n\
                 Security-Summary: A website could take over the browser in a profile.\n";
        assert_eq!(
            security_summary(m).as_deref(),
            Some("A website could take over the browser in a profile.")
        );
        // None, empty, or not in the trailer block.
        assert_eq!(security_summary("S\n\nSecurity: high\n"), None);
        assert_eq!(security_summary("S\n\nSecurity-Summary:   \n"), None);
        assert_eq!(
            security_summary("S\n\nSecurity-Summary: x\n\nThe end.\n"),
            None
        );
        // The last one in a message wins; case-insensitive key.
        assert_eq!(
            security_summary("S\n\nsecurity-summary: one\nSECURITY-SUMMARY: two\n").as_deref(),
            Some("two")
        );
        // Continuation lines are joined; whitespace collapsed; control and bidi chars removed.
        assert_eq!(
            security_summary("S\n\nSecurity-Summary: a\tb\x07c \u{202E}d\n  e\n").as_deref(),
            Some("a bc d e")
        );
        // Truncated to 120 characters.
        let long = "é".repeat(200);
        let s = security_summary(&format!("S\n\nSecurity-Summary: {long}\n")).unwrap();
        assert_eq!(s.chars().count(), MAX_SECURITY_SUMMARY);
        // No trailing space left by truncation.
        let words = "ab ".repeat(60);
        let s = security_summary(&format!("S\n\nSecurity-Summary: {words}x\n")).unwrap();
        assert!(!s.ends_with(' ') && s.chars().count() <= MAX_SECURITY_SUMMARY);
    }

    #[test]
    fn signer_output() {
        let out = "Good \"git\" signature for release@hinteros with ED25519 key SHA256:abc\n";
        assert_eq!(signer_from_output(out).as_deref(), Some("release@hinteros"));
        assert_eq!(signer_from_output("Could not verify signature."), None);
        assert_eq!(
            signer_from_output("Good \"git\" signature for  with x"),
            None
        );
    }

    /// A work repo with commits `a <- b <- c` and `a <- d`; the [`Git`] runs against its
    /// `.git`. `None` without git on PATH.
    fn history(dir: &Path) -> Option<(Git, [String; 4])> {
        let run = |args: &[&str]| -> Option<String> {
            let out = Command::new("git")
                .current_dir(dir)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(["-c", "user.name=T", "-c", "user.email=t@x"])
                .args(args)
                .output()
                .ok()?;
            assert!(out.status.success(), "git {args:?}");
            Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        };
        run(&["init", "--quiet"])?;
        let commit = |m: &str| -> Option<String> {
            run(&["commit", "--quiet", "--allow-empty", "-m", m])?;
            run(&["rev-parse", "HEAD"])
        };
        let a = commit("a")?;
        let b = commit("b")?;
        let c = commit("c")?;
        run(&["checkout", "--quiet", "--detach", &a])?;
        let d = commit("d")?;
        let git = Git {
            git: "git".into(),
            ssh_keygen: "ssh-keygen".into(),
            git_dir: dir.join(".git"),
            scheme: "file",
        };
        Some((git, [a, b, c, d]))
    }

    #[test]
    fn seen_tips_only_move_forward() {
        let dir = tempfile::tempdir().unwrap();
        let Some((git, [a, b, c, d])) = history(dir.path()) else {
            return;
        };
        let path = dir.path().join("state/channel-tips.json");
        let mut tips = SeenTips::load(&path);
        assert_eq!(tips, SeenTips::default(), "no file: no record");
        assert!(tips.record(&git, "stable", &b).unwrap(), "first record");
        assert!(!tips.record(&git, "stable", &b).unwrap(), "the same");
        assert!(!tips.record(&git, "stable", &a).unwrap(), "older");
        assert!(!tips.record(&git, "stable", &d).unwrap(), "not on the line");
        assert!(!tips.record(&git, "stable", "HEAD").unwrap(), "not a rev");
        assert_eq!(tips.get("stable"), Some(b.as_str()));
        assert!(tips.record(&git, "stable", &c).unwrap(), "descendant");
        assert!(tips.record(&git, "beta", &d).unwrap(), "per branch");
        tips.save(&path).unwrap();
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&path).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);
        let back = SeenTips::load(&path);
        assert_eq!(back, tips);
        assert_eq!(back.get("stable"), Some(c.as_str()));
        assert_eq!(back.get("beta"), Some(d.as_str()));
        assert_eq!(back.get("main"), None);
    }

    #[test]
    fn seen_tips_drop_what_is_not_a_branch_and_rev() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("channel-tips.json");
        let rev = "9a8b7c6d5e4f30211a2b3c4d5e6f708192a3b4c5";
        std::fs::write(
            &path,
            format!(r#"{{"stable":"{rev}","beta":"HEAD","../x":"{rev}"}}"#),
        )
        .unwrap();
        let tips = SeenTips::load(&path);
        assert_eq!(tips.get("stable"), Some(rev));
        assert_eq!(tips.get("beta"), None);
        assert_eq!(tips.get("../x"), None);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(SeenTips::load(&path), SeenTips::default());
    }

    #[test]
    fn older_than_seen_reason() {
        let e = Rejection::OlderThanSeen.error();
        assert_eq!(e.kind(), Some(ErrorCode::UpdateVerificationFailed));
        assert_eq!(e.data.unwrap(), json!({"reason": "older_than_seen"}));
    }
}
