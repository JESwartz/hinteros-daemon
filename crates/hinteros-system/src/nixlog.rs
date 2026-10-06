//! Nix's `--log-format internal-json` (lines `@nix {...}` on stderr) turned into job progress.
//!
//! Activity types (nix `ActivityType`): 100 copy path, 101 file transfer, 102 realise,
//! 103 copy paths, 104 builds, 105 build, 108 substitute. Result types: 105 progress
//! `[done, expected, running, failed]`, 106 set expected `[activity type, expected]`.

use std::collections::HashMap;

use hinteros_api::system::{ProgressDetail, Stage};
use serde_json::Value;

const ACT_COPY_PATH: u64 = 100;
const ACT_FILE_TRANSFER: u64 = 101;
const ACT_COPY_PATHS: u64 = 103;
const ACT_BUILDS: u64 = 104;
const ACT_BUILD: u64 = 105;
const ACT_SUBSTITUTE: u64 = 108;
const RES_PROGRESS: u64 = 105;
const RES_SET_EXPECTED: u64 = 106;

/// Longest error line kept for `log_hint`.
const MAX_HINT: usize = 400;

#[derive(Debug, Default)]
pub struct Progress {
    /// Activity id → type.
    activities: HashMap<u64, u64>,
    /// Per-activity byte progress of copies and transfers: id → (done, expected).
    bytes: HashMap<u64, (u64, u64)>,
    pub paths_done: u64,
    pub paths_total: u64,
    pub builds_done: u64,
    pub builds_total: u64,
    pub stage: Option<Stage>,
    /// The last error message nix printed (for `log_hint`).
    pub last_error: Option<String>,
}

impl Progress {
    /// Feed one stderr line; returns whether anything visible changed.
    pub fn line(&mut self, line: &str) -> bool {
        let Some(json) = line.strip_prefix("@nix ") else {
            if line.starts_with("error:") {
                self.last_error = Some(clip(line));
            }
            return false;
        };
        let Ok(v) = serde_json::from_str::<Value>(json) else {
            return false;
        };
        let id = v["id"].as_u64().unwrap_or(0);
        match v["action"].as_str() {
            Some("start") => {
                let ty = v["type"].as_u64().unwrap_or(0);
                if self.activities.len() < 100_000 {
                    self.activities.insert(id, ty);
                }
                let stage = match ty {
                    ACT_BUILDS | ACT_BUILD => Some(Stage::Building),
                    ACT_COPY_PATHS | ACT_COPY_PATH | ACT_SUBSTITUTE | ACT_FILE_TRANSFER => {
                        Some(Stage::Downloading)
                    }
                    _ => None,
                };
                // Building wins over downloading once it starts.
                if let Some(s) = stage
                    && self.stage != Some(Stage::Building)
                    && self.stage != Some(s)
                {
                    self.stage = Some(s);
                    return true;
                }
                false
            }
            Some("stop") => {
                self.activities.remove(&id);
                false
            }
            Some("result") => {
                let ty = v["type"].as_u64().unwrap_or(0);
                let fields: Vec<u64> = v["fields"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_u64).collect())
                    .unwrap_or_default();
                let act = self.activities.get(&id).copied();
                match (ty, act) {
                    (RES_PROGRESS, Some(ACT_COPY_PATHS)) if fields.len() >= 2 => {
                        self.paths_done = fields[0];
                        self.paths_total = fields[1];
                        true
                    }
                    (RES_PROGRESS, Some(ACT_BUILDS)) if fields.len() >= 2 => {
                        self.builds_done = fields[0];
                        self.builds_total = fields[1];
                        true
                    }
                    (RES_PROGRESS, Some(ACT_COPY_PATH | ACT_FILE_TRANSFER))
                        if fields.len() >= 2 =>
                    {
                        if self.bytes.len() < 100_000 || self.bytes.contains_key(&id) {
                            self.bytes.insert(id, (fields[0], fields[1]));
                        }
                        true
                    }
                    (RES_SET_EXPECTED, Some(ACT_COPY_PATHS)) if fields.len() >= 2 => {
                        if fields[0] == ACT_COPY_PATH {
                            self.paths_total = self.paths_total.max(fields[1]);
                        }
                        false
                    }
                    _ => false,
                }
            }
            Some("msg") => {
                if v["level"].as_u64() == Some(0)
                    && let Some(msg) = v["msg"].as_str()
                {
                    self.last_error = Some(clip(&strip_ansi(msg)));
                }
                false
            }
            _ => false,
        }
    }

    pub fn detail(&self) -> ProgressDetail {
        let (bytes_done, bytes_total) =
            self.bytes.values().fold((0u64, 0u64), |(d, t), (bd, bt)| {
                (d.saturating_add(*bd), t.saturating_add(*bt))
            });
        let nz = |n: u64| (n > 0).then_some(n);
        ProgressDetail {
            paths_done: nz(self.paths_total).map(|_| self.paths_done),
            paths_total: nz(self.paths_total),
            bytes_done: nz(bytes_total).map(|_| bytes_done),
            bytes_total: nz(bytes_total),
        }
    }

    /// Percent while downloading (paths, else bytes); `None` elsewhere.
    pub fn percent(&self) -> Option<u8> {
        if self.stage != Some(Stage::Downloading) {
            return None;
        }
        let d = self.detail();
        let (done, total) = match (d.paths_done, d.paths_total, d.bytes_done, d.bytes_total) {
            (Some(done), Some(total), _, _) => (done, total),
            (_, _, Some(done), Some(total)) => (done, total),
            _ => return None,
        };
        (total > 0).then(|| {
            u8::try_from(u128::from(done.min(total)) * 100 / u128::from(total)).unwrap_or(100)
        })
    }
}

fn clip(s: &str) -> String {
    s.trim().chars().take(MAX_HINT).collect()
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_a_download() {
        let mut p = Progress::default();
        let lines = [
            r#"@nix {"action":"start","id":1,"level":3,"type":103,"text":"","fields":[]}"#,
            r#"@nix {"action":"result","id":1,"type":106,"fields":[100,10]}"#,
            r#"@nix {"action":"result","id":1,"type":105,"fields":[4,10,1,0]}"#,
            r#"@nix {"action":"start","id":2,"level":4,"type":100,"text":"copying"}"#,
            r#"@nix {"action":"result","id":2,"type":105,"fields":[512,2048,0,0]}"#,
        ];
        for l in lines {
            p.line(l);
        }
        assert_eq!(p.stage, Some(Stage::Downloading));
        assert_eq!(
            p.detail(),
            ProgressDetail {
                paths_done: Some(4),
                paths_total: Some(10),
                bytes_done: Some(512),
                bytes_total: Some(2048)
            }
        );
        assert_eq!(p.percent(), Some(40));
        p.line(r#"@nix {"action":"start","id":3,"type":105,"text":"building foo"}"#);
        assert_eq!(p.stage, Some(Stage::Building));
        assert_eq!(p.percent(), None);
        p.line(r#"@nix {"action":"start","id":4,"type":100}"#);
        assert_eq!(p.stage, Some(Stage::Building), "building is not undone");
        p.line(
            r#"@nix {"action":"msg","level":0,"msg":"\u001b[31;1merror:\u001b[0m builder failed"}"#,
        );
        assert_eq!(p.last_error.as_deref(), Some("error: builder failed"));
        p.line("error: plain text error");
        assert_eq!(p.last_error.as_deref(), Some("error: plain text error"));
    }

    #[test]
    fn ignores_garbage() {
        let mut p = Progress::default();
        for l in [
            "",
            "@nix",
            "@nix {",
            "@nix []",
            r#"@nix {"action":"result","id":99,"type":105,"fields":[1]}"#,
            r#"@nix {"action":"result","id":1,"type":105,"fields":["a","b"]}"#,
        ] {
            p.line(l);
        }
        assert_eq!(p.stage, None);
        assert!(p.detail().is_empty());
        assert_eq!(p.percent(), None);
    }
}
