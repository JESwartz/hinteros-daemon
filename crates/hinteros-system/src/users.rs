//! The user database: the system's (NSS) on a box, or passwd/group files in the tests (hidden
//! `--passwd-file`/`--group-file` flags), so the integration tests can map their own uid to a
//! profile account without root.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

/// Where users come from (part of job specs, so jobs see the same database).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Users {
    /// A passwd-format file instead of the system database (tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passwd: Option<PathBuf>,
    /// A group-format file instead of the system database (tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<PathBuf>,
}

fn parse_passwd(text: &str) -> Vec<User> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() < 7 {
                return None;
            }
            Some(User {
                name: f[0].to_owned(),
                uid: f[2].parse().ok()?,
                gid: f[3].parse().ok()?,
                home: PathBuf::from(f[5]),
            })
        })
        .collect()
}

impl Users {
    fn file_users(path: &Path) -> Vec<User> {
        std::fs::read_to_string(path)
            .map(|t| parse_passwd(&t))
            .unwrap_or_default()
    }

    pub fn by_uid(&self, uid: u32) -> Option<User> {
        match &self.passwd {
            Some(p) => Self::file_users(p).into_iter().find(|u| u.uid == uid),
            None => {
                let u = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)).ok()??;
                Some(User {
                    name: u.name,
                    uid,
                    gid: u.gid.as_raw(),
                    home: u.dir,
                })
            }
        }
    }

    pub fn by_name(&self, name: &str) -> Option<User> {
        match &self.passwd {
            Some(p) => Self::file_users(p).into_iter().find(|u| u.name == name),
            None => {
                let u = nix::unistd::User::from_name(name).ok()??;
                Some(User {
                    name: u.name,
                    uid: u.uid.as_raw(),
                    gid: u.gid.as_raw(),
                    home: u.dir,
                })
            }
        }
    }

    /// Every user name in the database whose name is `hinteros-p<N>` or `hinteros`, with N
    /// (for the profile counter: numbers are never reused).
    pub fn profile_numbers(&self) -> Vec<u32> {
        let names: Vec<String> = match &self.passwd {
            Some(p) => Self::file_users(p).into_iter().map(|u| u.name).collect(),
            None => std::fs::read_to_string("/etc/passwd")
                .map(|t| parse_passwd(&t).into_iter().map(|u| u.name).collect())
                .unwrap_or_default(),
        };
        names
            .iter()
            .filter_map(|n| {
                if n == crate::state::P1_USER {
                    Some(1)
                } else {
                    n.strip_prefix("hinteros-")
                        .and_then(crate::state::profile_number)
                }
            })
            .collect()
    }

    /// The members of group `name` (its member list; primary members are not listed by NSS
    /// either).
    pub fn group_members(&self, name: &str) -> Vec<String> {
        match &self.group {
            Some(p) => std::fs::read_to_string(p)
                .unwrap_or_default()
                .lines()
                .find_map(|l| {
                    let f: Vec<&str> = l.split(':').collect();
                    (f.len() >= 4 && f[0] == name).then(|| {
                        f[3].split(',')
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                            .collect()
                    })
                })
                .unwrap_or_default(),
            None => nix::unistd::Group::from_name(name)
                .ok()
                .flatten()
                .map(|g| g.mem)
                .unwrap_or_default(),
        }
    }

    /// The gid of group `name`.
    pub fn group_gid(&self, name: &str) -> Option<u32> {
        match &self.group {
            Some(p) => std::fs::read_to_string(p).ok()?.lines().find_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                (f.len() >= 3 && f[0] == name)
                    .then(|| f[2].parse().ok())
                    .flatten()
            }),
            None => nix::unistd::Group::from_name(name)
                .ok()
                .flatten()
                .map(|g| g.gid.as_raw()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files() {
        let d = tempfile::tempdir().unwrap();
        let passwd = d.path().join("passwd");
        let group = d.path().join("group");
        std::fs::write(
            &passwd,
            "root:x:0:0::/root:/bin/sh\nhinteros:x:1000:100::/home/hinteros:/bin/sh\n\
             hinteros-p3:x:20003:100::/home/hinteros-p3:/bin/sh\nbad line\n",
        )
        .unwrap();
        std::fs::write(
            &group,
            "users:x:100:\nhinteros:x:990:hinteros,hinteros-p3\n",
        )
        .unwrap();
        let u = Users {
            passwd: Some(passwd),
            group: Some(group),
        };
        assert_eq!(u.by_uid(1000).unwrap().name, "hinteros");
        assert_eq!(
            u.by_name("hinteros-p3").unwrap().home,
            Path::new("/home/hinteros-p3")
        );
        assert!(u.by_uid(5).is_none());
        let mut n = u.profile_numbers();
        n.sort();
        assert_eq!(n, [1, 3]);
        assert_eq!(u.group_members("hinteros"), ["hinteros", "hinteros-p3"]);
        assert_eq!(u.group_gid("hinteros"), Some(990));
        assert!(u.group_members("nope").is_empty());
    }
}
