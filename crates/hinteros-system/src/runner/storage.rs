//! Storage jobs: measuring the disk, and the two clean-ups.
//!
//! The measure job logs counts only: a home's file and folder names stay in `storage.json`
//! (root, 0600), never in the journal, a progress line or a result.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use hinteros_api::system::{
    CleanedResult, JobResult, MeasuredResult, Measurement, NodeKind, Stage, StorageNode,
};
use tracing::{info, warn};

use super::{Failure, Outcome, ProgressLine, Runner, check};
use crate::appreg::Owner;
use crate::generations;
use crate::job::{self, CleanupJob, CleanupTarget, MeasureJob};
use crate::storage::{self, GenFacts, Limits, OwnerFacts, StoreFacts, Stored};

/// Most stdout kept from `nix path-info --all` / `--print-live` (about 100 bytes per path).
const BIG_OUTPUT: u64 = 256 << 20;

/// Store paths in a command's output (one per line, possibly followed by more columns).
fn paths_in(out: &str) -> impl Iterator<Item = &str> {
    out.lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|p| p.starts_with('/'))
}

/// `nix path-info -s` lines: `<path> <nar size>`.
pub fn parse_sizes(out: &str) -> HashMap<String, u64> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let path = it.next().filter(|p| p.starts_with('/'))?;
            let size = it.last()?.parse().ok()?;
            Some((path.to_owned(), size))
        })
        .collect()
}

/// Generations and the store path each points to.
type Gens = Vec<(GenFacts, Option<PathBuf>)>;

impl Runner {
    fn nix_store(&self) -> Command {
        Command::new(&self.tools.nix_store)
    }

    fn store_facts(&mut self) -> Result<StoreFacts, Failure> {
        let mut cmd = self.nix_command();
        cmd.args(["path-info", "--all", "-s"]);
        let out = self.exec_with(cmd, true, BIG_OUTPUT, &mut |_, _| {})?;
        let sizes = parse_sizes(&out);
        let mut cmd = self.nix_store();
        cmd.args(["--gc", "--print-live"]);
        let out = self.exec_with(cmd, true, BIG_OUTPUT, &mut |_, _| {})?;
        let live = paths_in(&out).map(str::to_owned).collect();
        Ok(StoreFacts { sizes, live })
    }

    /// `nix-store -qR <path>`; empty (logged) if the path cannot be queried.
    fn closure(&mut self, path: &Path) -> Result<Vec<String>, Failure> {
        let mut cmd = self.nix_store();
        cmd.arg("--query").arg("--requisites").arg(path);
        match self.exec_with(cmd, true, BIG_OUTPUT, &mut |_, _| {}) {
            Ok(out) => Ok(paths_in(&out).map(str::to_owned).collect()),
            Err(Failure::Cancelled) => Err(Failure::Cancelled),
            Err(Failure::Error(e)) => {
                warn!(path = %path.display(), "no closure: {}", e.message);
                Ok(Vec::new())
            }
        }
    }

    fn measuring(&mut self, done: usize, total: usize) {
        let percent = (done * 100 / total.max(1)).min(100) as u8;
        self.report(ProgressLine {
            stage: Stage::Measuring,
            percent: Some(percent),
            detail: None,
            activity_at: None,
            subject: None,
        });
    }

    /// System generations as facts (closures filled in later).
    fn system_gens(&self) -> Gens {
        let paths = &self.paths;
        let current = fs::canonicalize(&paths.current_system).ok();
        let booted = fs::canonicalize(&paths.booted_system).ok();
        let profile_gen = generations::current_generation(&paths.profile);
        generations::links(&paths.profile)
            .into_iter()
            .map(|l| {
                let g = GenFacts {
                    generation: Some(l.generation),
                    date: l.date,
                    current: (l.toplevel.is_some() && l.toplevel == current)
                        || Some(l.generation) == profile_gen,
                    booted: l.toplevel.is_some() && l.toplevel == booted,
                    closure: Vec::new(),
                };
                (g, l.toplevel)
            })
            .collect()
    }

    /// An owner's app generations as facts (the guest: its single link).
    fn owner_gens(&self, owner: &Owner) -> Gens {
        let link = owner.link(&self.paths);
        if !owner.has_generations() {
            return fs::canonicalize(&link)
                .ok()
                .map(|env| (GenFacts::default(), Some(env)))
                .into_iter()
                .collect();
        }
        let current = generations::current_generation(&link);
        generations::links(&link)
            .into_iter()
            .map(|l| {
                let g = GenFacts {
                    generation: Some(l.generation),
                    date: l.date,
                    current: Some(l.generation) == current,
                    booted: false,
                    closure: Vec::new(),
                };
                (g, l.toplevel)
            })
            .collect()
    }

    pub(super) fn storage_measure(&mut self, j: &MeasureJob) -> Outcome {
        self.set_stage(Stage::Preparing);
        let paths = self.paths.clone();
        let disk = storage::disk_usage(&self.own.store_dir)
            .ok_or_else(|| self.fail("cannot read the store's filesystem"))?;
        let store_fs = storage::filesystem_of(&self.own.store_dir);
        let on_store_fs =
            |dir: &Path| store_fs.is_some() && storage::filesystem_of(dir) == store_fs;
        let mut system = self.system_gens();
        let mut owners: Vec<(String, Gens)> = j
            .owners
            .iter()
            .map(|o| (o.key(), self.owner_gens(o)))
            .collect();
        let total =
            3 + system.len() + owners.iter().map(|(_, g)| g.len()).sum::<usize>() + j.homes.len();
        let mut done = 0;
        self.set_stage(Stage::Measuring);
        self.measuring(done, total);

        let facts = self.store_facts().map_err(|e| match e {
            Failure::Error(e) => self.fail(format!("reading the store: {}", e.message)),
            c => c,
        })?;
        done += 2;
        self.measuring(done, total);
        for (g, top) in system
            .iter_mut()
            .chain(owners.iter_mut().flat_map(|(_, g)| g.iter_mut()))
        {
            self.check_cancel()?;
            if let Some(top) = top.clone() {
                g.closure = self.closure(&top)?;
            }
            done += 1;
            self.measuring(done, total);
        }
        let system: Vec<GenFacts> = system.into_iter().map(|(g, _)| g).collect();
        let owners: Vec<OwnerFacts> = owners
            .into_iter()
            .map(|(id, gens)| OwnerFacts {
                id,
                gens: gens.into_iter().map(|(g, _)| g).collect(),
            })
            .collect();
        let mut store = storage::store_tree(&facts, &system, &owners);
        drop(facts);

        // The homes: walked in-process (cancel is checked every few thousand entries).
        let cancel_file = self.dir.join(job::CANCEL);
        let cancel = move || cancel_file.exists();
        let mut homes: Vec<(StorageNode, bool)> = Vec::new();
        let mut ids: Vec<String> = Vec::new();
        for h in &check::measure_homes(&j.homes, &paths.users) {
            self.check_cancel()?;
            let kind = if h.id == "guest" {
                NodeKind::Guest
            } else {
                NodeKind::Profile
            };
            let measured = h
                .home
                .as_deref()
                .and_then(|home| storage::measure_home(home, kind, Limits::default(), &cancel));
            self.check_cancel()?;
            let (mut tree, on_disk) = match (measured, h.home.as_deref()) {
                (Some(tree), Some(home)) => (tree, on_store_fs(home)),
                _ => (StorageNode::new(kind, 0), false),
            };
            if kind == NodeKind::Profile {
                tree.id = Some(h.id.clone());
            }
            info!(
                home = %h.id,
                bytes = tree.bytes,
                shown = node_count(&tree),
                "home measured"
            );
            homes.push((tree, on_disk));
            ids.push(h.id.clone());
            done += 1;
            self.measuring(done, total);
        }
        let media = storage::measure_media(&self.own.srv_dir, Limits::default(), &cancel)
            .map(|m| (m, on_store_fs(&self.own.srv_dir)));
        self.check_cancel()?;
        info!(
            bytes = media.as_ref().map(|(m, _)| m.bytes),
            "/srv measured"
        );
        done += 1;
        self.measuring(done, total);

        self.set_stage(Stage::Finishing);
        let measured_at = job::now();
        // Scales `homes` too: the detailed trees stay equal to the top level.
        let root = storage::top_level(disk, &mut store, &mut homes, media);
        let detailed: BTreeMap<String, StorageNode> = ids
            .into_iter()
            .zip(homes.into_iter().map(|(h, _)| h))
            .collect();
        let stored = Stored {
            version: storage::STORED_VERSION,
            measurement: Measurement {
                measured_at,
                disk,
                root,
                garbage_bytes: store.garbage_bytes,
                estimates: store.estimates,
            },
            homes: detailed,
        };
        stored
            .save(&paths.storage_json())
            .map_err(|e| self.fail(format!("writing the measurement: {e}")))?;
        info!(
            system = stored.measurement.root.children.first().map(|c| c.bytes),
            garbage = stored.measurement.garbage_bytes,
            "storage measured"
        );
        Ok((
            Some(JobResult::Measured(MeasuredResult { measured_at })),
            false,
        ))
    }

    /// Past this point a job cannot be cancelled (no stage change, unlike activation).
    fn point_of_no_return(&mut self) -> Result<(), Failure> {
        let created = crate::fsutil::open(
            &self.dir.join(job::ACTIVATING),
            rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE,
            0o600,
        );
        if let Err(e) = created {
            return Err(self.fail(format!("cannot mark the job: {e}")));
        }
        if self.cancel_requested() {
            let _ = fs::remove_file(self.dir.join(job::ACTIVATING));
            return Err(Failure::Cancelled);
        }
        Ok(())
    }

    pub(super) fn cleanup(&mut self, j: &CleanupJob) -> Outcome {
        self.set_stage(Stage::Preparing);
        self.inhibit("delay");
        let paths = self.paths.clone();
        let free_before = storage::disk_usage(&self.own.store_dir).map(|d| d.free_bytes);
        // What goes: per Nix profile, the generation numbers.
        let plan: Vec<(PathBuf, Vec<u32>)> = match j.target {
            CleanupTarget::System => {
                let gens: Vec<GenFacts> = self.system_gens().into_iter().map(|(g, _)| g).collect();
                vec![(paths.profile.clone(), storage::removed_by(&gens, j.keep))]
            }
            CleanupTarget::Profiles => {
                let mut seen = HashSet::new();
                j.owners
                    .iter()
                    .filter(|o| o.has_generations() && seen.insert(o.link(&paths)))
                    .map(|o| {
                        let gens: Vec<GenFacts> =
                            self.owner_gens(o).into_iter().map(|(g, _)| g).collect();
                        (o.link(&paths), storage::removed_by(&gens, j.keep))
                    })
                    .collect()
            }
        };
        self.check_cancel()?;
        self.point_of_no_return()?;
        self.set_stage(Stage::Cleaning);
        let mut removed = 0u32;
        for (profile, numbers) in &plan {
            if numbers.is_empty() {
                continue;
            }
            let mut cmd = Command::new(&self.tools.nix_env);
            cmd.arg("-p").arg(profile).arg("--delete-generations");
            cmd.args(numbers.iter().map(u32::to_string));
            self.exec(cmd, false, &mut |_, _| {})?;
            removed += numbers.len() as u32;
            info!(profile = %profile.display(), generations = ?numbers, "generations removed");
        }
        if j.target == CleanupTarget::System && removed > 0 {
            // The boot menu: no entry may point at a removed generation. Without this, the
            // collection below would leave entries whose system is gone, so it is skipped.
            self.inhibit("block");
            let top = fs::canonicalize(&paths.profile)
                .map_err(|e| self.fail(format!("the system profile: {e}")))?;
            let mut cmd = Command::new(top.join("bin/switch-to-configuration"));
            cmd.arg("boot");
            self.exec(cmd, false, &mut |_, _| {})?;
            self.inhibit("delay");
        }
        self.set_stage(Stage::Collecting);
        let mut cmd = self.nix_store();
        cmd.arg("--gc");
        self.exec(cmd, false, &mut |_, _| {})?;
        self.set_stage(Stage::Finishing);
        storage::sync_filesystem(&self.own.store_dir);
        let free_after = storage::disk_usage(&self.own.store_dir).map(|d| d.free_bytes);
        let freed = match (free_before, free_after) {
            (Some(b), Some(a)) => a.saturating_sub(b),
            _ => 0,
        };
        info!(target = ?j.target, keep = j.keep, removed, freed, "clean-up finished");
        Ok((
            Some(JobResult::Cleaned(CleanedResult {
                freed_bytes: freed,
                generations_removed: removed,
            })),
            false,
        ))
    }
}

fn node_count(n: &StorageNode) -> usize {
    1 + n.children.iter().map(node_count).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nix_output() {
        let sizes = parse_sizes(
            "/nix/store/aaa-glibc\t  123456\n/nix/store/bbb-x 7\nwarning: something\n\n",
        );
        assert_eq!(sizes.len(), 2);
        assert_eq!(sizes["/nix/store/aaa-glibc"], 123_456);
        let live: Vec<&str> =
            paths_in("finding garbage collector roots...\n/nix/store/aaa\n/nix/store/bbb\n")
                .collect();
        assert_eq!(live, ["/nix/store/aaa", "/nix/store/bbb"]);
    }
}
