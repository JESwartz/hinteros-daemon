//! Test harness: the real `hinteros-system` binary against a temporary root.
//!
//! - A private `dbus-daemon` with fake logind (seat0's active session, inhibitors), polkit
//!   (`CheckAuthorization` answers per action, records subjects) and systemd (`StartUnit`
//!   of `hinteros-job@<id>.service` runs `hinteros-system job <id>` as a child of the test).
//! - A fake root: `/etc/hinteros`, `/var/lib/hinteros`, `/run/hinteros`, a system profile with
//!   generations, `/run/current-system`, the update policy, and fake `nix`, `nix-env`,
//!   `nixos-facter` and `switch-to-configuration` scripts that log what they were asked to do.
//! - Real git and ssh-keygen for update verification (an upstream repo with SSH-signed
//!   commits).
//!
//! Skipped when dbus-daemon (or git/ssh-keygen) is missing, unless HINTEROS_REQUIRE_TEST_TOOLS
//! is set.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hinteros_api::client::{Client, ClientError};
use hinteros_api::common::Peer;
use hinteros_api::conformance::ApiSchema;
use hinteros_api::system;
use serde_json::{Value, json};
use tokio::time::timeout;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

pub const TIMEOUT: Duration = Duration::from_secs(20);

pub fn tool_missing(tool: &str) -> bool {
    let found = Command::new(tool)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !found {
        assert!(
            std::env::var_os("HINTEROS_REQUIRE_TEST_TOOLS").is_none(),
            "{tool} is required"
        );
        eprintln!("{tool} not found, skipping");
    }
    !found
}

pub fn me() -> u32 {
    rustix::process::getuid().as_raw()
}

// ---------------------------------------------------------------------------------------------
// Private bus

pub struct Bus {
    child: Child,
    pub address: String,
    _dir: tempfile::TempDir,
}

impl Bus {
    pub fn start() -> Option<Self> {
        if tool_missing("dbus-daemon") {
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("bus.conf");
        std::fs::write(
            &config,
            format!(
                r#"<busconfig>
  <type>session</type>
  <listen>unix:path={}/bus</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>"#,
                dir.path().display()
            ),
        )
        .unwrap();
        let mut child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Some(Self {
            child,
            address: address.trim().into(),
            _dir: dir,
        })
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------------------------
// Fakes

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    Challenge,
}

#[derive(Default)]
pub struct FakeState {
    /// uid owning seat0's active session (None: no active session).
    pub seat_user: Option<u32>,
    /// polkit answers per action (default: yes).
    pub answers: HashMap<String, Answer>,
    /// (action, pid in the subject, whether a pidfd came along).
    pub polkit_calls: Vec<(String, Option<u32>, bool)>,
    /// (action, flags, cancellation id) of every check.
    pub polkit_flags: Vec<(String, u32, String)>,
    /// How an interactive check (flags 1) ends, per action: what the agent conversation
    /// gives and how long it takes. Without an entry, interactive checks answer like `answers`.
    pub interactive: HashMap<String, Interactive>,
    /// Cancellation ids of `CancelCheckAuthorization` calls.
    pub cancelled: Vec<String>,
    /// Inhibitor modes taken.
    pub inhibits: Vec<String>,
    /// Started units.
    pub units: Vec<String>,
    pub children: Vec<Child>,
    /// How to run a job: (binary, run dir, extra env).
    pub job_cmd: Option<JobCmd>,
}

/// How the fake systemd runs a job: binary, run dir, extra environment.
pub type JobCmd = (PathBuf, PathBuf, Vec<(String, String)>);

/// The outcome of a fake agent conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interactive {
    pub answer: Answer,
    /// polkit.dismissed (the user cancelled).
    pub dismissed: bool,
    pub delay: Duration,
}

pub type Shared = Arc<Mutex<FakeState>>;

struct Seat(Shared);

#[zbus::interface(name = "org.freedesktop.login1.Seat")]
impl Seat {
    #[zbus(property)]
    fn active_session(&self) -> (String, OwnedObjectPath) {
        match self.0.lock().unwrap().seat_user {
            Some(_) => (
                "1".into(),
                OwnedObjectPath::try_from("/org/freedesktop/login1/session/_31").unwrap(),
            ),
            None => (String::new(), OwnedObjectPath::try_from("/").unwrap()),
        }
    }
}

struct Session(Shared);

#[zbus::interface(name = "org.freedesktop.login1.Session")]
impl Session {
    #[zbus(property)]
    fn user(&self) -> (u32, OwnedObjectPath) {
        let uid = self.0.lock().unwrap().seat_user.unwrap_or(u32::MAX);
        (
            uid,
            OwnedObjectPath::try_from(format!("/org/freedesktop/login1/user/_{uid}")).unwrap(),
        )
    }
}

struct Manager(Shared);

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl Manager {
    fn inhibit(
        &self,
        _what: String,
        _who: String,
        _why: String,
        mode: String,
    ) -> zbus::zvariant::OwnedFd {
        self.0.lock().unwrap().inhibits.push(mode);
        let f = std::fs::File::open("/dev/null").unwrap();
        zbus::zvariant::OwnedFd::from(std::os::fd::OwnedFd::from(f))
    }
}

struct Authority(Shared);

#[zbus::interface(name = "org.freedesktop.PolicyKit1.Authority")]
impl Authority {
    async fn check_authorization(
        &self,
        subject: (String, HashMap<String, OwnedValue>),
        action_id: String,
        _details: HashMap<String, String>,
        flags: u32,
        cancellation_id: String,
    ) -> (bool, bool, HashMap<String, String>) {
        let (answer, extra) = {
            let mut s = self.0.lock().unwrap();
            let pid = subject.1.get("pid").and_then(|v| u32::try_from(v).ok());
            let has_pidfd = subject.1.contains_key("pidfd");
            assert_eq!(subject.0, "unix-process");
            s.polkit_calls.push((action_id.clone(), pid, has_pidfd));
            s.polkit_flags
                .push((action_id.clone(), flags, cancellation_id.clone()));
            let plain = s.answers.get(&action_id).copied().unwrap_or(Answer::Yes);
            match s.interactive.get(&action_id).copied() {
                Some(i) if flags & 1 == 1 => (i.answer, Some(i)),
                _ => (plain, None),
            }
        };
        let mut details = HashMap::new();
        if let Some(i) = extra {
            tokio::time::sleep(i.delay).await;
            if self.0.lock().unwrap().cancelled.contains(&cancellation_id) {
                return (false, false, details);
            }
            if i.dismissed {
                details.insert("polkit.dismissed".into(), "true".into());
            }
        }
        (answer == Answer::Yes, answer == Answer::Challenge, details)
    }

    fn cancel_check_authorization(&self, cancellation_id: String) {
        self.0.lock().unwrap().cancelled.push(cancellation_id);
    }
}

struct Systemd(Shared);

#[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
impl Systemd {
    fn start_unit(&self, name: String, _mode: String) -> zbus::fdo::Result<OwnedObjectPath> {
        let mut s = self.0.lock().unwrap();
        let id = name
            .strip_prefix("hinteros-job@")
            .and_then(|n| n.strip_suffix(".service"))
            .ok_or_else(|| zbus::fdo::Error::Failed(format!("unexpected unit {name}")))?
            .to_owned();
        let (bin, run_dir, env) = s.job_cmd.clone().expect("job command configured");
        let log = std::fs::File::create(run_dir.join(format!("job-{id}.log"))).unwrap();
        let child = Command::new(bin)
            .arg("job")
            .arg("--run-dir")
            .arg(&run_dir)
            .arg(&id)
            .envs(env)
            .env("RUST_LOG", "debug")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        s.children.push(child);
        s.units.push(name);
        Ok(OwnedObjectPath::try_from("/org/freedesktop/systemd1/job/1").unwrap())
    }
}

pub async fn serve_fakes(address: &str, state: Shared) -> zbus::Connection {
    let conn = zbus::connection::Builder::address(address)
        .unwrap()
        .build()
        .await
        .unwrap();
    let server = conn.object_server();
    server
        .at("/org/freedesktop/login1/seat/seat0", Seat(state.clone()))
        .await
        .unwrap();
    server
        .at(
            "/org/freedesktop/login1/session/_31",
            Session(state.clone()),
        )
        .await
        .unwrap();
    server
        .at("/org/freedesktop/login1", Manager(state.clone()))
        .await
        .unwrap();
    server
        .at(
            "/org/freedesktop/PolicyKit1/Authority",
            Authority(state.clone()),
        )
        .await
        .unwrap();
    server
        .at("/org/freedesktop/systemd1", Systemd(state))
        .await
        .unwrap();
    for name in [
        "org.freedesktop.login1",
        "org.freedesktop.PolicyKit1",
        "org.freedesktop.systemd1",
    ] {
        conn.request_name(name).await.unwrap();
    }
    conn
}

// ---------------------------------------------------------------------------------------------
// Fake root

pub fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The fake `nix`: `flake lock --override-input hinteros <url> path:<dir>` writes a lock with
/// the URL's ref and rev; `build ... path:<dir>#...` prints internal-json progress and creates
/// a new "store path" (a generation toplevel with the config snapshot).
const FAKE_NIX: &str = r#"
echo "$@" >> "$FAKE_ROOT/nix.log"
while [ "$1" = "--extra-experimental-features" ]; do shift 2; done
case "$1" in
flake)
  url="$5"; dir="${6#path:}"
  rev="${url##*rev=}"; ref="${url#*ref=}"; ref="${ref%%&*}"
  printf '{"nodes":{"hinteros":{"locked":{"rev":"%s","type":"git"},"original":{"ref":"%s","type":"git"}},"root":{"inputs":{"hinteros":"hinteros"}}},"root":"root","version":7}\n' "$rev" "$ref" > "$dir/flake.lock"
  ;;
build)
  for a in "$@"; do last="$a"; done
  dir="${last#path:}"; dir="${dir%%#*}"
  echo '@nix {"action":"start","id":1,"level":3,"type":103,"text":""}' >&2
  echo '@nix {"action":"result","id":1,"type":105,"fields":[1,4,0,0]}' >&2
  while [ -e "$FAKE_ROOT/build-hold" ]; do sleep 0.05; done
  if [ -e "$FAKE_ROOT/build-fail" ]; then
    echo '@nix {"action":"msg","level":0,"msg":"error: builder for fake.drv failed"}' >&2
    exit 1
  fi
  echo '@nix {"action":"result","id":1,"type":105,"fields":[4,4,0,0]}' >&2
  echo '@nix {"action":"start","id":2,"level":3,"type":105,"text":"building"}' >&2
  n=$(cat "$FAKE_ROOT/counter")
  n=$((n+1)); echo "$n" > "$FAKE_ROOT/counter"
  out="$FAKE_ROOT/store/gen$n"
  "$FAKE_ROOT/mkgen" "$out" "$dir"
  echo "$out"
  ;;
*) echo "fake nix: unexpected $*" >&2; exit 2 ;;
esac
"#;

/// `mkgen <out> <config dir>`: a toplevel with switch-to-configuration, the config snapshot
/// and `sw/bin/hinteros-session` pointing at the package named in `$FAKE_ROOT/session-pkg`.
const MKGEN: &str = r#"
out="$1"; dir="$2"
mkdir -p "$out/bin" "$out/hinteros" "$out/sw/bin"
for f in flake.nix flake.lock state.json; do
  [ -e "$dir/$f" ] && cp "$dir/$f" "$out/hinteros/$f"
done
ln -s "$FAKE_ROOT/pkgs/$(cat "$FAKE_ROOT/session-pkg")/bin/hinteros-session" "$out/sw/bin/hinteros-session"
cat > "$out/bin/switch-to-configuration" <<'EOF'
#!/bin/sh
top=$(dirname "$(dirname "$0")")
echo "$top $1" >> "$FAKE_ROOT/switch.log"
if [ "$1" = switch ] && [ -e "$FAKE_ROOT/switch-fail" ]; then exit 1; fi
if [ "$1" = switch ]; then ln -sfn "$top" "$FAKE_ROOT/current"; fi
exit 0
EOF
chmod +x "$out/bin/switch-to-configuration"
"#;

const FAKE_NIX_ENV: &str = r#"
echo "$@" >> "$FAKE_ROOT/nix-env.log"
p="$2"; dir=$(dirname "$p"); base=$(basename "$p")
case "$3" in
--set)
  max=0
  for l in "$dir/$base"-*-link; do
    n="${l##*/}"; n="${n#$base-}"; n="${n%-link}"
    [ "$n" -gt "$max" ] && max="$n"
  done
  n=$((max+1))
  ln -s "$4" "$dir/$base-$n-link"
  ln -sfn "$base-$n-link" "$p"
  ;;
--switch-generation)
  [ -e "$dir/$base-$4-link" ] || exit 1
  ln -sfn "$base-$4-link" "$p"
  ;;
*) exit 2 ;;
esac
"#;

pub const STATE: &str = r#"{
  "schemaVersion": 1,
  "base": "unstable",
  "settings": {
    "ssh": { "enable": false }
  },
  "apps": ["steam"],
  "profiles": [{ "id": "p1", "name": "Dev", "user": "hinteros" }]
}
"#;

pub struct Root {
    pub dir: tempfile::TempDir,
}

impl Root {
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
    pub fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }
    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.p(rel)).unwrap_or_default()
    }
    pub fn touch(&self, rel: &str) {
        std::fs::write(self.p(rel), "").unwrap();
    }
    pub fn rm(&self, rel: &str) {
        let _ = std::fs::remove_file(self.p(rel));
    }
    pub fn etc_state(&self) -> Value {
        serde_json::from_str(&self.read("etc/hinteros/state.json")).unwrap()
    }
    /// Target of the current system link.
    pub fn current(&self) -> PathBuf {
        std::fs::read_link(self.p("current")).unwrap()
    }

    /// The hinteros input URL of the device flake (the upstream repo of [`Upstream`]).
    pub fn input_url(&self, branch: &str) -> String {
        format!(
            "git+file://{}?ref={branch}",
            self.p("upstream.git").display()
        )
    }

    /// Lock the hinteros input to `rev` on `branch` (in /etc/hinteros and generation 1).
    pub fn set_lock(&self, branch: &str, rev: &str) {
        let lock = format!(
            r#"{{"nodes":{{"hinteros":{{"locked":{{"rev":"{rev}","type":"git"}},"original":{{"ref":"{branch}","type":"git"}}}},"root":{{"inputs":{{"hinteros":"hinteros"}}}}}},"root":"root","version":7}}"#
        );
        std::fs::write(self.p("etc/hinteros/flake.lock"), &lock).unwrap();
        std::fs::write(self.p("store/gen1/hinteros/flake.lock"), &lock).unwrap();
    }

    pub fn create() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let r = Self { dir };
        for d in [
            "etc/hinteros",
            "var/lib/hinteros",
            "run",
            "profiles",
            "store",
            "bin",
            "policy/update",
            "ssh",
            "pkgs/a/bin",
            "pkgs/b/bin",
        ] {
            std::fs::create_dir_all(r.p(d)).unwrap();
        }
        std::fs::write(r.p("pkgs/a/bin/hinteros-session"), "a").unwrap();
        std::fs::write(r.p("pkgs/b/bin/hinteros-session"), "b").unwrap();
        std::fs::write(r.p("session-pkg"), "a").unwrap();
        std::fs::write(
            r.p("etc/hinteros/flake.nix"),
            flake_nix(&r.input_url("stable")),
        )
        .unwrap();
        std::fs::write(r.p("etc/hinteros/state.json"), STATE).unwrap();
        script(&r.p("bin/nix"), FAKE_NIX);
        script(&r.p("mkgen"), MKGEN);
        script(&r.p("bin/nix-env"), FAKE_NIX_ENV);
        script(
            &r.p("bin/nixos-facter"),
            r#"echo "$@" >> "$FAKE_ROOT/facter.log"; echo '{"version":1,"fake":true}' > "$2""#,
        );
        // Generation 1, running and booted, built from the config above.
        std::fs::write(r.p("counter"), "1").unwrap();
        // ETXTBSY: another test thread may be forking while it still holds the script's write
        // descriptor (a fork inherits it until exec); retry briefly.
        let status = (0..50)
            .find_map(|_| {
                match Command::new(r.p("mkgen"))
                    .arg(r.p("store/gen1"))
                    .arg(r.p("etc/hinteros"))
                    .env("FAKE_ROOT", r.path())
                    .status()
                {
                    Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                        std::thread::sleep(Duration::from_millis(20));
                        None
                    }
                    other => Some(other.unwrap()),
                }
            })
            .expect("mkgen stays busy");
        assert!(status.success());
        symlink(r.p("store/gen1"), r.p("profiles/system-1-link")).unwrap();
        symlink("system-1-link", r.p("profiles/system")).unwrap();
        symlink(r.p("store/gen1"), r.p("current")).unwrap();
        symlink(r.p("store/gen1"), r.p("booted")).unwrap();
        std::fs::write(
            r.p("policy/release.json"),
            r#"{"version":"0.1.0-test","dev":true}"#,
        )
        .unwrap();
        std::fs::write(
            r.p("ssh/ssh_host_ed25519_key.pub"),
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINraSens892Rb1jPRy00F9HKBgAjXCmwsXHmcTtlB7ep root@box\n",
        )
        .unwrap();
        r
    }
}

// ---------------------------------------------------------------------------------------------
// The service

pub struct System {
    pub child: Child,
    pub socket: PathBuf,
    pub root: Root,
    pub fakes: Shared,
    /// Audit entries, read continuously from the fake journal socket (its queue may be short).
    pub journal: Arc<Mutex<Vec<HashMap<String, String>>>>,
    journal_stop: Arc<std::sync::atomic::AtomicBool>,
    _conn: zbus::Connection,
    _bus: Bus,
}

impl System {
    /// Start the service against `root`, with the caller owning seat0 and polkit saying yes.
    pub async fn start(root: Root, extra: &[&str]) -> Option<Self> {
        let bus = Bus::start()?;
        let fakes: Shared = Arc::new(Mutex::new(FakeState {
            seat_user: Some(me()),
            ..Default::default()
        }));
        let path = format!(
            "{}:{}",
            root.p("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        fakes.lock().unwrap().job_cmd = Some((
            PathBuf::from(env!("CARGO_BIN_EXE_hinteros-system")),
            root.p("run"),
            vec![
                ("PATH".into(), path.clone()),
                ("FAKE_ROOT".into(), root.path().display().to_string()),
            ],
        ));
        let conn = serve_fakes(&bus.address, fakes.clone()).await;
        let sock = UnixDatagram::bind(root.p("journal.sock")).unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let journal: Arc<Mutex<Vec<HashMap<String, String>>>> = Arc::default();
        let journal_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let (journal, stop) = (journal.clone(), journal_stop.clone());
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 1 << 16];
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok(n) = sock.recv(&mut buf) {
                        let m = hinteros_system::audit::parse_datagram(&buf[..n]);
                        journal.lock().unwrap().push(m);
                    }
                }
            });
        }
        let socket = root.p("run/system.sock");
        let r = |p: &str| root.p(p).display().to_string();
        let mut args: Vec<String> = vec![
            "--socket".into(),
            socket.display().to_string(),
            "--config-dir".into(),
            r("etc/hinteros"),
            "--state-dir".into(),
            r("var/lib/hinteros"),
            "--run-dir".into(),
            r("run"),
            "--profile".into(),
            r("profiles/system"),
            "--current-system".into(),
            r("current"),
            "--booted-system".into(),
            r("booted"),
            "--policy-dir".into(),
            r("policy"),
            "--ssh-host-key-dir".into(),
            r("ssh"),
            "--bus".into(),
            bus.address.clone(),
            "--journal-socket".into(),
            r("journal.sock"),
            "--nix".into(),
            r("bin/nix"),
            "--nix-env".into(),
            r("bin/nix-env"),
            "--nixos-facter".into(),
            r("bin/nixos-facter"),
            "--min-memory-bytes".into(),
            "0".into(),
            "--min-disk-bytes".into(),
            "0".into(),
        ];
        // Generous rate limits unless the test sets its own (clap refuses a flag given twice).
        for (flag, value) in [("--rate-per-minute", "1000"), ("--rate-burst", "1000")] {
            if !extra.contains(&flag) {
                args.extend([flag.to_string(), value.to_string()]);
            }
        }
        args.extend(extra.iter().map(|s| s.to_string()));
        let child = Command::new(env!("CARGO_BIN_EXE_hinteros-system"))
            .args(&args)
            .env("PATH", path)
            .env("RUST_LOG", "debug")
            .stderr(std::fs::File::create(root.p("service.log")).unwrap())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + TIMEOUT;
        while std::os::unix::net::UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "service did not start listening");
            std::thread::sleep(Duration::from_millis(20));
        }
        Some(Self {
            child,
            socket,
            root,
            fakes,
            journal,
            journal_stop,
            _conn: conn,
            _bus: bus,
        })
    }

    pub async fn client(&self) -> Client {
        let mut c = Client::connect(&self.socket).await.unwrap();
        let hello = c
            .hello(system::VERSION, Peer::new("test", "0"))
            .await
            .unwrap();
        ApiSchema::system()
            .check_result("hello", &serde_json::to_value(&hello).unwrap())
            .unwrap();
        c
    }

    /// A client subscribed to the job events.
    pub async fn subscriber(&self) -> Client {
        let mut c = self.client().await;
        call(
            &mut c,
            "subscribe",
            json!({"events": ["job_started", "job_progress", "job_finished"]}),
        )
        .await
        .unwrap();
        c
    }

    /// Audit entries received so far (fields of each journal message).
    pub fn audit(&self) -> Vec<HashMap<String, String>> {
        std::thread::sleep(Duration::from_millis(100));
        std::mem::take(&mut *self.journal.lock().unwrap())
    }

    pub fn log(&self) -> String {
        self.root.read("service.log")
    }

    pub fn job_log(&self, id: &str) -> String {
        self.root.read(&format!("run/job-{id}.log"))
    }
}

impl Drop for System {
    fn drop(&mut self) {
        self.journal_stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
        let mut s = self.fakes.lock().unwrap();
        for c in s.children.iter_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Call a method; validate params (if valid), the result or error against the schema.
pub async fn call(c: &mut Client, method: &str, params: Value) -> Result<Value, Value> {
    let schema = ApiSchema::system();
    match c.call_raw(method, params).await {
        Ok(result) => {
            schema
                .check_result(method, &result)
                .unwrap_or_else(|e| panic!("{method}: {e}"));
            Ok(result)
        }
        Err(ClientError::Rpc(e)) => {
            let v = serde_json::to_value(&e).unwrap();
            schema
                .check_error(&v)
                .unwrap_or_else(|err| panic!("{method}: {err}"));
            Err(v)
        }
        Err(e) => panic!("{method}: {e:?}"),
    }
}

pub fn code(e: &Value) -> i64 {
    e["code"].as_i64().unwrap()
}

/// The next event, validated against the schema.
pub async fn next_event(c: &mut Client) -> (String, Value) {
    let (name, params) = timeout(TIMEOUT, c.next_event_raw())
        .await
        .expect("timed out waiting for an event")
        .unwrap();
    ApiSchema::system()
        .check_event(&name, &params)
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    (name, params)
}

/// Collect the events of job `id` until its `job_finished`; returns all of them.
pub async fn until_finished(c: &mut Client, id: &str) -> Vec<(String, Value)> {
    let mut seen = Vec::new();
    loop {
        let (name, params) = next_event(c).await;
        if params["job_id"] != id {
            continue;
        }
        let done = name == "job_finished";
        seen.push((name, params));
        if done {
            return seen;
        }
    }
}

pub fn finished(events: &[(String, Value)]) -> &Value {
    &events.last().unwrap().1
}

/// Poll until `f` is true.
pub async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// git

/// An upstream repo (bare) plus a work tree that commits to it, signing with SSH keys.
pub struct Upstream {
    pub bare: PathBuf,
    pub work: PathBuf,
    pub keys: PathBuf,
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@hinteros",
            "-c",
            "gpg.format=ssh",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

impl Upstream {
    pub fn create(root: &Root) -> Option<Self> {
        if tool_missing("git") || tool_missing("ssh-keygen") {
            return None;
        }
        let bare = root.p("upstream.git");
        let work = root.p("work");
        let keys = root.p("keys");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&keys).unwrap();
        for k in ["release", "rogue"] {
            let ok = Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", k, "-f"])
                .arg(keys.join(k))
                .status()
                .unwrap()
                .success();
            assert!(ok);
        }
        // Only `release` is an allowed signer.
        let pubkey = std::fs::read_to_string(keys.join("release.pub")).unwrap();
        let mut parts = pubkey.split_whitespace();
        let line = format!(
            "release@hinteros namespaces=\"git\" {} {}\n",
            parts.next().unwrap(),
            parts.next().unwrap()
        );
        std::fs::write(root.p("policy/update/allowed_signers"), line).unwrap();
        git(
            &root.p(""),
            &["init", "--bare", "--quiet", bare.to_str().unwrap()],
        );
        git(&work, &["init", "--quiet"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        std::fs::write(
            root.p("policy/update/channels.json"),
            json!({
                "repo": format!("file://{}", bare.display()),
                "branches": {"stable": "stable", "beta": "beta"}
            })
            .to_string(),
        )
        .unwrap();
        Some(Self { bare, work, keys })
    }

    /// Commit (signed by `key`, or unsigned) and return the commit id.
    pub fn commit(&self, message: &str, key: Option<&str>) -> String {
        let n = std::fs::read_dir(&self.work).unwrap().count();
        std::fs::write(self.work.join(format!("f{n}")), message).unwrap();
        git(&self.work, &["add", "-A"]);
        let signing = key.map(|k| format!("user.signingkey={}", self.keys.join(k).display()));
        let mut args: Vec<&str> = Vec::new();
        if let Some(s) = &signing {
            args.extend(["-c", s.as_str()]);
        }
        args.extend(["commit", "--quiet", "-m", message]);
        if key.is_some() {
            args.push("-S");
        }
        git(&self.work, &args);
        git(&self.work, &["rev-parse", "HEAD"])
    }

    pub fn push(&self, rev: &str, branch: &str) {
        git(
            &self.work,
            &[
                "push",
                "--quiet",
                "--force",
                "origin",
                &format!("{rev}:refs/heads/{branch}"),
            ],
        );
    }

    pub fn checkout(&self, rev: &str) {
        git(&self.work, &["checkout", "--quiet", "--detach", rev]);
    }
}

pub fn flake_nix(input_url: &str) -> String {
    format!(
        "{{\n  inputs.hinteros.url = \"{input_url}\";\n  outputs = {{ hinteros, ... }}: {{\n    nixosConfigurations.hinteros = hinteros.lib.mkDevice {{\n      state = builtins.fromJSON (builtins.readFile ./state.json);\n    }};\n  }};\n}}\n"
    )
}
