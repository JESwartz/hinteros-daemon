//! Command line, paths and limits. Every path is configurable so the integration tests can run
//! the real binary against a temporary root; the defaults are the paths on a HinterOS box.
//! Nothing here comes from a client.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[command(flatten)]
    pub serve: ServeArgs,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run one job (what `hinteros-job@<id>.service` executes): reads
    /// `<run-dir>/jobs/<id>/spec.json`, written by the service.
    Job {
        /// Runtime directory holding `jobs/`
        #[arg(long, env = "HINTEROS_RUN_DIR", default_value = "/run/hinteros")]
        run_dir: PathBuf,
        /// Job id (16 hex digits)
        id: String,
    },
    /// Check that state.json files parse with the same `State::parse` the service uses (no
    /// service, no root, no sockets, no D-Bus). Prints `ok <file>` or `invalid <file>: <reason>`;
    /// exits 0 if all are valid, 1 if any is invalid, 2 on usage or I/O errors.
    CheckState {
        /// state.json files to check
        #[arg(required = true, value_name = "FILE")]
        files: Vec<PathBuf>,
    },
    /// The PAM helper (`pam_exec`, as root, `PAM_USER` set): `unlocked`, `verify` (the PIN on
    /// stdin), `end-others` or `splash` (the handover splash on `PAM_TTY`; always exits 0)
    Pin {
        #[arg(value_enum)]
        verb: crate::pam::Verb,
        #[command(flatten)]
        dirs: HelperDirs,
        /// D-Bus address for logind and systemd [default: the system bus]
        #[arg(long)]
        bus: Option<String>,
        /// `splash`: the typeface of the framebuffer splash (the launcher's; TTF/OTF). Without
        /// it, or without a usable framebuffer, the splash is console text
        #[arg(long)]
        font: Option<PathBuf>,
        /// journald's native socket (the audit entry when the admin PIN locks)
        #[arg(long, default_value = crate::pam::JOURNAL_SOCKET)]
        journal_socket: PathBuf,
    },
    /// The framebuffer splash's painter (started by `pin splash`, not by hand)
    #[command(hide = true)]
    SplashPaint(crate::splash::PaintArgs),
    /// Forgotten PIN: remove a profile's PIN and its wrong-PIN wait (root only, from a terminal).
    /// Without a profile: list the profiles. The profile then enters without a PIN and is asked
    /// to set a new one when it starts.
    ResetPin {
        /// The profile's id (`p2`) or its exact name
        profile: Option<String>,
        #[command(flatten)]
        dirs: HelperDirs,
        /// journald's native socket (the audit entry)
        #[arg(long, default_value = "/run/systemd/journal/socket")]
        journal_socket: PathBuf,
    },
    /// Measure every profile's disk use into usage.json (`hinteros-usage.service`)
    Usage {
        #[command(flatten)]
        dirs: HelperDirs,
    },
    /// Apply the time zone `time_zone.set` asked for (`hinteros-time-zone-apply.service`):
    /// the zone in `<run-dir>/time-zone.request` into `<config-dir>/state.json` and the
    /// localtime link
    TimeZoneApply {
        #[command(flatten)]
        dirs: HelperDirs,
        /// The system's tzdata
        #[arg(long, default_value = "/etc/zoneinfo")]
        zoneinfo_dir: PathBuf,
        /// The link to point at `<zoneinfo-dir>/<zone>`
        #[arg(long, default_value = "/etc/localtime")]
        localtime: PathBuf,
    },
}

/// Paths of the helper subcommands (defaults: the box's).
#[derive(Args, Debug, Clone)]
pub struct HelperDirs {
    #[arg(long, default_value = "/var/lib/hinteros")]
    pub state_dir: PathBuf,
    #[arg(long, default_value = "/run/hinteros")]
    pub run_dir: PathBuf,
    #[arg(long, default_value = "/etc/hinteros")]
    pub config_dir: PathBuf,
    /// Tests only: a passwd-format file instead of the system's user database
    #[arg(long, hide = true)]
    pub passwd_file: Option<PathBuf>,
    /// Tests only: a group-format file instead of the system's group database
    #[arg(long, hide = true)]
    pub group_file: Option<PathBuf>,
}

impl HelperDirs {
    pub fn paths(&self) -> Paths {
        crate::pam::paths(
            self.state_dir.clone(),
            self.run_dir.clone(),
            self.config_dir.clone(),
            crate::users::Users {
                passwd: self.passwd_file.clone(),
                group: self.group_file.clone(),
            },
        )
    }
}

#[derive(Args, Debug, Clone)]
pub struct ServeArgs {
    /// Socket to listen on
    #[arg(long, env = "HINTEROS_SYSTEM_SOCKET", default_value = hinteros_api::system::DEFAULT_SOCKET)]
    pub socket: PathBuf,

    /// Device config dir (flake.nix, flake.lock, state.json, facter.json)
    #[arg(long, default_value = "/etc/hinteros")]
    pub config_dir: PathBuf,

    /// State dir (staging/, git/, the last update check)
    #[arg(long, default_value = "/var/lib/hinteros")]
    pub state_dir: PathBuf,

    /// Runtime dir (jobs/)
    #[arg(long, env = "HINTEROS_RUN_DIR", default_value = "/run/hinteros")]
    pub run_dir: PathBuf,

    /// The system profile (generations are `<profile>-<N>-link`)
    #[arg(long, default_value = "/nix/var/nix/profiles/system")]
    pub profile: PathBuf,

    /// The running system
    #[arg(long, default_value = "/run/current-system")]
    pub current_system: PathBuf,

    /// The booted system
    #[arg(long, default_value = "/run/booted-system")]
    pub booted_system: PathBuf,

    /// Release info and update policy of the running generation (`release.json`,
    /// `update/channels.json`, `update/allowed_signers`) [default: <current-system>/sw/share/hinteros]
    #[arg(long)]
    pub policy_dir: Option<PathBuf>,

    /// Where the SSH host keys are (`ssh_host_*_key.pub`)
    #[arg(long, default_value = "/etc/ssh")]
    pub ssh_host_key_dir: PathBuf,

    /// D-Bus address for logind, polkit and systemd [default: the system bus]
    #[arg(long, env = "HINTEROS_SYSTEM_BUS")]
    pub bus: Option<String>,

    /// journald's native socket (structured audit log); stderr only if it is missing
    #[arg(long, default_value = "/run/systemd/journal/socket")]
    pub journal_socket: PathBuf,

    /// nix binary used by jobs
    #[arg(long, default_value = "nix")]
    pub nix: String,
    /// nix-env binary used by jobs (profile switching)
    #[arg(long, default_value = "nix-env")]
    pub nix_env: String,
    /// git binary used by jobs (update checks)
    #[arg(long, default_value = "git")]
    pub git: String,
    /// ssh-keygen binary (git's SSH signature verification)
    #[arg(long, default_value = "ssh-keygen")]
    pub ssh_keygen: String,
    /// nixos-facter binary (hardware.redetect)
    #[arg(long, default_value = "nixos-facter")]
    pub nixos_facter: String,
    /// nix-store binary (the guest's GC root)
    #[arg(long, default_value = "nix-store")]
    pub nix_store: String,

    /// The greeter's socket (`profiles.create` from the picker) [none: no greeter socket]
    #[arg(long, default_value = "/run/hinteros/greeter.sock")]
    pub greeter_socket: Option<PathBuf>,
    /// The group that may connect to the greeter socket
    #[arg(long, default_value = "hinteros-greeter")]
    pub greeter_group: String,

    /// Root of the per-profile app Nix profiles (`apps/<user>`, `starter`)
    #[arg(long, default_value = "/nix/var/nix/profiles/hinteros")]
    pub apps_profiles_dir: PathBuf,
    /// Tests only: a passwd-format file instead of the system's user database
    #[arg(long, hide = true)]
    pub passwd_file: Option<PathBuf>,
    /// Tests only: a group-format file instead of the system's group database
    #[arg(long, hide = true)]
    pub group_file: Option<PathBuf>,
    /// Tests only: look for a removed profile's processes here instead of /proc
    #[arg(long, hide = true)]
    pub test_proc_dir: Option<PathBuf>,

    /// Free memory a rebuild needs (MemAvailable)
    #[arg(long, default_value_t = 2 << 30)]
    pub min_memory_bytes: u64,
    /// Free disk space a rebuild needs (on the Nix store's filesystem)
    #[arg(long, default_value_t = 4 << 30)]
    pub min_disk_bytes: u64,
    /// Filesystem checked for free space
    #[arg(long, default_value = "/nix/store")]
    pub store_dir: PathBuf,

    /// Mutating requests per minute and uid
    #[arg(long, default_value_t = 10, hide = true)]
    pub rate_per_minute: u32,
    /// Burst of mutating requests per uid
    #[arg(long, default_value_t = 3, hide = true)]
    pub rate_burst: u32,
    /// Connections per uid
    #[arg(long, default_value_t = 8, hide = true)]
    pub max_connections_per_uid: usize,
    /// update.check returns the last result within this many seconds
    #[arg(long, default_value_t = 60, hide = true)]
    pub update_check_cache_s: u64,
    /// How long an interactive polkit check (an `auth_admin` action such as SSH) may wait for
    /// the user to authenticate in the session daemon's agent
    #[arg(long, default_value_t = 300)]
    pub auth_timeout_s: u64,
    /// Tests only: use this directory as the only profile home for `apps.remove {delete_data}`
    /// instead of the homes from the user database (so a test never touches a real home)
    #[arg(long, hide = true)]
    pub test_profile_home: Option<PathBuf>,
    /// The guest account's uid (profiles.nix: 20000). `guest.end` wipes only an account named
    /// hinteros-guest with exactly this uid
    #[arg(long, default_value_t = crate::state::GUEST_UID)]
    pub guest_uid: u32,
    /// Shared directories `guest.end` clears of the guest's entries (repeatable)
    /// [default: /tmp, /var/tmp, /dev/shm]
    #[arg(long = "guest-tmp-dir")]
    pub guest_tmp_dirs: Vec<PathBuf>,
    /// The running generation's App Store catalog index
    /// [default: <current-system>/sw/share/hinteros/catalog.json]
    #[arg(long)]
    pub catalog_index: Option<PathBuf>,
    /// The system's tzdata: TZif files and `zone1970.tab` (`time_zone.*`). `/etc/localtime`
    /// points into it
    #[arg(long, default_value = "/etc/zoneinfo")]
    pub zoneinfo_dir: PathBuf,
    /// The link `time_zone.set` points at `<zoneinfo-dir>/<zone>`
    #[arg(long, default_value = "/etc/localtime")]
    pub localtime: PathBuf,
}

impl ServeArgs {
    pub fn policy_dir(&self) -> PathBuf {
        self.policy_dir
            .clone()
            .unwrap_or_else(|| self.current_system.join("sw/share/hinteros"))
    }

    pub fn paths(&self) -> Paths {
        Paths {
            config_dir: self.config_dir.clone(),
            state_dir: self.state_dir.clone(),
            run_dir: self.run_dir.clone(),
            profile: self.profile.clone(),
            current_system: self.current_system.clone(),
            booted_system: self.booted_system.clone(),
            apps_profiles: self.apps_profiles_dir.clone(),
            catalog_index: self.catalog_index.clone(),
            users: self.users(),
            proc_dir: self.test_proc_dir.clone().unwrap_or_else(default_proc),
            zoneinfo_dir: self.zoneinfo_dir.clone(),
            localtime: self.localtime.clone(),
        }
    }

    pub fn users(&self) -> crate::users::Users {
        crate::users::Users {
            passwd: self.passwd_file.clone(),
            group: self.group_file.clone(),
        }
    }

    pub fn tools(&self) -> Tools {
        Tools {
            nix: self.nix.clone(),
            nix_env: self.nix_env.clone(),
            git: self.git.clone(),
            ssh_keygen: self.ssh_keygen.clone(),
            nixos_facter: self.nixos_facter.clone(),
            nix_store: self.nix_store.clone(),
        }
    }

    pub fn update_check_cache(&self) -> Duration {
        Duration::from_secs(self.update_check_cache_s)
    }

    pub fn auth_timeout(&self) -> Duration {
        Duration::from_secs(self.auth_timeout_s.max(1))
    }

    pub fn catalog_index(&self) -> PathBuf {
        self.paths().catalog_index()
    }
}

/// Paths a job needs; part of its spec (the service's own configuration, never a client's).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub run_dir: PathBuf,
    pub profile: PathBuf,
    pub current_system: PathBuf,
    pub booted_system: PathBuf,
    /// Root of the per-profile app Nix profiles.
    #[serde(default = "default_apps_profiles")]
    pub apps_profiles: PathBuf,
    /// The catalog index override (tests); `None`: the running generation's.
    #[serde(default)]
    pub catalog_index: Option<PathBuf>,
    /// Where users come from.
    #[serde(default)]
    pub users: crate::users::Users,
    /// Where processes are looked up (and killed, for a removed profile).
    #[serde(default = "default_proc")]
    pub proc_dir: PathBuf,
    /// The system's tzdata (TZif files and `zone1970.tab`).
    #[serde(default = "default_zoneinfo")]
    pub zoneinfo_dir: PathBuf,
    /// The link to the box's time zone file.
    #[serde(default = "default_localtime")]
    pub localtime: PathBuf,
}

fn default_proc() -> PathBuf {
    "/proc".into()
}

pub fn default_zoneinfo() -> PathBuf {
    "/etc/zoneinfo".into()
}

pub fn default_localtime() -> PathBuf {
    "/etc/localtime".into()
}

fn default_apps_profiles() -> PathBuf {
    "/nix/var/nix/profiles/hinteros".into()
}

impl Paths {
    /// The running generation's catalog index (resolved when called).
    pub fn catalog_index(&self) -> PathBuf {
        self.catalog_index
            .clone()
            .unwrap_or_else(|| self.current_system.join("sw/share/hinteros/catalog.json"))
    }
    /// A profile's app Nix profile (`apps/<user>`, generations `apps/<user>-<n>-link`).
    pub fn app_profile(&self, user: &str) -> PathBuf {
        self.apps_profiles.join("apps").join(user)
    }
    /// The starter env's Nix profile.
    pub fn starter_profile(&self) -> PathBuf {
        self.apps_profiles.join("starter")
    }
    /// `/run/hinteros/apps`: one link per profile user, what sessions read.
    pub fn apps_run_dir(&self) -> PathBuf {
        self.run_dir.join("apps")
    }
    /// The guest's env link (a single link, no generations; an indirect GC root).
    pub fn guest_link(&self) -> PathBuf {
        self.apps_run_dir().join(crate::state::GUEST_USER)
    }
    /// The app registry (owners, their ids and box features).
    pub fn registry(&self) -> PathBuf {
        self.state_dir.join("apps/registry.json")
    }
    /// The starter set's ids.
    pub fn starter_json(&self) -> PathBuf {
        self.state_dir.join("starter.json")
    }
    /// Profile metadata (names, colours, lock flags, the counter).
    pub fn profiles_json(&self) -> PathBuf {
        self.state_dir.join("profiles.json")
    }
    /// PIN hashes and backoff state.
    pub fn pins_dir(&self) -> PathBuf {
        self.state_dir.join("pins")
    }
    /// Shared display mode and audio output.
    pub fn runtime_json(&self) -> PathBuf {
        self.state_dir.join("runtime.json")
    }
    /// Disk use per profile.
    pub fn usage_json(&self) -> PathBuf {
        self.state_dir.join("usage.json")
    }
    /// The last storage measurement (root only: it holds the homes' trees).
    pub fn storage_json(&self) -> PathBuf {
        self.state_dir.join("storage.json")
    }
    /// The greeter's public view.
    pub fn public_json(&self) -> PathBuf {
        self.run_dir.join("profiles-public.json")
    }
    /// The switch hint and one-time token (root only).
    pub fn switch_json(&self) -> PathBuf {
        self.run_dir.join("switch.json")
    }
    /// Created once the service queued the v1 → v2 migration this boot.
    pub fn migrate_marker(&self) -> PathBuf {
        self.run_dir.join("migrate-attempted")
    }
    pub fn jobs_dir(&self) -> PathBuf {
        self.run_dir.join("jobs")
    }
    pub fn staging(&self) -> PathBuf {
        self.state_dir.join("staging")
    }
    pub fn git_dir(&self) -> PathBuf {
        self.state_dir.join("git")
    }
    pub fn last_check(&self) -> PathBuf {
        self.state_dir.join("update-check.json")
    }
    /// Which vendor terms were accepted, when and by whom (written by apps.install jobs).
    pub fn terms_accepted(&self) -> PathBuf {
        self.state_dir.join("terms-accepted.json")
    }
    pub fn state_json(&self) -> PathBuf {
        self.config_dir.join("state.json")
    }
    /// The lock between the service writing `state.json` in place (`time_zone.set`) and a job
    /// replacing `/etc/hinteros` ([`crate::state::lock`]).
    pub fn state_lock(&self) -> PathBuf {
        self.run_dir.join("state.lock")
    }
}

/// External programs a job runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tools {
    pub nix: String,
    pub nix_env: String,
    pub git: String,
    pub ssh_keygen: String,
    pub nixos_facter: String,
    #[serde(default = "default_nix_store")]
    pub nix_store: String,
}

fn default_nix_store() -> String {
    "nix-store".into()
}
