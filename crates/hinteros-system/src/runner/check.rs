//! What a job checks of its spec before it runs (README, "Privileges"): the service that wrote
//! it is not root, so a spec says what to do, never where or with which program. Paths,
//! programs and the bus are the job's own ([`super::Own`]); what a spec still names (the
//! channel's repository and branches, homes, data to delete) must be what the job's own
//! configuration and the user database say, or the job does not run.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use super::Own;
use crate::flake::{self, ChannelTable};
use crate::job::{Action, Apply, DeleteData, Home, JobSpec, MeasureHome, UpdateSource};
use crate::state::{self, GUEST_USER, State};
use crate::users::Users;

/// Check `spec` against the job's own configuration; what can be had again from it (the
/// channel's repository and URLs) is replaced, anything else that does not match fails the job.
pub fn check(spec: &mut JobSpec, own: &Own) -> anyhow::Result<()> {
    let uid = spec.uid;
    match &mut spec.action {
        Action::Apply(a) => apply(a, uid, own),
        Action::Services(j) => apply(&mut j.apply, uid, own),
        Action::UpdateCheck(s) | Action::ChannelCheck(s) => source(s, own),
        Action::Apps(j) => {
            if let Some(d) = &mut j.delete_data {
                delete_homes(d, uid, own)?;
            }
            Ok(())
        }
        Action::ClearData(d) => delete_homes(d, uid, own),
        Action::ProfileRemove(j) => {
            if !state::is_profile_id(&j.id) || j.user != state::user_for(&j.id) {
                bail!("{} is not the account of profile {}", j.user, j.id);
            }
            let user = own.paths.users.by_name(&j.user);
            j.uid = user.as_ref().map(|u| u.uid);
            j.home = user.and_then(|u| home_of(u.uid, u.home));
            Ok(())
        }
        Action::Rollback { .. }
        | Action::ProfileCreate(_)
        | Action::GuestEnd(_)
        | Action::StorageMeasure(_)
        | Action::Cleanup(_)
        | Action::Jellyfin(_)
        | Action::Plex(_)
        | Action::Tailscale(_)
        | Action::Cloudflare(_) => Ok(()),
    }
}

/// A home root may work in: a regular account's, absolute, not `/`.
fn home_of(uid: u32, home: PathBuf) -> Option<PathBuf> {
    (uid >= 1000 && home.is_absolute() && home != Path::new("/")).then_some(home)
}

fn apply(a: &mut Apply, uid: u32, own: &Own) -> anyhow::Result<()> {
    if let Some(d) = &mut a.delete_data {
        delete_homes(d, uid, own)?;
    }
    let Some(u) = &mut a.update else {
        return Ok(());
    };
    source(&mut u.source, own)?;
    let table = ChannelTable::load(&own.policy_dir).context("the channel table")?;
    u.input_url = table.input_url(&u.source.branch, None);
    if let Some(from) = &u.retarget_from
        && !table
            .branches
            .values()
            .any(|b| table.input_url(b, None) == *from)
    {
        bail!("{from} is not the input URL of a channel");
    }
    Ok(())
}

/// The repository and branches from the running generation's channel table, never the spec's.
fn source(s: &mut UpdateSource, own: &Own) -> anyhow::Result<()> {
    let table = ChannelTable::load(&own.policy_dir).context("the channel table")?;
    let branch = table
        .branch(s.channel)
        .with_context(|| format!("the channel table has no {}", s.channel.name()))?;
    if s.branch != branch {
        bail!("{} is not the branch of {}", s.branch, s.channel.name());
    }
    s.repo = table.repo.clone();
    s.scheme = table.scheme().unwrap_or("https").to_owned();
    s.host = table.host();
    s.others.retain(|b| table.channel_of(b).is_some());
    if let Some(c) = &mut s.catalog {
        c.input_url = table.input_url(branch, None);
    }
    Ok(())
}

/// The running generation's update policy files (`allowed_signers`), the job's own.
pub fn allowed_signers(own: &Own) -> PathBuf {
    flake::allowed_signers(&own.policy_dir)
}

/// App data is deleted only in the home of the account that asked (`uid`), a profile's or the
/// guest's, as the user database has it.
fn delete_homes(d: &mut DeleteData, uid: u32, own: &Own) -> anyhow::Result<()> {
    if let Some(dir) = &own.test_profile_home {
        d.homes = vec![Home {
            user: "test".into(),
            uid,
            dir: dir.clone(),
        }];
        return Ok(());
    }
    let accounts: Vec<String> = State::load(&own.paths.state_json())
        .map(|(s, _)| s.accounts().into_iter().map(|a| a.user).collect())
        .unwrap_or_default();
    for h in &d.homes {
        let known = own
            .paths
            .users
            .by_name(&h.user)
            .and_then(|u| Some((u.uid, home_of(u.uid, u.home)?)));
        let ok = h.uid == uid
            && (h.user == GUEST_USER || accounts.contains(&h.user))
            && known == Some((h.uid, h.dir.clone()));
        if !ok {
            bail!(
                "{} ({}) is not the home of the profile that asked",
                h.dir.display(),
                h.user
            );
        }
    }
    Ok(())
}

/// The homes to measure, from the user database (the spec names the profiles, not where their
/// homes are).
pub fn measure_homes(homes: &[MeasureHome], users: &Users) -> Vec<MeasureHome> {
    homes
        .iter()
        .map(|h| {
            let user = if h.id == "guest" {
                Some(GUEST_USER.to_owned())
            } else {
                state::is_profile_id(&h.id).then(|| state::user_for(&h.id))
            };
            MeasureHome {
                id: h.id.clone(),
                home: user
                    .and_then(|n| users.by_name(&n))
                    .and_then(|u| home_of(u.uid, u.home)),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use hinteros_api::system::{Channel, JobKind};

    use super::*;
    use crate::job::{ApplyUpdate, NotFastForward, ProfileRemoveJob};

    /// A job's own configuration in `d`: the channel table, a user database with a profile, the
    /// guest, root and a stranger, and a state with profile p1.
    fn own(d: &Path) -> Own {
        std::fs::create_dir_all(d.join("policy/update")).unwrap();
        std::fs::write(
            d.join("policy/update/channels.json"),
            r#"{"repo": "https://git.hinteros.org/hinteros.git",
                "branches": {"stable": "stable", "beta": "beta"}}"#,
        )
        .unwrap();
        std::fs::write(
            d.join("passwd"),
            "root:x:0:0::/root:/bin/sh\n\
             hinteros-p1:x:20001:20001::/home/p1:/bin/sh\n\
             hinteros-guest:x:20000:20000::/home/guest:/bin/sh\n\
             stranger:x:1001:100::/home/stranger:/bin/sh\n",
        )
        .unwrap();
        std::fs::create_dir_all(d.join("etc")).unwrap();
        std::fs::write(
            d.join("etc/state.json"),
            r#"{"schemaVersion": 2, "base": "unstable", "stateVersion": "25.11",
                "profiles": [{"id": "p1"}], "requirements": [], "services": []}"#,
        )
        .unwrap();
        let args = crate::config::Cli::parse_from([
            "hinteros-system",
            "--config-dir",
            &d.join("etc").display().to_string(),
            "--policy-dir",
            &d.join("policy").display().to_string(),
            "--passwd-file",
            &d.join("passwd").display().to_string(),
        ]);
        Own::new(&args.serve)
    }

    fn spec(uid: u32, action: Action) -> JobSpec {
        JobSpec {
            id: "0123456789abcdef".into(),
            kind: JobKind::Rebuild,
            uid,
            created_at: 0,
            action,
            subject: Default::default(),
        }
    }

    fn source(repo: &str, branch: &str) -> UpdateSource {
        UpdateSource {
            repo: repo.into(),
            scheme: "file".into(),
            host: "evil".into(),
            branch: branch.into(),
            channel: Channel::Stable,
            current_rev: None,
            catalog: None,
            others: vec!["beta".into(), "elsewhere".into()],
        }
    }

    #[test]
    fn the_repository_and_urls_are_the_jobs_own() {
        let d = tempfile::tempdir().unwrap();
        let own = own(d.path());
        let mut s = spec(
            20001,
            Action::UpdateCheck(source("file:///tmp/evil", "stable")),
        );
        check(&mut s, &own).unwrap();
        let Action::UpdateCheck(got) = &s.action else {
            unreachable!()
        };
        assert_eq!(got.repo, "https://git.hinteros.org/hinteros.git");
        assert_eq!(
            (got.scheme.as_str(), got.host.as_str()),
            ("https", "git.hinteros.org")
        );
        assert_eq!(got.others, ["beta"]);
        assert_eq!(
            allowed_signers(&own),
            d.path().join("policy/update/allowed_signers")
        );

        // A branch that is not the channel's: refused.
        let mut s = spec(20001, Action::ChannelCheck(source("x", "main")));
        assert!(check(&mut s, &own).is_err());

        // An update locks the channel's URL, whatever the spec says; a retarget only from a
        // channel's URL.
        let update = |from: Option<&str>| {
            spec(
                20001,
                Action::Apply(Apply {
                    update: Some(ApplyUpdate {
                        source: source("x", "stable"),
                        rev: None,
                        not_fast_forward: NotFastForward::Reject,
                        input_url: "git+https://evil.example/x?ref=stable".into(),
                        retarget_from: from.map(str::to_owned),
                    }),
                    ..Default::default()
                }),
            )
        };
        let mut s = update(Some("git+https://git.hinteros.org/hinteros.git?ref=beta"));
        check(&mut s, &own).unwrap();
        let Action::Apply(Apply {
            update: Some(u), ..
        }) = &s.action
        else {
            unreachable!()
        };
        assert_eq!(
            u.input_url,
            "git+https://git.hinteros.org/hinteros.git?ref=stable"
        );
        assert!(
            check(
                &mut update(Some("git+https://evil.example/x?ref=beta")),
                &own
            )
            .is_err()
        );
    }

    #[test]
    fn data_is_deleted_only_in_the_callers_profile_home() {
        let d = tempfile::tempdir().unwrap();
        let own = own(d.path());
        let data = |user: &str, uid: u32, dir: &str| {
            Action::ClearData(DeleteData {
                homes: vec![Home {
                    user: user.into(),
                    uid,
                    dir: dir.into(),
                }],
                paths: vec![".local/share/x".into()],
            })
        };
        assert!(
            check(
                &mut spec(20001, data("hinteros-p1", 20001, "/home/p1")),
                &own
            )
            .is_ok()
        );
        assert!(
            check(
                &mut spec(20000, data(GUEST_USER, 20000, "/home/guest")),
                &own
            )
            .is_ok()
        );
        // Another profile's home, a home that is not the account's, an account that is not a
        // profile, root.
        assert!(
            check(
                &mut spec(20000, data("hinteros-p1", 20001, "/home/p1")),
                &own
            )
            .is_err()
        );
        assert!(check(&mut spec(20001, data("hinteros-p1", 20001, "/etc")), &own).is_err());
        assert!(
            check(
                &mut spec(1001, data("stranger", 1001, "/home/stranger")),
                &own
            )
            .is_err()
        );
        assert!(check(&mut spec(0, data("root", 0, "/root")), &own).is_err());
    }

    #[test]
    fn homes_come_from_the_user_database() {
        let d = tempfile::tempdir().unwrap();
        let own = own(d.path());
        let mut s = spec(
            0,
            Action::ProfileRemove(ProfileRemoveJob {
                state: State::parse(&std::fs::read(d.path().join("etc/state.json")).unwrap())
                    .unwrap(),
                id: "p1".into(),
                user: "hinteros-p1".into(),
                uid: Some(0),
                home: Some("/".into()),
            }),
        );
        check(&mut s, &own).unwrap();
        let Action::ProfileRemove(j) = &s.action else {
            unreachable!()
        };
        assert_eq!(
            (j.uid, j.home.as_deref()),
            (Some(20001), Some(Path::new("/home/p1")))
        );
        let Action::ProfileRemove(j) = &mut s.action else {
            unreachable!()
        };
        j.user = "stranger".into();
        assert!(check(&mut s, &own).is_err());

        let homes = measure_homes(
            &[
                MeasureHome {
                    id: "p1".into(),
                    home: Some("/etc".into()),
                },
                MeasureHome {
                    id: "guest".into(),
                    home: None,
                },
                MeasureHome {
                    id: "p9".into(),
                    home: Some("/root".into()),
                },
            ],
            &own.paths.users,
        );
        let got: Vec<_> = homes.iter().map(|h| h.home.as_deref()).collect();
        assert_eq!(
            got,
            [
                Some(Path::new("/home/p1")),
                Some(Path::new("/home/guest")),
                None
            ]
        );
    }
}
