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
    /// The running generation's App Store catalog index
    /// [default: <current-system>/sw/share/hinteros/catalog.json]
    #[arg(long)]
    pub catalog_index: Option<PathBuf>,
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
        }
    }

    pub fn tools(&self) -> Tools {
        Tools {
            nix: self.nix.clone(),
            nix_env: self.nix_env.clone(),
            git: self.git.clone(),
            ssh_keygen: self.ssh_keygen.clone(),
            nixos_facter: self.nixos_facter.clone(),
        }
    }

    pub fn update_check_cache(&self) -> Duration {
        Duration::from_secs(self.update_check_cache_s)
    }

    pub fn auth_timeout(&self) -> Duration {
        Duration::from_secs(self.auth_timeout_s.max(1))
    }

    pub fn catalog_index(&self) -> PathBuf {
        self.catalog_index
            .clone()
            .unwrap_or_else(|| self.current_system.join("sw/share/hinteros/catalog.json"))
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
}

impl Paths {
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
}
