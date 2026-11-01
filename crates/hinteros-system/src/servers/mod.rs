//! The Server section (M10, `hinteros-docs/design/server.md`): box-wide catalog
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
/// [`is_state_dir`] are left out (the index parser refuses them already).
pub fn delete_targets(
    entry: &Entry,
    server: &crate::config::ServerPaths,
    state_dir: &Path,
) -> Vec<(PathBuf, String)> {
    let mut out: Vec<(PathBuf, String)> = entry
        .state_dirs
        .iter()
        .filter(|d| is_state_dir(d))
        .map(|d| (server.var_lib.clone(), d.clone()))
        .collect();
    if entry.id == jellyfin::ID {
        out.push((server.var_cache.clone(), "jellyfin".into()));
        out.push((state_dir.to_owned(), format!("services/{}", jellyfin::ID)));
    }
    out.dedup();
    out
}

/// A delete target is allowed only below `/var/lib` or `/var/cache` (lexically: the parts are
/// checked with [`is_state_dir`], and the job opens every component without following links).
pub fn delete_allowed(base: &Path, rel: &str, server: &crate::config::ServerPaths) -> bool {
    is_state_dir(rel)
        && (base.starts_with(&server.var_lib) || base == server.var_cache)
        && base.is_absolute()
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
        assert_eq!(unit_for("tailscale"), Some("tailscaled.service"));
        assert_eq!(unit_for("other"), None);
    }
}
