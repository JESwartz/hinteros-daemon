//! `hinteros-system pin <verb>`: the PAM helper greetd's PAM stack runs through `pam_exec`
//! (profiles.md "PAM: where the PIN and the kill are enforced"). It runs as root with an empty
//! `PATH`, reads `PAM_USER`, and prints only its status lines.
//!
//! - `unlocked`: 0 when the user may enter without a PIN (an unlocked profile, a profile without
//!   a PIN yet, the guest, a valid one-time token, a spare while no admin profile exists).
//! - `verify`: the PIN from stdin, with the shared backoff. Wrong: `hinteros:wrong`; locked:
//!   `hinteros:locked:<seconds>`.
//! - `end-others`: ends every other `hinteros` account's processes before a session opens.
//! - `splash`: paints "Loading <name>…" on the session's VT (`PAM_TTY`) during the handover, on
//!   the framebuffer in the launcher's typeface (`--font`) or as console text (see
//!   [`crate::splash`]); always exits 0.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::config::Paths;
use crate::pins::{self, Pins, Verify};
use crate::profiles::ProfilesFile;
use crate::public;
use crate::state::{self, State};
use crate::users::Users;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Verb {
    Unlocked,
    Verify,
    EndOthers,
    Splash,
}

/// What the helper needs to know about the box.
pub struct Ctx {
    pub paths: Paths,
    pub users: Users,
    pub bus: Option<String>,
}

impl Ctx {
    fn state(&self) -> Option<State> {
        State::load(&self.paths.state_json()).ok().map(|(s, _)| s)
    }

    fn file(&self) -> ProfilesFile {
        ProfilesFile::load(&self.paths.profiles_json()).unwrap_or_default()
    }
}

/// `unlocked` for `user`.
pub fn unlocked(ctx: &Ctx, user: &str, now: i64) -> bool {
    let Some(state) = ctx.state() else {
        return false;
    };
    let file = ctx.file();
    if user == state::GUEST_USER {
        return state.guest_enabled();
    }
    let Some(account) = state.accounts().into_iter().find(|a| a.user == user) else {
        return false;
    };
    let pins = Pins::new(ctx.paths.pins_dir());
    match file.profiles.get(&account.id) {
        Some(meta) => {
            !meta.locked
                || !pins.is_set(&account.id)
                || public::take_token(&ctx.paths.switch_json(), user, now)
        }
        // A spare: only in first-boot mode.
        None => file.first_boot(&state),
    }
}

/// `verify` for `user` with `pin`: (exit success, status line).
pub fn verify(ctx: &Ctx, user: &str, pin: &str, now: i64) -> (bool, Option<String>) {
    let Some(state) = ctx.state() else {
        return (false, None);
    };
    let file = ctx.file();
    let Some(account) = state
        .accounts()
        .into_iter()
        .find(|a| a.user == user && file.profiles.contains_key(&a.id))
    else {
        return (false, None);
    };
    // The admin profile's PIN has no backoff (DECISIONS J4).
    let backoff = !file.admin(&state).is_some_and(|(a, _)| a.id == account.id);
    match Pins::new(ctx.paths.pins_dir()).verify_with(&account.id, pin, now, backoff) {
        Ok(Verify::Ok) => (true, None),
        Ok(Verify::Wrong { locked_for }) if locked_for > 0 => {
            (false, Some(format!("hinteros:locked:{locked_for}")))
        }
        Ok(Verify::Wrong { .. }) => (false, Some("hinteros:wrong".into())),
        Ok(Verify::Locked { seconds }) => (false, Some(format!("hinteros:locked:{seconds}"))),
        Ok(Verify::NoPin) => (false, None),
        Err(e) => {
            warn!("pin check: {e}");
            (false, None)
        }
    }
}

/// The accounts whose processes `end-others` stops when `entering` opens a session: every
/// member of `hinteros` and the guest, never `entering`, the admin account, root or a system
/// user (uid < 1000).
pub fn others(ctx: &Ctx, entering: &str) -> Vec<(String, u32)> {
    let admin = ctx.state().and_then(|s| s.settings.admin.map(|a| a.user));
    let mut names = ctx.users.group_members("hinteros");
    names.push(state::GUEST_USER.into());
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter(|n| n != entering && Some(n) != admin.as_ref())
        .filter_map(|n| {
            let u = ctx.users.by_name(&n)?;
            (u.uid >= 1000).then_some((n, u.uid))
        })
        .collect()
}

/// Processes of `uid` (real or effective), from `/proc`.
pub fn processes_of(proc_dir: &Path, uid: u32) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir(proc_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let pid: i32 = e.file_name().to_str()?.parse().ok()?;
            let status = std::fs::read_to_string(e.path().join("status")).ok()?;
            let line = status.lines().find(|l| l.starts_with("Uid:"))?;
            let mut f = line.split_whitespace().skip(1);
            let real: u32 = f.next()?.parse().ok()?;
            let eff: u32 = f.next()?.parse().ok()?;
            (real == uid || eff == uid).then_some(pid)
        })
        .collect()
}

async fn end_user(conn: &zbus::Connection, name: &str, uid: u32) {
    if let Err(e) = conn
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "TerminateUser",
            &(uid,),
        )
        .await
    {
        info!(user = name, "TerminateUser: {e}");
    }
    // A bounded wait for the user manager to stop.
    let unit = format!("user@{uid}.service");
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let active = conn
            .call_method(
                Some("org.freedesktop.systemd1"),
                "/org/freedesktop/systemd1",
                Some("org.freedesktop.systemd1.Manager"),
                "GetUnit",
                &(unit.as_str(),),
            )
            .await
            .is_ok();
        if !active {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let slice = format!("user-{uid}.slice");
    if let Err(e) = conn
        .call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "KillUnit",
            &(slice.as_str(), "all", 9i32),
        )
        .await
    {
        info!(user = name, "KillUnit {slice}: {e}");
    }
}

/// `end-others`: logind and systemd first (they own the sessions and the slice), then SIGKILL
/// for anything of those uids that is left (system-level units running as them).
pub fn end_others(ctx: &Ctx, entering: &str, proc_dir: &Path) {
    end_list(ctx, others(ctx, entering), proc_dir);
}

/// End one account's processes (a profile being removed).
pub fn end_uid(ctx: &Ctx, name: &str, uid: u32, proc_dir: &Path) {
    end_list(ctx, vec![(name.to_owned(), uid)], proc_dir);
}

fn end_list(ctx: &Ctx, targets: Vec<(String, u32)>, proc_dir: &Path) {
    let alive: Vec<(String, u32)> = targets
        .into_iter()
        .filter(|(_, uid)| *uid >= 1000 && !processes_of(proc_dir, *uid).is_empty())
        .collect();
    if alive.is_empty() {
        return;
    }
    let bus = ctx.bus.clone();
    let list = alive.clone();
    let done = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map(|rt| {
            rt.block_on(async move {
                let conn = match &bus {
                    Some(a) => match zbus::connection::Builder::address(a.as_str()) {
                        Ok(b) => b.build().await,
                        Err(e) => Err(e),
                    },
                    None => zbus::Connection::system().await,
                };
                match conn {
                    Ok(conn) => {
                        for (name, uid) in &list {
                            end_user(&conn, name, *uid).await;
                        }
                    }
                    Err(e) => warn!("no system bus: {e}"),
                }
            })
        });
    if let Err(e) = done {
        warn!("end-others: {e}");
    }
    for (name, uid) in &alive {
        let pids = processes_of(proc_dir, *uid);
        for pid in &pids {
            if let Some(pid) = rustix::process::Pid::from_raw(*pid) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
        if !pids.is_empty() {
            info!(user = %name, uid, killed = pids.len(), "ended a profile's leftover processes");
        }
    }
}

/// Xwayland's leftovers from a session that was ended: a `<tmp>/.X<n>-lock` whose pid is gone,
/// and its `<tmp>/.X11-unix/X<n>` socket. gamescope takes the first free display numbers, and
/// the session daemon (a user service, so not gamescope's child) expects `:0`: a stale `:0`
/// leaves the next session's home app and keyboard without a screen.
pub fn clear_stale_x(tmp: &Path, proc_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(n) = name
            .to_str()
            .and_then(|s| s.strip_prefix(".X"))
            .and_then(|s| s.strip_suffix("-lock"))
        else {
            continue;
        };
        if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if !std::fs::symlink_metadata(e.path()).is_ok_and(|m| m.file_type().is_file()) {
            continue;
        }
        // The pid, space-padded and NUL-terminated (wlroots). Unreadable: leave it alone.
        let Some(pid) = std::fs::read_to_string(e.path()).ok().and_then(|s| {
            s.trim_matches(|c: char| c.is_whitespace() || c == '\0')
                .parse::<u32>()
                .ok()
        }) else {
            continue;
        };
        if proc_dir.join(pid.to_string()).exists() {
            continue;
        }
        // remove_file never follows a symlink.
        let _ = std::fs::remove_file(tmp.join(".X11-unix").join(format!("X{n}")));
        let _ = std::fs::remove_file(e.path());
        info!(display = n, "removed a display left by an ended session");
    }
}

/// The switch hint has done its job once its target's session opens: drop it (and whatever is
/// left of its token), so a session that ends soon after (End guest session, a crash) goes to
/// the picker instead of straight back in.
pub fn fulfil_hint(ctx: &Ctx, user: &str, now: i64) {
    let path = ctx.paths.switch_json();
    let Some(hint) = public::SwitchHint::load(&path) else {
        return;
    };
    let Some(state) = ctx.state() else { return };
    let target_user = if hint.target == "guest" {
        Some(state::GUEST_USER.to_owned())
    } else {
        state
            .accounts()
            .into_iter()
            .find(|a| a.id == hint.target)
            .map(|a| a.user)
    };
    if target_user.as_deref() != Some(user) {
        return;
    }
    if let Err(e) = std::fs::remove_file(&path) {
        warn!("cannot drop the switch hint: {e}");
        return;
    }
    crate::public::publish(&ctx.paths, &state, &ctx.file(), now);
}

/// Mark `user`'s profile as used now (the picker focuses the last-used profile).
fn touch_last_used(ctx: &Ctx, user: &str, now: i64) {
    let Some(state) = ctx.state() else { return };
    let Some(a) = state.accounts().into_iter().find(|a| a.user == user) else {
        return;
    };
    let path = ctx.paths.profiles_json();
    let Ok(mut file) = ProfilesFile::load(&path) else {
        return;
    };
    if let Some(m) = file.profiles.get_mut(&a.id) {
        m.last_used_at = Some(now);
        if let Err(e) = file.save(&path) {
            warn!("profiles.json: {e}");
        }
        crate::public::publish(&ctx.paths, &state, &file, now);
    }
}

pub fn main(verb: Verb, ctx: &Ctx, font: Option<&Path>) -> ExitCode {
    let user = std::env::var("PAM_USER").unwrap_or_default();
    let now = crate::job::now();
    if verb == Verb::Splash {
        // Cosmetic: never fails a login, whatever happens.
        let tty = std::env::var("PAM_TTY").ok();
        let paths = &ctx.paths;
        let _ = std::panic::catch_unwind(|| {
            crate::splash::run(paths, &user, tty.as_deref(), font, now)
        });
        return ExitCode::SUCCESS;
    }
    if user.is_empty() {
        return ExitCode::FAILURE;
    }
    match verb {
        Verb::Unlocked => {
            if unlocked(ctx, &user, now) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Verb::Verify => {
            let pin = pins::read_authtok(std::io::stdin().lock());
            let (ok, line) = verify(ctx, &user, &pin, now);
            if let Some(line) = line {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{line}");
                let _ = out.flush();
            }
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Verb::Splash => ExitCode::SUCCESS,
        Verb::EndOthers => {
            end_others(ctx, &user, Path::new("/proc"));
            clear_stale_x(Path::new("/tmp"), Path::new("/proc"));
            if user != "hinteros-greeter" {
                touch_last_used(ctx, &user, now);
                fulfil_hint(ctx, &user, now);
            }
            ExitCode::SUCCESS
        }
    }
}

/// Paths for the helper (the service's defaults unless overridden).
pub fn paths(state_dir: PathBuf, run_dir: PathBuf, config_dir: PathBuf, users: Users) -> Paths {
    Paths {
        config_dir,
        state_dir,
        run_dir,
        profile: "/nix/var/nix/profiles/system".into(),
        current_system: "/run/current-system".into(),
        booted_system: "/run/booted-system".into(),
        apps_profiles: "/nix/var/nix/profiles/hinteros".into(),
        catalog_index: None,
        users,
        proc_dir: "/proc".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::migrated_file;

    fn ctx(root: &Path) -> Ctx {
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::create_dir_all(root.join("run")).unwrap();
        std::fs::write(
            root.join("passwd"),
            "hinteros:x:1000:100::/h:/bin/sh\nhinteros-p2:x:20002:100::/h2:/bin/sh\n\
             hinteros-p3:x:20003:100::/h3:/bin/sh\nhinteros-guest:x:20000:100::/g:/bin/sh\n\
             admin:x:1001:100::/a:/bin/sh\nnobody:x:65534:65534::/:/bin/sh\nsys:x:900:900::/:/bin/sh\n",
        )
        .unwrap();
        std::fs::write(
            root.join("group"),
            "hinteros:x:990:hinteros,hinteros-p2,hinteros-p3,hinteros-guest,admin,sys\n",
        )
        .unwrap();
        let users = Users {
            passwd: Some(root.join("passwd")),
            group: Some(root.join("group")),
        };
        Ctx {
            paths: paths(
                root.join("var"),
                root.join("run"),
                root.join("etc"),
                users.clone(),
            ),
            users,
            bus: None,
        }
    }

    const STATE: &str = r#"{"schemaVersion":2,"requirements":[],"services":[],
      "settings":{"admin":{"user":"admin"}},
      "profiles":[{"id":"p1","user":"hinteros"},{"id":"p2","user":"hinteros-p2"},{"id":"p3","user":"hinteros-p3"}]}"#;

    #[test]
    fn verbs_against_a_temp_root() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        std::fs::write(d.path().join("etc/state.json"), STATE).unwrap();
        let state = State::parse(STATE.as_bytes()).unwrap();
        // No profiles.json: first-boot mode, every account is a spare and may enter.
        assert!(unlocked(&c, "hinteros-p3", 0));
        let mut file = migrated_file(&state, 0);
        file.profiles.remove("p3");
        file.profiles.get_mut("p2").unwrap().locked = true;
        file.profiles.get_mut("p2").unwrap().admin = false;
        file.save(&c.paths.profiles_json()).unwrap();
        let pins = Pins::new(c.paths.pins_dir());
        // p1 unlocked; p2 locked but no PIN yet → enters; with a PIN → asks.
        assert!(unlocked(&c, "hinteros", 0));
        assert!(unlocked(&c, "hinteros-p2", 0));
        pins.set("p2", "2468").unwrap();
        assert!(!unlocked(&c, "hinteros-p2", 0));
        // A spare outside first-boot mode, the admin account, root, unknown users: never.
        for u in ["hinteros-p3", "admin", "root", "nobody", "x"] {
            assert!(!unlocked(&c, u, 0), "{u}");
        }
        // The guest while enabled.
        assert!(unlocked(&c, "hinteros-guest", 0));
        // A token, once.
        public::grant_token(&c.paths, "p2", "hinteros-p2", 0).unwrap();
        assert!(unlocked(&c, "hinteros-p2", 1));
        assert!(!unlocked(&c, "hinteros-p2", 1));
        // The hint is dropped once its target's session opens (and only then).
        fulfil_hint(&c, "hinteros", 1);
        assert!(c.paths.switch_json().exists());
        fulfil_hint(&c, "hinteros-p2", 1);
        assert!(!c.paths.switch_json().exists());
        public::grant_token(&c.paths, "guest", "hinteros-guest", 0).unwrap();
        fulfil_hint(&c, "hinteros-guest", 1);
        assert!(!c.paths.switch_json().exists());

        // verify: right, wrong, then the lock message.
        assert_eq!(verify(&c, "hinteros-p2", "2468", 10), (true, None));
        for _ in 0..3 {
            assert_eq!(
                verify(&c, "hinteros-p2", "1111", 10),
                (false, Some("hinteros:wrong".into()))
            );
        }
        assert_eq!(
            verify(&c, "hinteros-p2", "1111", 10),
            (false, Some("hinteros:locked:1".into()))
        );
        assert_eq!(
            verify(&c, "hinteros-p2", "2468", 10),
            (false, Some("hinteros:locked:1".into()))
        );
        assert_eq!(verify(&c, "hinteros-p2", "2468", 12), (true, None));
        assert_eq!(verify(&c, "hinteros-p3", "2468", 12), (false, None));
        // The admin profile (p1): no backoff, however many wrong tries; a stale wait from
        // before is ignored.
        pins.set("p1", "1357").unwrap();
        for _ in 0..8 {
            let _ = pins.verify("p1", "0000", 12);
        }
        for _ in 0..8 {
            assert_eq!(
                verify(&c, "hinteros", "0000", 12),
                (false, Some("hinteros:wrong".into()))
            );
        }
        assert_eq!(verify(&c, "hinteros", "1357", 12), (true, None));
        assert_eq!(verify(&c, "admin", "2468", 12), (false, None));

        // end-others: the other members and the guest, never the admin account or system users.
        let o: Vec<String> = others(&c, "hinteros-p2")
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(o, ["hinteros", "hinteros-guest", "hinteros-p3"]);
    }

    #[test]
    fn finds_processes_by_uid() {
        let d = tempfile::tempdir().unwrap();
        for (pid, uid) in [(10, 1000), (11, 20002), (12, 0)] {
            std::fs::create_dir_all(d.path().join(pid.to_string())).unwrap();
            std::fs::write(
                d.path().join(format!("{pid}/status")),
                format!("Name:\tx\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
            )
            .unwrap();
        }
        std::fs::create_dir_all(d.path().join("self")).unwrap();
        assert_eq!(processes_of(d.path(), 20002), [11]);
        assert!(processes_of(d.path(), 5).is_empty());
    }

    #[test]
    fn clears_only_dead_displays() {
        let tmp = tempfile::tempdir().unwrap();
        let proc_dir = tempfile::tempdir().unwrap();
        let t = tmp.path();
        std::fs::create_dir_all(t.join(".X11-unix")).unwrap();
        std::fs::create_dir_all(proc_dir.path().join("777")).unwrap();
        // :0 and :1 from an ended session, :2 alive, :3 unreadable, :4 a symlink.
        for (n, pid) in [
            (0, "      1282\0"),
            (1, "      1283\n"),
            (2, "       777\0"),
            (3, "x"),
        ] {
            std::fs::write(t.join(format!(".X{n}-lock")), pid).unwrap();
            std::fs::write(t.join(format!(".X11-unix/X{n}")), "").unwrap();
        }
        std::fs::write(t.join("victim"), "").unwrap();
        std::os::unix::fs::symlink(t.join("victim"), t.join(".X4-lock")).unwrap();
        std::fs::write(t.join(".X11-unix/X4"), "").unwrap();

        clear_stale_x(t, proc_dir.path());

        let exists = |p: &str| t.join(p).exists();
        assert!(!exists(".X0-lock") && !exists(".X11-unix/X0"));
        assert!(!exists(".X1-lock") && !exists(".X11-unix/X1"));
        assert!(exists(".X2-lock") && exists(".X11-unix/X2"));
        assert!(exists(".X3-lock") && exists(".X11-unix/X3"));
        assert!(exists(".X4-lock") && exists(".X11-unix/X4") && exists("victim"));
    }
}
