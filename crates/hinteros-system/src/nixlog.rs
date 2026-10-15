//! Nix's `--log-format internal-json` (lines `@nix {...}` on stderr) turned into job progress.
//!
//! Activity types (nix `ActivityType`): 100 copy path, 101 file transfer, 102 realise,
//! 103 copy paths, 104 builds, 105 build, 108 substitute. Result types: 101 build log line,
//! 104 set phase `[phase]`, 105 progress `[done, expected, running, failed]`, 106 set expected
//! `[activity type, expected]`. A build activity (105) starts with
//! `fields: [drv path, machine, round, rounds]` and text `building '<drv path>'`.
//!
//! How far one build is comes from its stdenv phase ([`PHASES`]); the build part of the job's
//! percent counts finished builds plus the running builds' fractions.

use std::collections::{HashMap, HashSet};

use hinteros_api::system::{Building, ProgressDetail, Stage};
use serde_json::Value;

const ACT_COPY_PATH: u64 = 100;
const ACT_FILE_TRANSFER: u64 = 101;
const ACT_COPY_PATHS: u64 = 103;
const ACT_BUILDS: u64 = 104;
const ACT_BUILD: u64 = 105;
const ACT_SUBSTITUTE: u64 = 108;
const RES_SET_PHASE: u64 = 104;
const RES_PROGRESS: u64 = 105;
const RES_SET_EXPECTED: u64 = 106;

/// Longest error line kept for `log_hint`.
const MAX_HINT: usize = 400;
/// Longest derivation name or version kept.
const MAX_NAME: usize = 80;
/// The percent stays below this until the nix run has ended.
const MAX_RUNNING_PERCENT: u8 = 99;

/// The standard stdenv phases in order (names without `Phase`), with the fraction of the build
/// done when each starts. Compiling (`build`) gets the biggest share.
const PHASES: &[(&str, f64)] = &[
    ("unpack", 0.0),
    ("patch", 0.05),
    ("updateAutotoolsGnuConfigScripts", 0.08),
    ("configure", 0.10),
    ("build", 0.20),
    ("check", 0.80),
    ("install", 0.88),
    ("fixup", 0.94),
    ("installCheck", 0.98),
    ("dist", 0.99),
];

/// A derivation being built.
#[derive(Debug, Clone)]
struct Build {
    /// Start order (the most recent running build is shown).
    seq: u64,
    name: String,
    version: Option<String>,
    phase: Option<String>,
    /// How far it is, from the last standard phase it started (never goes back); `None` until
    /// one starts (builders without phases, such as `runCommand`, never have one).
    fraction: Option<f64>,
    /// Nix already counts it in `builds_done` (its activity has not stopped yet).
    counted: bool,
}

#[derive(Debug, Default)]
pub struct Progress {
    /// Activity id → type.
    activities: HashMap<u64, u64>,
    /// Per-activity byte progress of copies and transfers: id → (done, expected).
    bytes: HashMap<u64, (u64, u64)>,
    /// File transfers that bring a store path (their bytes count).
    payload: HashSet<u64>,
    /// Running build activities.
    builds: HashMap<u64, Build>,
    /// Build activities that stopped before nix counted them as done.
    stopped_uncounted: u64,
    seq: u64,
    pub paths_done: u64,
    pub paths_total: u64,
    pub builds_done: u64,
    pub builds_total: u64,
    pub builds_running: u64,
    pub stage: Option<Stage>,
    /// The percent shown so far (it never goes back), exact and floored.
    shown: Option<f64>,
    best: Option<u8>,
    /// Where the fraction last dropped: (fraction, percent held then).
    base: Option<(f64, f64)>,
    /// Totals from nix's plan (`these N derivations will be built`, `these N paths will be
    /// fetched`), printed before the work starts.
    planned_builds: u64,
    planned_paths: u64,
    /// The last error message nix printed (for `log_hint`).
    pub last_error: Option<String>,
}

/// Split a store derivation path (`/nix/store/<hash>-<name>-<version>.drv`) into a short name
/// and version, by nix's rule: the version starts at the first `-` followed by a non-letter.
pub fn drv_name(path: &str) -> Option<(String, Option<String>)> {
    let base = path.rsplit('/').next()?;
    let base = base.strip_suffix(".drv").unwrap_or(base);
    // A store path's hash part: 32 base-32 characters and a dash.
    let full = match base.split_once('-') {
        Some((hash, rest))
            if hash.len() == 32 && hash.bytes().all(|b| b.is_ascii_alphanumeric()) =>
        {
            rest
        }
        _ => base,
    };
    if full.is_empty() || full.chars().any(char::is_control) {
        return None;
    }
    let bytes = full.as_bytes();
    let split = (0..bytes.len())
        .find(|&i| bytes[i] == b'-' && bytes.get(i + 1).is_some_and(|c| !c.is_ascii_alphabetic()));
    let clip = |s: &str| s.chars().take(MAX_NAME).collect::<String>();
    Some(match split {
        Some(i) if i > 0 => (
            clip(&full[..i]),
            Some(clip(&full[i + 1..])).filter(|v| !v.is_empty()),
        ),
        _ => (clip(full), None),
    })
}

/// The fraction of a build done when a standard phase starts; `None` for other phases.
fn phase_fraction(phase: &str) -> Option<f64> {
    PHASES.iter().find(|(p, _)| *p == phase).map(|(_, f)| *f)
}

/// A build's fraction as its shown percent: floored, below 100 while it runs.
fn percent_of(fraction: f64) -> u8 {
    ((fraction * 100.0).floor() as u8).min(MAX_RUNNING_PERCENT)
}

/// `unpackPhase` → `unpack`; other names as they are (clipped).
fn phase_name(phase: &str) -> String {
    let p = phase
        .strip_suffix("Phase")
        .filter(|p| !p.is_empty())
        .unwrap_or(phase);
    p.chars().filter(|c| !c.is_control()).take(40).collect()
}

impl Progress {
    /// Feed one stderr line; returns whether anything visible changed.
    pub fn line(&mut self, line: &str) -> bool {
        let changed = self.parse(line);
        if changed {
            self.update_best();
        }
        changed
    }

    fn parse(&mut self, line: &str) -> bool {
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
                let parent = v["parent"]
                    .as_u64()
                    .and_then(|p| self.activities.get(&p).copied());
                // A store path coming in; a file transfer counts only as part of one (not the
                // narinfo lookups nix makes while it plans).
                let payload = match ty {
                    ACT_COPY_PATH | ACT_SUBSTITUTE => true,
                    ACT_FILE_TRANSFER => matches!(parent, Some(ACT_COPY_PATH | ACT_SUBSTITUTE)),
                    _ => false,
                };
                if self.activities.len() < 100_000 {
                    self.activities.insert(id, ty);
                    if ty == ACT_FILE_TRANSFER && payload {
                        self.payload.insert(id);
                    }
                }
                let mut changed = false;
                if ty == ACT_BUILD && self.builds.len() < 10_000 {
                    let drv = v["fields"][0]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| drv_from_text(v["text"].as_str().unwrap_or("")));
                    if let Some((name, version)) = drv.as_deref().and_then(drv_name) {
                        self.seq += 1;
                        self.builds.insert(
                            id,
                            Build {
                                seq: self.seq,
                                name,
                                version,
                                phase: None,
                                fraction: None,
                                counted: false,
                            },
                        );
                        changed = true;
                    }
                }
                let stage = match ty {
                    ACT_BUILDS | ACT_BUILD => Some(Stage::Building),
                    _ if payload => Some(Stage::Downloading),
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
                changed
            }
            Some("stop") => {
                let ty = self.activities.remove(&id);
                self.payload.remove(&id);
                if ty == Some(ACT_BUILDS) {
                    // Nix is done building: what it built is the total (its last progress
                    // can still count a finished goal twice, e.g. [2, 3, 0, 0] for 2 builds).
                    self.builds_total = self.builds_done;
                    self.planned_builds = self.builds_done;
                    self.builds_running = 0;
                    self.stopped_uncounted = 0;
                    return true;
                }
                // A finished download's bytes stay counted (the totals stay honest).
                match self.builds.remove(&id) {
                    Some(b) => {
                        if !b.counted {
                            self.stopped_uncounted += 1;
                        }
                        true
                    }
                    None => false,
                }
            }
            Some("result") => {
                let ty = v["type"].as_u64().unwrap_or(0);
                if ty == RES_SET_PHASE {
                    let Some(build) = self.builds.get_mut(&id) else {
                        return false;
                    };
                    let phase = v["fields"][0].as_str().map(phase_name);
                    if phase.is_some() && build.phase != phase {
                        // An unknown phase keeps the fraction of the last known one.
                        if let Some(f) = phase.as_deref().and_then(phase_fraction)
                            && build.fraction.is_none_or(|old| f > old)
                        {
                            build.fraction = Some(f);
                        }
                        build.phase = phase;
                        return true;
                    }
                    return false;
                }
                let fields: Vec<u64> = v["fields"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_u64).collect())
                    .unwrap_or_default();
                let act = self.activities.get(&id).copied();
                match (ty, act) {
                    (RES_PROGRESS, Some(ACT_COPY_PATHS)) if fields.len() >= 2 => {
                        self.paths_done = fields[0];
                        self.paths_total = fields[1];
                        if fields[1] > 0 && self.stage.is_none_or(|s| s != Stage::Building) {
                            self.stage = Some(Stage::Downloading);
                        }
                        true
                    }
                    (RES_PROGRESS, Some(ACT_BUILDS)) if fields.len() >= 2 => {
                        self.count_done(fields[0].saturating_sub(self.builds_done));
                        self.builds_done = fields[0];
                        self.builds_total = fields[1];
                        self.builds_running = fields.get(2).copied().unwrap_or(0);
                        true
                    }
                    (RES_PROGRESS, Some(ACT_COPY_PATH | ACT_FILE_TRANSFER))
                        if fields.len() >= 2
                            && (act == Some(ACT_COPY_PATH) || self.payload.contains(&id)) =>
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
                    (RES_SET_EXPECTED, Some(ACT_BUILDS)) if fields.len() >= 2 => {
                        if fields[0] == ACT_BUILD && fields[1] > self.builds_total {
                            self.builds_total = fields[1];
                            return true;
                        }
                        false
                    }
                    _ => false,
                }
            }
            Some("msg") => {
                let Some(msg) = v["msg"].as_str() else {
                    return false;
                };
                let msg = strip_ansi(msg);
                if v["level"].as_u64() == Some(0) {
                    self.last_error = Some(clip(&msg));
                    return false;
                }
                match plan(&msg) {
                    Some(Plan::Builds(n)) if n > self.planned_builds => {
                        self.planned_builds = n;
                        true
                    }
                    Some(Plan::Paths(n)) if n > self.planned_paths => {
                        self.planned_paths = n;
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// The most recently started build that is still running.
    fn building(&self) -> Option<Building> {
        self.builds
            .values()
            .max_by_key(|b| b.seq)
            .map(|b| Building {
                name: b.name.clone(),
                version: b.version.clone(),
                phase: b.phase.clone(),
                percent: b.fraction.map(percent_of),
            })
    }

    /// The running builds' fractions summed (a build without phases counts 0; one nix already
    /// counts as done does not count), plus 1 for each build that stopped before nix counted
    /// it (it is done; nix's count follows).
    fn running_fraction(&self) -> f64 {
        self.builds
            .values()
            .filter(|b| !b.counted)
            .filter_map(|b| b.fraction)
            .sum::<f64>()
            + self.stopped_uncounted as f64
    }

    /// Nix counted `n` more builds as done. Nix counts a build before its build activity stops;
    /// so builds that stopped uncounted take the count first, then the most advanced running
    /// builds (a finished build went through all its phases) stop counting by their fraction.
    fn count_done(&mut self, n: u64) {
        let stopped = n.min(self.stopped_uncounted);
        self.stopped_uncounted -= stopped;
        for _ in stopped..n {
            let Some(b) = self
                .builds
                .values_mut()
                .filter(|b| !b.counted)
                .max_by(|a, b| {
                    let f = |b: &Build| b.fraction.unwrap_or(0.0);
                    f(a).total_cmp(&f(b)).then(b.seq.cmp(&a.seq))
                })
            else {
                break;
            };
            b.counted = true;
        }
    }

    pub fn detail(&self) -> ProgressDetail {
        let (bytes_done, bytes_total) =
            self.bytes.values().fold((0u64, 0u64), |(d, t), (bd, bt)| {
                (d.saturating_add(*bd), t.saturating_add(*bt))
            });
        let nz = |n: u64| (n > 0).then_some(n);
        let running = self.builds_running.max(self.builds.len() as u64);
        let paths_total = self.paths_total.max(self.planned_paths);
        let builds_total = self.builds_total.max(self.planned_builds);
        ProgressDetail {
            paths_done: nz(paths_total).map(|_| self.paths_done),
            paths_total: nz(paths_total),
            bytes_done: nz(bytes_total).map(|_| bytes_done),
            bytes_total: nz(bytes_total),
            builds_done: nz(builds_total).map(|_| self.builds_done),
            builds_total: nz(builds_total),
            builds_running: nz(running),
            building: self.building(),
        }
    }

    /// The fraction done right now: the mean of the known parts (downloaded paths, else bytes;
    /// built derivations, the running ones by their phase), each weighted equally. Can go down
    /// when nix raises a total.
    fn fraction(&self) -> Option<f64> {
        let d = self.detail();
        let part = |done: Option<u64>, total: Option<u64>| match (done, total) {
            (Some(done), Some(total)) if total > 0 => Some(done.min(total) as f64 / total as f64),
            _ => None,
        };
        let download =
            part(d.paths_done, d.paths_total).or_else(|| part(d.bytes_done, d.bytes_total));
        let build = match (d.builds_done, d.builds_total) {
            (Some(done), Some(total)) if total > 0 => {
                Some(((done.min(total) as f64 + self.running_fraction()) / total as f64).min(1.0))
            }
            _ => None,
        };
        match (download, build) {
            (Some(a), Some(b)) => Some((a + b) / 2.0),
            (a, b) => a.or(b),
        }
    }

    /// The shown percent follows the fraction, but never goes back: when the fraction drops
    /// (nix found more work), the percent holds and from then on covers the rest of the way
    /// (`fraction` from where it dropped to 1 maps to the held percent up to 100).
    fn update_best(&mut self) {
        let Some(f) = self.fraction() else { return };
        let held = self.shown.unwrap_or(0.0);
        let p = match self.base {
            None => f * 100.0,
            Some((f0, p0)) if f0 < 1.0 => p0 + (100.0 - p0) * (f - f0) / (1.0 - f0),
            Some((_, p0)) => p0,
        };
        let p = if p < held {
            self.base = Some((f, held));
            held
        } else {
            p
        };
        // Below 100 while nix runs.
        let p = p.min(f64::from(MAX_RUNNING_PERCENT));
        self.shown = Some(p);
        self.best = Some(p.floor() as u8);
    }

    /// Percent while downloading or building (see [`Self::fraction`]); never lower than a
    /// percent reported before, below 100 while nix runs; `None` before anything is countable
    /// and outside those stages.
    pub fn percent(&self) -> Option<u8> {
        match self.stage {
            Some(Stage::Downloading | Stage::Building) => self.best,
            _ => None,
        }
    }
}

/// What nix plans to do, from the message it prints before starting.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Builds(u64),
    Paths(u64),
}

/// `this derivation will be built:` / `these 5 derivations will be built:` /
/// `this path will be fetched (…)` / `these 12 paths will be fetched (…)` (the first line).
fn plan(msg: &str) -> Option<Plan> {
    let first = msg.lines().next()?.trim();
    let count = |rest: &str| -> Option<u64> {
        if rest.starts_with("this ") {
            Some(1)
        } else {
            rest.strip_prefix("these ")?.split(' ').next()?.parse().ok()
        }
    };
    if first.contains(" will be built") && first.contains("derivation") {
        count(first).map(Plan::Builds)
    } else if first.contains(" will be fetched") || first.contains(" will be downloaded") {
        count(first).map(Plan::Paths)
    } else {
        None
    }
}

/// The drv path from a build activity's text (`building '/nix/store/...drv'`).
fn drv_from_text(text: &str) -> Option<String> {
    let start = text.find("'/")? + 1;
    let end = start + text[start..].find('\'')?;
    Some(text[start..end].to_owned())
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
                bytes_total: Some(2048),
                ..Default::default()
            }
        );
        assert_eq!(p.percent(), Some(40));
        p.line(r#"@nix {"action":"start","id":3,"type":105,"text":"building foo"}"#);
        assert_eq!(p.stage, Some(Stage::Building));
        assert_eq!(p.percent(), Some(40), "still the download's percent");
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
            r#"@nix {"action":"result","id":7,"type":104,"fields":["buildPhase"]}"#,
            r#"@nix {"action":"start","id":8,"type":105,"text":"building","fields":[]}"#,
        ] {
            p.line(l);
        }
        assert_eq!(p.stage, Some(Stage::Building));
        assert!(p.detail().is_empty());
        assert_eq!(p.percent(), None);
    }

    #[test]
    fn derivation_names() {
        let n = |p: &str| drv_name(p);
        assert_eq!(
            n("/nix/store/0c1mfwi4vbb6m2s0q5zk9n7f8f8b8z3v-hinteros-daemon-0.7.0.drv"),
            Some(("hinteros-daemon".into(), Some("0.7.0".into())))
        );
        assert_eq!(
            n("/nix/store/0c1mfwi4vbb6m2s0q5zk9n7f8f8b8z3v-etc.drv"),
            Some(("etc".into(), None))
        );
        assert_eq!(
            n(
                "/nix/store/0c1mfwi4vbb6m2s0q5zk9n7f8f8b8z3v-nixos-system-hinteros-25.11.20261001.abcdef0.drv"
            ),
            Some((
                "nixos-system-hinteros".into(),
                Some("25.11.20261001.abcdef0".into())
            ))
        );
        assert_eq!(
            n(
                "/nix/store/0c1mfwi4vbb6m2s0q5zk9n7f8f8b8z3v-hinteros-launcher-0.7.1-vendor.tar.gz.drv"
            ),
            Some((
                "hinteros-launcher".into(),
                Some("0.7.1-vendor.tar.gz".into())
            ))
        );
        assert_eq!(
            n("/nix/store/0c1mfwi4vbb6m2s0q5zk9n7f8f8b8z3v-unit-script-hinteros-job-start.drv"),
            Some(("unit-script-hinteros-job-start".into(), None))
        );
        assert_eq!(n("/nix/store/0c1mfwi4vbb6m2s0q5zk9n7f8f8b8z3v-.drv"), None);
        assert_eq!(phase_name("unpackPhase"), "unpack");
        assert_eq!(phase_name("Phase"), "Phase");
        assert_eq!(phase_name("custom"), "custom");
    }

    /// A system rebuild as `nix build --log-format internal-json -v` prints it: substitutions,
    /// then builds of two derivations (one long compile with log lines and phases).
    #[test]
    fn follows_a_build() {
        let drv = |h: &str, n: &str| format!("/nix/store/{h:a<32}-{n}.drv");
        let daemon = drv("d", "hinteros-daemon-0.7.1");
        let etc = drv("e", "etc");
        let toplevel = drv("t", "nixos-system-hinteros-25.11");
        let mut lines = vec![
            r#"@nix {"action":"start","id":10,"level":0,"parent":0,"text":"","type":104,"fields":[]}"#.to_string(),
            r#"@nix {"action":"start","id":11,"level":0,"parent":0,"text":"","type":103,"fields":[]}"#.into(),
            r#"@nix {"action":"result","id":10,"type":105,"fields":[0,3,0,0]}"#.into(),
            r#"@nix {"action":"result","id":11,"type":105,"fields":[0,4,0,0]}"#.into(),
            r#"@nix {"action":"start","id":12,"level":4,"parent":0,"text":"copying path","type":100,"fields":["/nix/store/x","https://cache.nixos.org"]}"#.into(),
            r#"@nix {"action":"result","id":11,"type":105,"fields":[2,4,1,0]}"#.into(),
            r#"@nix {"action":"result","id":11,"type":105,"fields":[4,4,0,0]}"#.into(),
            r#"@nix {"action":"stop","id":12}"#.into(),
        ];
        lines.push(format!(
            r#"@nix {{"action":"start","id":20,"level":3,"parent":0,"text":"building '{daemon}'","type":105,"fields":["{daemon}","",1,1]}}"#
        ));
        lines.push(r#"@nix {"action":"result","id":10,"type":105,"fields":[0,3,1,0]}"#.into());
        let mut p = Progress::default();
        let mut percents = Vec::new();
        for l in &lines {
            p.line(l);
            percents.extend(p.percent());
        }
        assert_eq!(p.stage, Some(Stage::Building));
        let d = p.detail();
        assert_eq!(
            (d.builds_done, d.builds_total, d.builds_running),
            (Some(0), Some(3), Some(1))
        );
        assert_eq!(
            d.building,
            Some(Building {
                name: "hinteros-daemon".into(),
                version: Some("0.7.1".into()),
                phase: None,
                percent: None
            })
        );
        assert_eq!(p.percent(), Some(50), "downloads done, no build yet");

        // Phases and build log lines (log lines change nothing visible).
        assert!(p.line(r#"@nix {"action":"result","id":20,"type":104,"fields":["unpackPhase"]}"#));
        assert!(!p.line(
            r#"@nix {"action":"result","id":20,"type":101,"fields":["   Compiling serde v1.0"]}"#
        ));
        assert!(p.line(r#"@nix {"action":"result","id":20,"type":104,"fields":["buildPhase"]}"#));
        assert!(!p.line(r#"@nix {"action":"result","id":20,"type":104,"fields":["buildPhase"]}"#));
        assert_eq!(p.detail().building.unwrap().phase.as_deref(), Some("build"));

        // A second, parallel build is the one shown; when it stops the first one is again.
        p.line(&format!(
            r#"@nix {{"action":"start","id":21,"type":105,"text":"building '{etc}'","fields":[]}}"#
        ));
        assert_eq!(p.detail().building.unwrap().name, "etc");
        assert_eq!(p.detail().builds_running, Some(2));
        p.line(r#"@nix {"action":"stop","id":21}"#);
        p.line(r#"@nix {"action":"result","id":10,"type":105,"fields":[1,3,1,0]}"#);
        assert_eq!(p.detail().building.unwrap().name, "hinteros-daemon");
        percents.extend(p.percent());

        // Nix discovers more work: the total grows, the percent holds.
        p.line(r#"@nix {"action":"result","id":10,"type":105,"fields":[1,6,1,0]}"#);
        percents.extend(p.percent());
        assert_eq!(p.detail().builds_total, Some(6));
        p.line(r#"@nix {"action":"stop","id":20}"#);
        p.line(&format!(
            r#"@nix {{"action":"start","id":22,"type":105,"text":"building '{toplevel}'","fields":["{toplevel}","",1,1]}}"#
        ));
        p.line(r#"@nix {"action":"result","id":10,"type":105,"fields":[5,6,1,0]}"#);
        percents.extend(p.percent());
        p.line(r#"@nix {"action":"result","id":10,"type":105,"fields":[6,6,0,0]}"#);
        p.line(r#"@nix {"action":"stop","id":22}"#);
        percents.extend(p.percent());
        assert!(p.detail().building.is_none());
        assert!(
            percents.windows(2).all(|w| w[0] <= w[1]),
            "percent never goes back: {percents:?}"
        );
        assert_eq!(p.percent(), Some(99), "below 100 while nix runs");
        assert_eq!(
            percents[percents.len() - 3],
            70,
            "1 of 3 built and one in its build phase, downloads done"
        );
    }

    fn build_start(id: u64, name: &str) -> String {
        let drv = format!("/nix/store/{:a<32}-{name}.drv", "h");
        format!(
            r#"@nix {{"action":"start","id":{id},"type":105,"text":"building '{drv}'","fields":["{drv}","",1,1]}}"#
        )
    }

    fn phase(id: u64, phase: &str) -> String {
        format!(r#"@nix {{"action":"result","id":{id},"type":104,"fields":["{phase}"]}}"#)
    }

    fn builds(done: u64, total: u64, running: u64) -> String {
        format!(
            r#"@nix {{"action":"result","id":1,"type":105,"fields":[{done},{total},{running},0]}}"#
        )
    }

    /// The item's percent and the job's percent follow the stdenv phases; neither goes back;
    /// an unknown phase keeps the value; a builder without phases has no item percent.
    #[test]
    fn phases_advance_the_build() {
        let mut p = Progress::default();
        let item = |p: &Progress| p.detail().building.and_then(|b| b.percent);
        p.line(r#"@nix {"action":"start","id":1,"type":104,"text":"","fields":[]}"#);
        p.line(&build_start(10, "hinteros-daemon-0.7.1"));
        p.line(&builds(0, 2, 1));
        assert_eq!((item(&p), p.percent()), (None, Some(0)), "no phase yet");
        let mut seen = Vec::new();
        for (ph, item_percent, percent) in [
            ("unpackPhase", 0, 0),
            ("patchPhase", 5, 2),
            ("updateAutotoolsGnuConfigScriptsPhase", 8, 4),
            ("configurePhase", 10, 5),
            ("buildPhase", 20, 10),
            ("cargoNextestHook", 20, 10),
            ("checkPhase", 80, 40),
            ("configurePhase", 80, 40),
            ("installPhase", 88, 44),
            ("fixupPhase", 94, 47),
            ("installCheckPhase", 98, 49),
            ("distPhase", 99, 49),
        ] {
            assert!(p.line(&phase(10, ph)), "{ph} is visible");
            seen.push((
                p.detail().building.unwrap().phase.unwrap(),
                item(&p).unwrap(),
                p.percent().unwrap(),
            ));
            assert_eq!(
                (item(&p), p.percent()),
                (Some(item_percent), Some(percent)),
                "{ph}: {seen:?}"
            );
        }
        assert_eq!(
            seen[5].0, "cargoNextestHook",
            "the phase name still changes"
        );
        // Nix counts it done before its activity stops: it does not count twice.
        p.line(&builds(1, 2, 0));
        assert_eq!(p.percent(), Some(50));
        p.line(r#"@nix {"action":"stop","id":10}"#);
        assert_eq!(p.percent(), Some(50));

        // A builder without phases (runCommand) has no item percent, also after an unknown one.
        p.line(&build_start(11, "etc"));
        p.line(&builds(1, 2, 1));
        assert_eq!((item(&p), p.percent()), (None, Some(50)));
        p.line(&phase(11, "customPhase"));
        assert_eq!(item(&p), None, "an unknown first phase gives no percent");
        assert_eq!(
            p.detail().building.unwrap().phase.as_deref(),
            Some("custom")
        );
        p.line(&builds(2, 2, 0));
        p.line(r#"@nix {"action":"stop","id":11}"#);
        assert_eq!(p.percent(), Some(99), "below 100 while nix runs");
    }

    /// Several builds at once: each running build counts by its phase; the one shown is the
    /// most recent; a build that stops before nix counts it is not counted twice either.
    #[test]
    fn several_running_builds() {
        let mut p = Progress::default();
        p.line(r#"@nix {"action":"start","id":1,"type":104,"text":"","fields":[]}"#);
        p.line(&builds(0, 4, 0));
        p.line(&build_start(10, "a-1.0"));
        p.line(&build_start(11, "b-1.0"));
        p.line(&builds(0, 4, 2));
        p.line(&phase(10, "buildPhase"));
        p.line(&phase(11, "checkPhase"));
        let shown = |p: &Progress| {
            let b = p.detail().building.unwrap();
            (b.name, b.percent)
        };
        assert_eq!(shown(&p), ("b".into(), Some(80)));
        // (0 + 0.2 + 0.8) / 4
        assert_eq!(p.percent(), Some(25));
        p.line(&phase(11, "fixupPhase"));
        assert_eq!(p.percent(), Some(28));
        // b is done (nix counts it first): a still counts by its phase.
        p.line(&builds(1, 4, 1));
        assert_eq!(p.percent(), Some(30));
        p.line(r#"@nix {"action":"stop","id":11}"#);
        assert_eq!(p.percent(), Some(30));
        assert_eq!(shown(&p), ("a".into(), Some(20)));
        // A third build stops before nix counts it (the order may differ): it counts as done.
        p.line(&build_start(12, "c-1.0"));
        p.line(&builds(1, 4, 2));
        p.line(&phase(12, "installPhase"));
        assert_eq!(p.percent(), Some(52), "(1 + 0.2 + 0.88) / 4");
        p.line(r#"@nix {"action":"stop","id":12}"#);
        assert_eq!(p.percent(), Some(55), "(1 + 0.2 + 1) / 4");
        p.line(&builds(2, 4, 1));
        assert_eq!(p.percent(), Some(55), "(2 + 0.2) / 4: a still counts");
        assert_eq!(shown(&p), ("a".into(), Some(20)));
    }

    #[test]
    fn plan_messages() {
        assert_eq!(
            plan("this derivation will be built:\n  /nix/store/a.drv"),
            Some(Plan::Builds(1))
        );
        assert_eq!(
            plan("these 12 derivations will be built:\n  /nix/store/a.drv"),
            Some(Plan::Builds(12))
        );
        assert_eq!(
            plan("these 30 paths will be fetched (41.20 MiB download, 180.00 MiB unpacked):"),
            Some(Plan::Paths(30))
        );
        assert_eq!(
            plan("this path will be fetched (0.10 MiB download, 0.50 MiB unpacked):"),
            Some(Plan::Paths(1))
        );
        assert_eq!(plan("evaluating derivation 'path:/etc/hinteros#...'"), None);
        assert_eq!(plan("these many derivations will be built:"), None);
    }

    /// The plan nix prints first gives both totals before any work, so the percent counts
    /// builds from the start; when nix finds more work later the percent holds, then covers
    /// the rest of the way.
    #[test]
    fn percent_from_the_plan_never_goes_back() {
        let mut p = Progress::default();
        let mut seen = Vec::new();
        for l in [
            r#"@nix {"action":"msg","level":3,"msg":"these 2 derivations will be built:\n  /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hinteros-daemon-0.7.1.drv"}"#,
            r#"@nix {"action":"msg","level":3,"msg":"these 4 paths will be fetched (1.00 MiB download, 2.00 MiB unpacked):\n  /nix/store/x"}"#,
            r#"@nix {"action":"start","id":1,"type":103,"text":"","fields":[]}"#,
            r#"@nix {"action":"start","id":2,"type":104,"text":"","fields":[]}"#,
            r#"@nix {"action":"result","id":1,"type":105,"fields":[0,0,0,0]}"#,
            r#"@nix {"action":"result","id":1,"type":105,"fields":[2,4,1,0]}"#,
            r#"@nix {"action":"result","id":1,"type":105,"fields":[4,4,0,0]}"#,
            r#"@nix {"action":"result","id":2,"type":105,"fields":[1,2,1,0]}"#,
            r#"@nix {"action":"result","id":2,"type":105,"fields":[1,4,1,0]}"#,
            r#"@nix {"action":"result","id":2,"type":105,"fields":[3,4,1,0]}"#,
            r#"@nix {"action":"result","id":2,"type":105,"fields":[4,4,0,0]}"#,
        ] {
            p.line(l);
            seen.push(p.percent());
        }
        assert_eq!(p.detail().paths_total, Some(4));
        assert_eq!(
            seen,
            [
                None,
                None,
                None,
                Some(0),
                Some(0),
                Some(25),
                Some(50),
                Some(75),
                Some(75),
                Some(91),
                Some(99)
            ]
        );
        // A download that finished before nix knew of the builds: 99 holds.
        let mut p = Progress::default();
        p.line(r#"@nix {"action":"start","id":1,"type":103,"text":"","fields":[]}"#);
        p.line(r#"@nix {"action":"result","id":1,"type":105,"fields":[4,4,0,0]}"#);
        assert_eq!(p.percent(), Some(99));
        p.line(r#"@nix {"action":"start","id":2,"type":104,"text":"","fields":[]}"#);
        p.line(r#"@nix {"action":"result","id":2,"type":105,"fields":[0,2,1,0]}"#);
        assert_eq!(p.percent(), Some(99));
    }

    /// A real `nix-build --log-format internal-json -v` of a small stdenv derivation (two
    /// builds): nix's expected count goes 1, 2, 3, 4 and back down, and its last progress
    /// still says [2, 3] before the builds activity stops.
    #[test]
    fn real_build_log() {
        let log = include_str!("../tests/fixtures/nix-build-internal-json.log");
        let mut p = Progress::default();
        let mut percents = Vec::new();
        let mut names = Vec::new();
        let mut phases = Vec::new();
        let mut items = Vec::new();
        for l in log.lines() {
            p.line(l);
            percents.extend(p.percent());
            if let Some(b) = p.detail().building {
                if names.last() != Some(&(b.name.clone(), b.version.clone())) {
                    names.push((b.name.clone(), b.version.clone()));
                }
                if let Some(ph) = b.phase
                    && phases.last() != Some(&ph)
                {
                    phases.push(ph);
                }
                if let Some(i) = b.percent
                    && items.last() != Some(&i)
                {
                    items.push(i);
                }
            }
        }
        let d = p.detail();
        assert_eq!((d.builds_done, d.builds_total), (Some(2), Some(2)));
        assert_eq!(d.builds_running, None);
        assert!(d.building.is_none(), "nothing builds at the end");
        assert_eq!(p.stage, Some(Stage::Building));
        assert_eq!(
            names.last(),
            Some(&("hos-progress-probe".to_string(), Some("1.2.3".to_string())))
        );
        assert_eq!(
            names[0].0, "x.c",
            "the source file derivation first: {names:?}"
        );
        for ph in ["patch", "configure", "build", "install", "fixup"] {
            assert!(phases.iter().any(|p| p == ph), "{ph} in {phases:?}");
        }
        assert!(
            percents.windows(2).all(|w| w[0] <= w[1]),
            "never decreases: {percents:?}"
        );
        assert_eq!(percents.last(), Some(&99), "its final value: {percents:?}");
        assert_eq!(p.percent(), Some(99));
        assert!(percents.contains(&50), "1 of 2 built: {percents:?}");
        assert_eq!(items, [5, 8, 10, 20, 88, 94], "the probe's phases");
    }
}
