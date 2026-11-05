//! The box's time zone (time-zone.md): zone names, offsets from the system's
//! tzdata, the zone list and the localtime link.
//!
//! The service never writes `/etc` itself: `time_zone.set` leaves the zone in
//! `<run-dir>/time-zone.request` and starts `hinteros-time-zone-apply.service`, which runs
//! `hinteros-system time-zone-apply` ([`apply_main`]).
//!
//! The link the daemon replaces is [`LOCALTIME_LINK`], in a directory of its own, not
//! `/etc/localtime` itself: the system repo makes `/etc/localtime` a fixed link to it, so the
//! helper unit can write that directory and `/etc/hinteros` only, not all of `/etc`.
//!
//! Offsets come from the TZif files of the zoneinfo directory (`/etc/zoneinfo`, the same files
//! `/etc/localtime` points into), never from a database built into the daemon, so the daemon
//! and the C library always agree.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use hinteros_api::system::{DEFAULT_TIME_ZONE, TimeZoneEntry, TimeZoneInfo, is_zone_name};
use tracing::{debug, warn};
use tz::TimeZone;
use tz::timezone::TransitionRule;

use crate::config::Paths;
use crate::state::State;

/// The link the daemon points at `<zoneinfo>/<zone>` (`--localtime`). `/etc/localtime` is a
/// fixed link to it (the system repo's activation makes both).
pub const LOCALTIME_LINK: &str = "/etc/hinteros-time-zone/localtime";

/// Largest TZif file read (the biggest in tzdata is a few KiB).
const MAX_TZIF_BYTES: u64 = 256 * 1024;

/// How far past the last explicit transition the zone's rule is searched for the next change
/// (a rule changes the offset at least once a year when it changes it at all).
const RULE_HORIZON_S: i64 = 400 * 86_400;

#[derive(Debug, thiserror::Error)]
pub enum ZoneError {
    #[error("{0:?} is not a time zone name")]
    BadFormat(String),
    #[error("tzdata has no time zone {0:?}")]
    Unknown(String),
    #[error("reading time zone {0:?}: {1}")]
    Io(String, io::Error),
}

/// The TZif file of `zone` under `dir` (the name checked first: no absolute path, no `..`).
pub fn zone_file(dir: &Path, zone: &str) -> Result<PathBuf, ZoneError> {
    if !is_zone_name(zone) {
        return Err(ZoneError::BadFormat(zone.to_owned()));
    }
    Ok(dir.join(zone))
}

/// Load `zone` from the zoneinfo directory: an existing regular file (not a link, as the
/// system repo's check) that is TZif. `UTC` is always valid, also without a file.
pub fn load(dir: &Path, zone: &str) -> Result<TimeZone, ZoneError> {
    let path = zone_file(dir, zone)?;
    let unknown = || ZoneError::Unknown(zone.to_owned());
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound && zone == DEFAULT_TIME_ZONE => {
            return Ok(TimeZone::utc());
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Err(unknown());
        }
        Err(e) => return Err(ZoneError::Io(zone.to_owned(), e)),
    };
    if !meta.is_file() {
        return Err(unknown());
    }
    let mut bytes = Vec::new();
    fs::File::open(&path)
        .and_then(|f| f.take(MAX_TZIF_BYTES).read_to_end(&mut bytes))
        .map_err(|e| ZoneError::Io(zone.to_owned(), e))?;
    if !bytes.starts_with(b"TZif") {
        return Err(unknown());
    }
    TimeZone::from_tz_data(&bytes).map_err(|e| {
        warn!(zone, path = %path.display(), "unreadable TZif file: {e}");
        unknown()
    })
}

/// The offset (seconds east of UTC) and DST flag at `t`. Past the last transition of a file
/// without a rule (no TZif footer), the last transition's type stays, as in the C library.
fn type_at(tz: &TimeZone, t: i64) -> (i32, bool) {
    let r = tz.as_ref();
    match r.find_local_time_type(t) {
        Ok(l) => (l.ut_offset(), l.is_dst()),
        Err(_) => {
            let types = r.local_time_types();
            let l = r
                .transitions()
                .last()
                .map_or(&types[0], |tr| &types[tr.local_time_type_index()]);
            (l.ut_offset(), l.is_dst())
        }
    }
}

/// The first time after `now` at which the offset differs from the offset at `now`.
pub fn next_change(tz: &TimeZone, now: i64) -> Option<i64> {
    let r = tz.as_ref();
    let (offset, _) = type_at(tz, now);
    let types = r.local_time_types();
    // The explicit transitions (tzdata's files list them to 2037).
    for tr in r
        .transitions()
        .iter()
        .filter(|tr| tr.unix_leap_time() > now)
    {
        if types[tr.local_time_type_index()].ut_offset() != offset {
            return Some(tr.unix_leap_time());
        }
    }
    // Then the rule (the TZif footer).
    let Some(TransitionRule::Alternate(rule)) = r.extra_rule() else {
        return None;
    };
    if rule.std().ut_offset() == rule.dst().ut_offset() {
        return None;
    }
    let start = r
        .transitions()
        .last()
        .map_or(now, |tr| tr.unix_leap_time().max(now));
    // A day at a time, then to the second.
    let mut before = start;
    let mut t = start;
    while t - start <= RULE_HORIZON_S {
        t += 86_400;
        if type_at(tz, t).0 != offset {
            let (mut lo, mut hi) = (before, t);
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                if type_at(tz, mid).0 == offset {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            return Some(hi);
        }
        before = t;
    }
    None
}

/// Offsets of a zone at `now`: (offset, standard offset, next change).
///
/// The standard offset is the lower of the zone's two offsets while it observes DST (the next
/// change goes to the other kind, within the year): normally the non-DST one, and for tzdata's
/// negative DST (Europe/Dublin: IST is standard, winter GMT is flagged DST) the winter offset,
/// which is what the map's bands use. Without DST it is the current offset.
pub fn offsets(tz: &TimeZone, now: i64) -> (i32, i32, Option<i64>) {
    let (offset, dst) = type_at(tz, now);
    let next = next_change(tz, now);
    let standard = match next {
        Some(t) if t - now <= RULE_HORIZON_S => {
            let (other, other_dst) = type_at(tz, t);
            if other_dst != dst {
                offset.min(other)
            } else {
                offset
            }
        }
        _ => offset,
    };
    (offset, standard, next)
}

/// `zone`'s info at `now`.
pub fn info(dir: &Path, zone: &str, now: i64) -> Result<TimeZoneInfo, ZoneError> {
    let tz = load(dir, zone)?;
    let (offset_s, standard_offset_s, offset_valid_until) = offsets(&tz, now);
    Ok(TimeZoneInfo {
        zone: zone.to_owned(),
        offset_s,
        standard_offset_s,
        offset_valid_until,
    })
}

/// UTC's info (also what applies when the named zone cannot be read).
pub fn utc_info() -> TimeZoneInfo {
    TimeZoneInfo {
        zone: DEFAULT_TIME_ZONE.into(),
        offset_s: 0,
        standard_offset_s: 0,
        offset_valid_until: None,
    }
}

/// The zone that applies for `state`: the one it names if tzdata has it, else UTC.
pub fn effective(dir: &Path, state: &State, now: i64) -> TimeZoneInfo {
    let zone = state.time_zone();
    match info(dir, zone, now) {
        Ok(i) => i,
        Err(e) => {
            if zone != DEFAULT_TIME_ZONE {
                warn!(
                    zone,
                    "state.json names a time zone that cannot be used ({e}); UTC applies"
                );
            }
            info(dir, DEFAULT_TIME_ZONE, now).unwrap_or_else(|_| utc_info())
        }
    }
}

/// The zone names of `zone1970.tab` (third column) plus `UTC`, sorted.
pub fn names(dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::from([DEFAULT_TIME_ZONE.to_owned()]);
    let tab = dir.join("zone1970.tab");
    match fs::read_to_string(&tab) {
        Ok(text) => {
            for line in text.lines().filter(|l| !l.starts_with('#')) {
                match line.split('\t').nth(2) {
                    Some(z) if is_zone_name(z) => {
                        out.insert(z.to_owned());
                    }
                    _ if line.trim().is_empty() => {}
                    _ => debug!(line, "zone1970.tab: line skipped"),
                }
            }
        }
        Err(e) => warn!(path = %tab.display(), "no zone list ({e}); only UTC is offered"),
    }
    out
}

/// Every zone the picker offers with its offsets at `now`, sorted by name. A listed zone
/// without a usable file is left out.
pub fn list(dir: &Path, now: i64) -> Vec<TimeZoneEntry> {
    names(dir)
        .into_iter()
        .filter_map(|zone| match info(dir, &zone, now) {
            Ok(i) => Some(TimeZoneEntry {
                zone: i.zone,
                offset_s: i.offset_s,
                standard_offset_s: i.standard_offset_s,
            }),
            Err(e) => {
                warn!(zone, "zone1970.tab names a zone that cannot be used: {e}");
                None
            }
        })
        .collect()
}

/// Point the `localtime` link at `<zoneinfo>/<zone>`: a new link next to it, renamed over the
/// old one, so readers see the old zone or the new one, never none.
pub fn set_localtime(localtime: &Path, zoneinfo: &Path, zone: &str) -> io::Result<()> {
    let target = zone_file(zoneinfo, zone).map_err(io::Error::other)?;
    let dir = localtime.parent().unwrap_or(Path::new("/"));
    let name = localtime.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    symlink(&target, &tmp)?;
    if let Err(e) = fs::rename(&tmp, localtime) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::File::open(dir).and_then(|d| d.sync_all())
}

/// The request `time_zone.set` leaves for the helper: one line, the zone name.
pub const REQUEST_FILE: &str = "time-zone.request";

/// The helper's unit (oneshot, root; the system repo defines it).
pub const APPLY_UNIT: &str = "hinteros-time-zone-apply.service";

/// Longest request file read.
const MAX_REQUEST_BYTES: u64 = 256;

/// `<run-dir>/time-zone.request`.
pub fn request_path(paths: &Paths) -> PathBuf {
    paths.run_dir.join(REQUEST_FILE)
}

/// Leave a request for the helper (atomically, 0600).
pub fn write_request(paths: &Paths, zone: &str) -> io::Result<()> {
    crate::fsutil::write_atomic(&request_path(paths), format!("{zone}\n").as_bytes(), 0o600)
}

/// Read the request: one line, a zone name of the right shape.
fn read_request(paths: &Paths) -> anyhow::Result<String> {
    let path = request_path(paths);
    let mut text = String::new();
    fs::File::open(&path)
        .and_then(|f| f.take(MAX_REQUEST_BYTES).read_to_string(&mut text))
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let zone = text.strip_suffix('\n').unwrap_or(&text);
    if !is_zone_name(zone) {
        anyhow::bail!("{} does not hold a time zone name", path.display());
    }
    Ok(zone.to_owned())
}

/// Under the `state.json` lock: write `settings.timeZone` (when it changes) and point the
/// localtime link at `zone` (a TZif file of the zoneinfo dir, checked again here). On a failure
/// `state.json` is left (or put back) as it was. Returns the zone that applied before.
pub fn apply_zone(paths: &Paths, zone: &str) -> anyhow::Result<String> {
    load(&paths.zoneinfo_dir, zone)?;
    let _lock = crate::state::lock(&paths.state_lock())?;
    let path = paths.state_json();
    let old = fs::read(&path)?;
    let mut st = State::parse(&old)?;
    let before = effective(&paths.zoneinfo_dir, &st, crate::job::now()).zone;
    let changed = st.time_zone() != zone;
    if changed {
        st.settings.time_zone = Some(zone.to_owned());
        crate::fsutil::write_atomic(&path, &st.to_canonical_json(), 0o644)?;
    }
    if let Err(e) = set_localtime(&paths.localtime, &paths.zoneinfo_dir, zone) {
        if changed && let Err(e2) = crate::fsutil::write_atomic(&path, &old, 0o644) {
            warn!("cannot put state.json back: {e2}");
        }
        return Err(anyhow::anyhow!(
            "replacing {}: {e}",
            paths.localtime.display()
        ));
    }
    Ok(before)
}

/// The request applied: zone checked, `state.json`, the link, the request file removed.
pub fn apply_request(paths: &Paths) -> anyhow::Result<(String, String)> {
    let zone = read_request(paths)?;
    let before = apply_zone(paths, &zone)?;
    let _ = fs::remove_file(request_path(paths));
    Ok((before, zone))
}

/// `hinteros-system time-zone-apply` (`hinteros-time-zone-apply.service`).
pub fn apply_main(paths: &Paths) -> std::process::ExitCode {
    match apply_request(paths) {
        Ok((before, zone)) => {
            tracing::info!(from = %before, to = %zone, "time zone applied");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!("time zone not applied: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Point `/etc/localtime` at the zone `state.json` names now (UTC when tzdata lacks it):
/// after a rollback restored another `state.json` (the switch ran before the restore). Returns
/// the zone applied.
pub fn apply_state(paths: &Paths) -> io::Result<String> {
    let (state, _) = State::load(&paths.state_json()).map_err(io::Error::other)?;
    let zone = effective(&paths.zoneinfo_dir, &state, crate::job::now()).zone;
    set_localtime(&paths.localtime, &paths.zoneinfo_dir, &zone)?;
    Ok(zone)
}

/// Before a job replaces `/etc/hinteros` with its staged copy: keep the live time zone (set in
/// place by `time_zone.set` while the job ran, or after the job's state was computed). Holds no
/// lock; the caller holds [`crate::state::lock`].
pub fn carry_live_zone(live: &Path, staged: &Path) -> io::Result<bool> {
    let (live, _) = State::load(live).map_err(io::Error::other)?;
    let (mut staged_state, _) = State::load(staged).map_err(io::Error::other)?;
    if staged_state.settings.time_zone == live.settings.time_zone {
        return Ok(false);
    }
    staged_state.settings.time_zone = live.settings.time_zone;
    crate::fsutil::write_atomic(staged, &staged_state.to_canonical_json(), 0o644)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use super::*;

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/zoneinfo")
    }

    /// 2026-10-14T12:00:00Z.
    const OCT_2026: i64 = 1_791_979_200;
    /// 2026-01-15T12:00:00Z.
    const JAN_2026: i64 = 1_768_478_400;

    fn at(zone: &str, now: i64) -> TimeZoneInfo {
        info(&fixtures(), zone, now).unwrap()
    }

    #[test]
    fn chicago() {
        // October: CDT (-5), standard CST (-6), back to CST on 2026-11-01 07:00 UTC.
        let i = at("America/Chicago", OCT_2026);
        assert_eq!(i.offset_s, -5 * 3600);
        assert_eq!(i.standard_offset_s, -6 * 3600);
        assert_eq!(i.offset_valid_until, Some(1_793_516_400));
        // January: CST, DST starts 2026-03-08 08:00 UTC.
        let i = at("America/Chicago", JAN_2026);
        assert_eq!(i.offset_s, -6 * 3600);
        assert_eq!(i.standard_offset_s, -6 * 3600);
        assert_eq!(i.offset_valid_until, Some(1_772_956_800));
        // The change itself: one second before is still the old offset.
        let i = at("America/Chicago", 1_793_516_399);
        assert_eq!(i.offset_s, -5 * 3600);
        assert_eq!(i.offset_valid_until, Some(1_793_516_400));
        let i = at("America/Chicago", 1_793_516_400);
        assert_eq!(i.offset_s, -6 * 3600);
    }

    #[test]
    fn past_the_explicit_transitions_the_rule_applies() {
        // 2040-07-01T00:00:00Z: past tzdata's last listed transition (2037), from the footer.
        let i = at("America/Chicago", 2_224_713_600);
        assert_eq!(i.offset_s, -5 * 3600);
        assert_eq!(i.standard_offset_s, -6 * 3600);
        // 2040-11-04 07:00 UTC (first Sunday of November).
        assert_eq!(i.offset_valid_until, Some(2_235_625_200));
        let i = at("Europe/London", 2_224_713_600);
        assert_eq!(i.offset_s, 3600);
        assert_eq!(i.standard_offset_s, 0);
        // 2040-10-28 01:00 UTC (last Sunday of October).
        assert_eq!(i.offset_valid_until, Some(2_234_998_800));
    }

    #[test]
    fn no_dst() {
        for (zone, offset) in [("Asia/Kolkata", 19_800), ("Asia/Tokyo", 9 * 3600)] {
            let i = at(zone, OCT_2026);
            assert_eq!(i.offset_s, offset, "{zone}");
            assert_eq!(i.standard_offset_s, offset, "{zone}");
            assert_eq!(i.offset_valid_until, None, "{zone}");
        }
        // Brazil stopped DST in 2019: no further change.
        let i = at("America/Sao_Paulo", OCT_2026);
        assert_eq!(i.offset_s, -3 * 3600);
        assert_eq!(i.standard_offset_s, -3 * 3600);
        assert_eq!(i.offset_valid_until, None);
        assert_eq!(at("UTC", OCT_2026), utc_info());
    }

    #[test]
    fn negative_dst_gives_the_winter_offset() {
        // tzdata's Europe/Dublin: IST (+1) is standard, winter GMT is flagged DST.
        let summer = at("Europe/Dublin", 1_783_000_000);
        assert_eq!(summer.offset_s, 3600);
        assert_eq!(summer.standard_offset_s, 0);
        let winter = at("Europe/Dublin", JAN_2026);
        assert_eq!(winter.offset_s, 0);
        assert_eq!(winter.standard_offset_s, 0);
        assert!(winter.offset_valid_until.is_some());
    }

    #[test]
    fn half_hour_dst() {
        // Lord Howe: +10:30 standard, +11 in (southern) summer.
        let i = at("Australia/Lord_Howe", JAN_2026);
        assert_eq!(i.offset_s, 11 * 3600);
        assert_eq!(i.standard_offset_s, 10 * 3600 + 1800);
        let i = at("Australia/Lord_Howe", 1_783_000_000);
        assert_eq!(i.offset_s, 10 * 3600 + 1800);
        assert_eq!(i.standard_offset_s, 10 * 3600 + 1800);
    }

    #[test]
    fn names_are_checked_before_any_file_is_opened() {
        let d = fixtures();
        for bad in [
            "../zoneinfo/UTC",
            "/etc/localtime",
            "America/../UTC",
            "",
            "a b",
            "zone1970.tab",
            "posixrules",
        ] {
            assert!(
                matches!(load(&d, bad), Err(ZoneError::BadFormat(_))),
                "{bad:?}"
            );
        }
        for unknown in ["Mars/Olympus_Mons", "America", "America/Chicago/X"] {
            assert!(
                matches!(load(&d, unknown), Err(ZoneError::Unknown(_))),
                "{unknown:?}"
            );
        }
    }

    #[test]
    fn utc_needs_no_file() {
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(info(empty.path(), "UTC", OCT_2026).unwrap(), utc_info());
        assert!(matches!(
            load(empty.path(), "Europe/London"),
            Err(ZoneError::Unknown(_))
        ));
        // No zone1970.tab: only UTC.
        assert_eq!(list(empty.path(), OCT_2026).len(), 1);
    }

    #[test]
    fn list_is_the_tab_plus_utc_sorted() {
        let l = list(&fixtures(), OCT_2026);
        let zones: Vec<&str> = l.iter().map(|e| e.zone.as_str()).collect();
        // The fixture tab also names a zone without a file (left out).
        assert_eq!(
            zones,
            [
                "America/Chicago",
                "America/Sao_Paulo",
                "Asia/Kolkata",
                "Asia/Tokyo",
                "Australia/Lord_Howe",
                "Europe/Dublin",
                "Europe/London",
                "UTC"
            ]
        );
        let chicago = &l[0];
        assert_eq!(
            (chicago.offset_s, chicago.standard_offset_s),
            (-5 * 3600, -6 * 3600)
        );
    }

    #[test]
    fn a_file_that_is_not_tzif_is_unknown() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("Etc")).unwrap();
        fs::write(d.path().join("Etc/Bad"), "not a zone").unwrap();
        fs::write(d.path().join("Etc/Truncated"), "TZif2").unwrap();
        symlink(fixtures().join("UTC"), d.path().join("Etc/Link")).unwrap();
        for z in ["Etc/Bad", "Etc/Truncated", "Etc", "Etc/Link"] {
            assert!(
                matches!(load(d.path(), z), Err(ZoneError::Unknown(_))),
                "{z}"
            );
        }
    }

    #[test]
    fn localtime_link_is_replaced() {
        let d = tempfile::tempdir().unwrap();
        let lt = d.path().join("localtime");
        set_localtime(&lt, &fixtures(), "Europe/London").unwrap();
        assert_eq!(
            fs::read_link(&lt).unwrap(),
            fixtures().join("Europe/London")
        );
        set_localtime(&lt, &fixtures(), "UTC").unwrap();
        assert_eq!(fs::read_link(&lt).unwrap(), fixtures().join("UTC"));
        // A regular file there (an image's copy) is replaced too.
        fs::remove_file(&lt).unwrap();
        fs::write(&lt, "x").unwrap();
        set_localtime(&lt, &fixtures(), "Asia/Tokyo").unwrap();
        assert_eq!(fs::read_link(&lt).unwrap(), fixtures().join("Asia/Tokyo"));
        let left: Vec<_> = fs::read_dir(d.path()).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporary link left");
        assert!(set_localtime(&lt, &fixtures(), "../x").is_err());
    }

    #[test]
    fn the_link_is_in_a_directory_of_its_own() {
        // The helper unit may write only that directory and /etc/hinteros (not /etc), so
        // neither the service nor the helper defaults to /etc/localtime.
        use clap::Parser;
        let link = Path::new(LOCALTIME_LINK);
        assert_eq!(link.parent(), Some(Path::new("/etc/hinteros-time-zone")));
        let cli =
            crate::config::Cli::try_parse_from(["hinteros-system", "time-zone-apply"]).unwrap();
        let Some(crate::config::Command::TimeZoneApply { localtime, .. }) = cli.command else {
            panic!("not time-zone-apply");
        };
        assert_eq!(localtime, link);
        let cli = crate::config::Cli::try_parse_from(["hinteros-system"]).unwrap();
        assert_eq!(cli.serve.localtime, link);
        assert_eq!(crate::config::default_localtime(), link);
    }

    fn helper_paths(d: &Path) -> Paths {
        let mut p = crate::pam::paths(
            d.join("var"),
            d.join("run"),
            d.join("etc"),
            Default::default(),
        );
        p.zoneinfo_dir = fixtures();
        p.localtime = d.join("localtime");
        fs::create_dir_all(d.join("etc")).unwrap();
        fs::create_dir_all(d.join("run")).unwrap();
        fs::write(
            d.join("etc/state.json"),
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[],"profiles":[{"id":"p1"}],"settings":{"ssh":{"enable":true}}}"#,
        )
        .unwrap();
        p
    }

    #[test]
    fn helper_applies_a_request() {
        let d = tempfile::tempdir().unwrap();
        let p = helper_paths(d.path());
        write_request(&p, "Europe/London").unwrap();
        assert_eq!(
            fs::metadata(request_path(&p)).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let (before, zone) = apply_request(&p).unwrap();
        assert_eq!((before.as_str(), zone.as_str()), ("UTC", "Europe/London"));
        let (s, _) = State::load(&p.state_json()).unwrap();
        assert_eq!(s.time_zone(), "Europe/London");
        assert!(s.settings.ssh.enable, "the rest stays");
        assert_eq!(
            fs::read_link(&p.localtime).unwrap(),
            fixtures().join("Europe/London")
        );
        assert!(!request_path(&p).exists(), "request removed");

        // The same zone again: state.json is not rewritten, the link is made again.
        let bytes = fs::read(p.state_json()).unwrap();
        let ino = fs::metadata(p.state_json()).unwrap().ino();
        fs::remove_file(&p.localtime).unwrap();
        write_request(&p, "Europe/London").unwrap();
        assert_eq!(apply_request(&p).unwrap().0, "Europe/London");
        assert_eq!(fs::read(p.state_json()).unwrap(), bytes);
        assert_eq!(
            fs::metadata(p.state_json()).unwrap().ino(),
            ino,
            "not rewritten"
        );
        assert_eq!(
            fs::read_link(&p.localtime).unwrap(),
            fixtures().join("Europe/London")
        );
    }

    #[test]
    fn helper_refuses_bad_requests_and_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let mut p = helper_paths(d.path());
        let bytes = fs::read(p.state_json()).unwrap();
        // No request.
        assert!(apply_request(&p).is_err());
        // Bad names, unknown zones, two lines.
        for bad in [
            "../../etc/shadow",
            "/etc/passwd",
            "posixrules",
            "Mars/Olympus_Mons",
            "Europe/Paris",
            "UTC\nAsia/Tokyo",
            "",
        ] {
            fs::write(request_path(&p), format!("{bad}\n")).unwrap();
            assert!(apply_request(&p).is_err(), "{bad:?}");
            assert_eq!(fs::read(p.state_json()).unwrap(), bytes, "{bad:?}");
            assert!(!p.localtime.exists(), "{bad:?}");
        }
        // The link cannot be made (its directory is missing): state.json is put back.
        p.localtime = d.path().join("missing/localtime");
        write_request(&p, "Asia/Tokyo").unwrap();
        assert!(apply_request(&p).is_err());
        assert_eq!(fs::read(p.state_json()).unwrap(), bytes);
        assert!(request_path(&p).exists(), "kept for a look");
    }

    #[test]
    fn the_live_zone_survives_a_job() {
        let d = tempfile::tempdir().unwrap();
        let base = r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[],"profiles":[{"id":"p1"}],"settings":{"ssh":{"enable":true}}}"#;
        let live = d.path().join("live.json");
        let staged = d.path().join("staged.json");
        let with_zone = r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[],"profiles":[{"id":"p1"}],"settings":{"ssh":{"enable":true},"timeZone":"Asia/Tokyo"}}"#;
        fs::write(&live, with_zone).unwrap();
        fs::write(&staged, base).unwrap();
        assert!(carry_live_zone(&live, &staged).unwrap());
        let (s, _) = State::load(&staged).unwrap();
        assert_eq!(s.time_zone(), "Asia/Tokyo");
        assert!(s.settings.ssh.enable, "the rest of the staged state stays");
        assert!(
            !carry_live_zone(&live, &staged).unwrap(),
            "already the same"
        );
    }
}
