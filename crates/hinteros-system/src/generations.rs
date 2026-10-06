//! System generations: the `<profile>-<N>-link` symlinks next to the system profile.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use hinteros_api::system::Generation;

use crate::flake;

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

/// `generations.list`.
pub fn list(profile: &Path, current_system: &Path, booted_system: &Path) -> Vec<Generation> {
    let current = std::fs::canonicalize(current_system).ok();
    let booted = std::fs::canonicalize(booted_system).ok();
    links(profile)
        .into_iter()
        .map(|l| {
            let rev = l.toplevel.as_ref().and_then(|t| {
                flake::read_lock(&t.join("hinteros/flake.lock"))
                    .ok()
                    .and_then(|lock| lock.rev)
            });
            Generation {
                generation: l.generation,
                date: l.date,
                rev,
                current: l.toplevel.is_some() && l.toplevel == current,
                booted: l.toplevel.is_some() && l.toplevel == booted,
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
        let gens = list(&profile, &d.join("current"), &d.join("booted"));
        let nums: Vec<u32> = gens.iter().map(|g| g.generation).collect();
        assert_eq!(nums, [10, 2, 1]);
        assert!(gens[1].current && !gens[1].booted);
        assert_eq!(gens[1].rev.as_deref(), Some(rev));
        assert!(gens[2].booted && !gens[2].current);
        assert_eq!(gens[0].rev, None);
    }
}
