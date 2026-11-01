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
/// features the profiles' apps need now, gives `readds_features`.
pub fn list(
    profile: &Path,
    current_system: &Path,
    booted_system: &Path,
    profiles_json: &Path,
    needed: Option<&BTreeSet<String>>,
) -> Vec<Generation> {
    let current = std::fs::canonicalize(current_system).ok();
    let booted = std::fs::canonicalize(booted_system).ok();
    let names = ProfilesFile::load(profiles_json).unwrap_or_default();
    links(profile)
        .into_iter()
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
            &d.join("profiles.json"),
            None,
        );
        assert!(gens.iter().all(|g| g.readds_features.is_none()));
        let needed: BTreeSet<String> = ["graphics32".to_string()].into();
        let with_needed = list(
            &profile,
            &d.join("current"),
            &d.join("booted"),
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
}
