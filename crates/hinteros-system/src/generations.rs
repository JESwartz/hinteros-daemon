//! System generations: the `<profile>-<N>-link` symlinks next to the system profile.

use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use hinteros_api::system::{Generation, GenerationProfile};

use crate::flake;
use crate::profiles::ProfilesFile;
use crate::state::State;

/// One generation link and the toplevel it points to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub generation: u32,
    pub link: PathBuf,
    pub toplevel: Option<PathBuf>,
    pub date: i64,
}

/// All generations of `profile`, newest first.
pub fn links(profile: &Path) -> Vec<Link> {
    let (Some(dir), Some(name)) = (profile.parent(), profile.file_name()) else {
        return Vec::new();
    };
    let prefix = format!("{}-", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Link> = entries
        .flatten()
        .filter_map(|e| {
            let file = e.file_name();
            let n = file
                .to_str()?
                .strip_prefix(&prefix)?
                .strip_suffix("-link")?
                .parse::<u32>()
                .ok()
                .filter(|n| *n > 0)?;
            let link = e.path();
            let date = std::fs::symlink_metadata(&link).ok()?.mtime();
            Some(Link {
                generation: n,
                toplevel: std::fs::canonicalize(&link).ok(),
                link,
                date,
            })
        })
        .collect();
    out.sort_by_key(|l| std::cmp::Reverse(l.generation));
    out
}

/// The systems systemd-boot has an entry for: `init=<toplevel>/init` in `<entries>/nixos-*.conf`.
/// The boot loader writes entries for the newest `configurationLimit` generations only, and the
/// system repo fails the switch to one without (a rollback past the limit). `None` when there
/// are no entries or one cannot be read (another boot loader, no ESP): nothing is known.
pub fn boot_toplevels(entries: &Path) -> Option<BTreeSet<PathBuf>> {
    let mut found = false;
    let mut out = BTreeSet::new();
    for e in std::fs::read_dir(entries).ok()?.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with("nixos-") && name.ends_with(".conf")) {
            continue;
        }
        found = true;
        let text = std::fs::read_to_string(e.path()).ok()?;
        out.extend(
            text.lines()
                .filter_map(|l| l.trim_start().strip_prefix("options"))
                .flat_map(str::split_whitespace)
                .filter_map(|w| w.strip_prefix("init=")?.strip_suffix("/init"))
                .map(PathBuf::from),
        );
    }
    found.then_some(out)
}

/// The index's name in the run dir.
pub const BOOT_INDEX: &str = "boot-entries.json";

/// [`boot_toplevels`], or where the entries cannot be read (the ESP is root's only, and the
/// service is not root) what `hinteros-system boot-entries` last wrote to `index`. Neither:
/// `None` (nothing known).
pub fn bootable(entries: &Path, index: &Path) -> Option<BTreeSet<PathBuf>> {
    match std::fs::read_dir(entries) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            crate::fsutil::read_cache::<Option<BTreeSet<PathBuf>>>(index).flatten()
        }
        _ => boot_toplevels(entries),
    }
}

/// `hinteros-system boot-entries` (the `hinteros-boot-entries` unit, run when the entries
/// change): write [`boot_toplevels`] to `index` for the service.
pub fn index_main(entries: &Path, index: &Path) -> std::process::ExitCode {
    let found = boot_toplevels(entries);
    let written = crate::fsutil::cache_bytes(&found)
        .map_err(std::io::Error::other)
        .and_then(|b| crate::fsutil::write_atomic(index, &b, 0o644));
    match written {
        Ok(()) => {
            tracing::info!(
                entries = found.as_ref().map(BTreeSet::len),
                "boot entries indexed"
            );
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!("cannot write {}: {e}", index.display());
            std::process::ExitCode::FAILURE
        }
    }
}

/// The generation the profile points at.
pub fn current_generation(profile: &Path) -> Option<u32> {
    let target = std::fs::read_link(profile).ok()?;
    let name = target.file_name()?.to_str()?.to_owned();
    let prefix = format!("{}-", profile.file_name()?.to_str()?);
    name.strip_prefix(&prefix)?
        .strip_suffix("-link")?
        .parse()
        .ok()
}

/// `generations.list`. `profiles_json` gives the profiles' current names; `needed`, the box
/// features the profiles' apps need now, gives `readds_features`. A generation without a boot
/// entry (`boot_toplevels`) is left out unless it is the current one: a rollback to it fails.
pub fn list(
    profile: &Path,
    current_system: &Path,
    booted_system: &Path,
    boot_entries: &Path,
    boot_index: &Path,
    profiles_json: &Path,
    needed: Option<&BTreeSet<String>>,
) -> Vec<Generation> {
    let current = std::fs::canonicalize(current_system).ok();
    let booted = std::fs::canonicalize(booted_system).ok();
    let bootable = bootable(boot_entries, boot_index);
    let names = ProfilesFile::load(profiles_json).unwrap_or_default();
    links(profile)
        .into_iter()
        .filter(|l| match (&bootable, &l.toplevel) {
            (Some(b), Some(t)) => b.contains(t) || Some(t) == current.as_ref(),
            _ => true,
        })
        .map(|l| {
            let rev = l.toplevel.as_ref().and_then(|t| {
                flake::read_lock(&t.join("hinteros/flake.lock"))
                    .ok()
                    .and_then(|lock| lock.rev)
            });
            let state = l
                .toplevel
                .as_ref()
                .and_then(|t| State::load(&t.join("hinteros/state.json")).ok())
                .map(|(s, _)| s);
            let readds_features = needed
                .zip(state.as_ref())
                .map(|(n, s)| {
                    n.iter()
                        .filter(|f| !s.requirements.contains(f))
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .filter(|m| !m.is_empty());
            Generation {
                generation: l.generation,
                date: l.date,
                rev,
                current: l.toplevel.is_some() && l.toplevel == current,
                booted: l.toplevel.is_some() && l.toplevel == booted,
                readds_features,
                profiles: state.map(|state| {
                    state
                        .accounts()
                        .into_iter()
                        .map(|a| GenerationProfile {
                            name: names.profiles.get(&a.id).map(|m| m.name.clone()),
                            id: a.id,
                        })
                        .collect()
                }),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn lists_generations() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("profiles")).unwrap();
        for n in [1, 2, 10] {
            std::fs::create_dir_all(d.join(format!("store/gen{n}/hinteros"))).unwrap();
            let link = d.join(format!("profiles/system-{n}-link"));
            symlink(d.join(format!("store/gen{n}")), link).unwrap();
        }
        std::fs::write(d.join("profiles/system-x-link"), "").unwrap();
        symlink("system-2-link", d.join("profiles/system")).unwrap();
        symlink(d.join("store/gen2"), d.join("current")).unwrap();
        symlink(d.join("store/gen1"), d.join("booted")).unwrap();
        let rev = "0f3c5a9e2b7d4c1a8e6f9b0d2c4e6a8b1d3f5a7c";
        std::fs::write(
            d.join("store/gen2/hinteros/flake.lock"),
            format!(
                r#"{{"nodes":{{"hinteros":{{"locked":{{"rev":"{rev}"}},"original":{{"ref":"stable"}}}},"root":{{"inputs":{{"hinteros":"hinteros"}}}}}},"root":"root","version":7}}"#
            ),
        )
        .unwrap();

        let profile = d.join("profiles/system");
        assert_eq!(current_generation(&profile), Some(2));
        std::fs::write(
            d.join("store/gen2/hinteros/state.json"),
            r#"{"schemaVersion":2,"base":"stable","stateVersion":"26.11","settings":{"ssh":{"enable":false}},"requirements":[],"services":[],"profiles":[{"id":"p1"},{"id":"p2"}]}"#,
        )
        .unwrap();
        std::fs::write(
            d.join("profiles.json"),
            r#"{"version":1,"next":3,"profiles":{"p1":{"name":"Sam","color":"teal","admin":true}}}"#,
        )
        .unwrap();
        let gens = list(
            &profile,
            &d.join("current"),
            &d.join("booted"),
            &d.join("entries"),
            &d.join("boot-entries.json"),
            &d.join("profiles.json"),
            None,
        );
        assert!(gens.iter().all(|g| g.readds_features.is_none()));
        let needed: BTreeSet<String> = ["graphics32".to_string()].into();
        let with_needed = list(
            &profile,
            &d.join("current"),
            &d.join("booted"),
            &d.join("entries"),
            &d.join("boot-entries.json"),
            &d.join("profiles.json"),
            Some(&needed),
        );
        assert_eq!(
            with_needed[1].readds_features,
            Some(vec!["graphics32".to_string()])
        );
        assert_eq!(
            with_needed[2].readds_features, None,
            "no state.json: unknown"
        );
        let nums: Vec<u32> = gens.iter().map(|g| g.generation).collect();
        assert_eq!(nums, [10, 2, 1]);
        assert!(gens[1].current && !gens[1].booted);
        assert_eq!(gens[1].rev.as_deref(), Some(rev));
        assert!(gens[2].booted && !gens[2].current);
        assert_eq!(gens[0].rev, None);
        let p2 = gens[1].profiles.as_ref().unwrap();
        assert_eq!(p2.len(), 2);
        assert_eq!(
            (p2[0].id.as_str(), p2[0].name.as_deref()),
            ("p1", Some("Sam"))
        );
        assert_eq!((p2[1].id.as_str(), p2[1].name.as_deref()), ("p2", None));
        assert_eq!(gens[0].profiles, None);
    }

    /// Only the newest `configurationLimit` generations have a boot entry; a rollback to an
    /// older one fails, so the list leaves it out (but never the current one).
    #[test]
    fn leaves_out_generations_without_a_boot_entry() {
        let dir = tempfile::tempdir().unwrap();
        // Entries name canonical store paths.
        let d = &std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(d.join("profiles")).unwrap();
        std::fs::create_dir_all(d.join("entries")).unwrap();
        for n in [1, 2, 3, 4] {
            std::fs::create_dir_all(d.join(format!("store/gen{n}"))).unwrap();
            symlink(
                d.join(format!("store/gen{n}")),
                d.join(format!("profiles/system-{n}-link")),
            )
            .unwrap();
        }
        symlink("system-1-link", d.join("profiles/system")).unwrap();
        symlink(d.join("store/gen1"), d.join("current")).unwrap();
        symlink(d.join("store/gen1"), d.join("booted")).unwrap();
        let list_all = || {
            list(
                &d.join("profiles/system"),
                &d.join("current"),
                &d.join("booted"),
                &d.join("entries"),
                &d.join("boot-entries.json"),
                &d.join("profiles.json"),
                None,
            )
            .iter()
            .map(|g| g.generation)
            .collect::<Vec<_>>()
        };

        // No entries (another boot loader): nothing is known, all are listed.
        assert_eq!(boot_toplevels(&d.join("entries")), None);
        assert_eq!(boot_toplevels(&d.join("missing")), None);
        assert_eq!(list_all(), [4, 3, 2, 1]);

        for n in [3, 4] {
            let toplevel = d.join(format!("store/gen{n}"));
            std::fs::write(
                d.join(format!("entries/nixos-generation-{n}+3-0.conf")),
                format!(
                    "title HinterOS\nlinux /EFI/nixos/k.efi\noptions init={}/init loglevel=4\n",
                    toplevel.display()
                ),
            )
            .unwrap();
        }
        std::fs::write(d.join("entries/other.conf"), "options init=/x/init\n").unwrap();
        let tops = boot_toplevels(&d.join("entries")).unwrap();
        assert_eq!(tops.len(), 2);
        assert!(tops.contains(&d.join("store/gen3")));
        // 2 has no entry; 1 has none either but is the current one.
        assert_eq!(list_all(), [4, 3, 1]);

        // The service is not root and cannot read the ESP: it uses the index the helper wrote.
        assert_eq!(
            index_main(&d.join("entries"), &d.join("boot-entries.json")),
            std::process::ExitCode::SUCCESS
        );
        if !rustix::process::geteuid().is_root() {
            use std::os::unix::fs::PermissionsExt;
            let mode = |m| std::fs::Permissions::from_mode(m);
            std::fs::set_permissions(d.join("entries"), mode(0o000)).unwrap();
            assert_eq!(list_all(), [4, 3, 1]);
            std::fs::remove_file(d.join("boot-entries.json")).unwrap();
            assert_eq!(list_all(), [4, 3, 2, 1], "no index: nothing is known");
            std::fs::set_permissions(d.join("entries"), mode(0o755)).unwrap();
        }
    }
}
