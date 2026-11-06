//! The Server section (M10): box-wide catalog
//! services (`kind = service`) and their setup flows: Jellyfin, Plex, Tailscale, Cloudflare
//! Tunnel. The service talks to them only on loopback or their unix sockets (a short timeout,
//! never blocking the service); everything that waits longer runs in a job.

use std::path::{Path, PathBuf};

use hinteros_api::catalog::{Entry, is_state_dir};

pub mod backup;
pub mod cloudflare;
pub mod http;
pub mod jellyfin;
pub mod media;
pub mod netinfo;
pub mod plex;
pub mod tailscale;

pub const PLEX: &str = "plex-media-server";
pub const TAILSCALE: &str = "tailscale";
pub const CLOUDFLARE: &str = "cloudflare-tunnel";

/// The systemd unit that runs a service (its `status` in `services.list`).
pub fn unit_for(id: &str) -> Option<&'static str> {
    Some(match id {
        jellyfin::ID => "jellyfin.service",
        PLEX => "plex.service",
        TAILSCALE => "tailscaled.service",
        CLOUDFLARE => cloudflare::UNIT,
        _ => return None,
    })
}

/// What `services.disable {delete_data}` removes for an entry: (a base directory, a path below
/// it). The entry's `stateDirs` below `/var/lib`; for Jellyfin also its cache and HinterOS's
/// own files of it (admin password, API key, users). Entries that fail
/// [`is_state_dir`] are left out (the index parser refuses them already), and so are entries
/// that name the box's own state (see [`SYSTEM_STATE`]).
pub fn delete_targets(
    entry: &Entry,
    server: &crate::config::ServerPaths,
    state_dir: &Path,
) -> Vec<(PathBuf, String)> {
    let mut out: Vec<(PathBuf, String)> = entry
        .state_dirs
        .iter()
        .filter(|d| is_state_dir(d) && not_system_state(d, Some(&entry.id)))
        .map(|d| (server.var_lib.clone(), d.clone()))
        .collect();
    if entry.id == jellyfin::ID {
        out.push((server.var_cache.clone(), "jellyfin".into()));
        out.push((state_dir.to_owned(), format!("services/{}", jellyfin::ID)));
    }
    out.dedup();
    out
}

/// `/var/lib` directories that hold the box's own state (`profiles.json`, root's staging, the
/// services' secrets, ...): no delete target may name them, or a folder above them. HinterOS's
/// own directory only as `hinteros/services/<entry>` or below. The catalog's validator refuses
/// the same `stateDirs`.
const SYSTEM_STATE: &[&str] = &["hinteros", "hinteros-root", "nixos", "systemd", "private"];

/// `rel` (below `/var/lib`) leaves the box's own state alone; with `entry`, only that entry's
/// `hinteros/services/<entry>` is allowed.
fn not_system_state(rel: &str, entry: Option<&str>) -> bool {
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [first, ..] if !SYSTEM_STATE.contains(first) => true,
        ["hinteros", "services", id, ..] => entry.is_none_or(|e| e == *id),
        _ => false,
    }
}

/// A delete target is allowed only below `/var/lib` or `/var/cache` (lexically: the parts are
/// checked with [`is_state_dir`], and the job opens every component without following links),
/// and never on the box's own state below `/var/lib` ([`SYSTEM_STATE`]).
pub fn delete_allowed(base: &Path, rel: &str, server: &crate::config::ServerPaths) -> bool {
    if !is_state_dir(rel) || !base.is_absolute() {
        return false;
    }
    if base == server.var_cache {
        return true;
    }
    match base
        .strip_prefix(&server.var_lib)
        .ok()
        .and_then(Path::to_str)
    {
        Some(above) => not_system_state(&format!("{above}/{rel}"), None),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerPaths;

    #[test]
    fn delete_targets_stay_in_var() {
        let index = hinteros_api::catalog::Index::parse(
            br#"{"indexVersion":4,"entries":[
              {"id":"jellyfin-server","kind":"service","name":"J","status":"available","stateDirs":["jellyfin"]},
              {"id":"cloudflare-tunnel","kind":"service","name":"C","status":"available","stateDirs":["hinteros/services/cloudflare-tunnel"]}]}"#,
            "/".into(),
        )
        .unwrap();
        let s = ServerPaths::default();
        let state = Path::new("/var/lib/hinteros");
        let j = delete_targets(index.get("jellyfin-server").unwrap(), &s, state);
        assert_eq!(
            j,
            [
                (PathBuf::from("/var/lib"), "jellyfin".to_owned()),
                (PathBuf::from("/var/cache"), "jellyfin".to_owned()),
                (
                    PathBuf::from("/var/lib/hinteros"),
                    "services/jellyfin-server".to_owned()
                ),
            ]
        );
        for (base, rel) in &j {
            assert!(delete_allowed(base, rel, &s));
        }
        let c = delete_targets(index.get("cloudflare-tunnel").unwrap(), &s, state);
        assert_eq!(
            c,
            [(
                PathBuf::from("/var/lib"),
                "hinteros/services/cloudflare-tunnel".to_owned()
            )]
        );
        assert!(!delete_allowed(Path::new("/etc"), "x", &s));
        assert!(!delete_allowed(Path::new("/var/lib"), "../etc", &s));
        assert!(!delete_allowed(Path::new("/var/cache/x"), "y", &s));
        assert!(!delete_allowed(Path::new("var/lib"), "y", &s));
        // Never the box's own state.
        for (base, rel) in [
            ("/var/lib", "hinteros"),
            ("/var/lib", "hinteros/services"),
            ("/var/lib", "hinteros/profiles.json"),
            ("/var/lib/hinteros", "services"),
            ("/var/lib/hinteros", "state.json"),
            ("/var/lib", "nixos"),
            ("/var/lib", "systemd/timers"),
            ("/var/lib", "private"),
        ] {
            assert!(!delete_allowed(Path::new(base), rel, &s), "{base} {rel}");
        }
        assert!(delete_allowed(Path::new("/var/lib"), "hinteros-thing", &s));
        let bad = hinteros_api::catalog::Index::parse(
            br#"{"indexVersion":4,"entries":[
              {"id":"x","kind":"service","name":"X","status":"available",
               "stateDirs":["hinteros","hinteros/services/other","hinteros/services/x/cache","nixos","x"]}]}"#,
            "/".into(),
        )
        .unwrap();
        assert_eq!(
            delete_targets(bad.get("x").unwrap(), &s, state),
            [
                (
                    PathBuf::from("/var/lib"),
                    "hinteros/services/x/cache".to_owned()
                ),
                (PathBuf::from("/var/lib"), "x".to_owned()),
            ]
        );
        assert_eq!(unit_for("tailscale"), Some("tailscaled.service"));
        assert_eq!(unit_for("other"), None);
    }
}
