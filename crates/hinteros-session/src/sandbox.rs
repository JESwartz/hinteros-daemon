//! The app sandbox: catalog apps (not Steam, not `sandbox = false` apps) run in bwrap, set up
//! by the daemon at launch (`[sandbox] enable`, see `hinteros-docs/design/sandbox.md`).
//!
//! Spawn chain: reaper -> bwrap -> app (gamescope tags windows by the outermost reaper).
//!
//! - Namespaces: all new except the network (`--unshare-all --share-net`), `--die-with-parent`,
//!   `--new-session`. User namespaces stay allowed inside (Firefox and Electron sandboxes).
//! - Files: read-only `/nix/store`, `/run/current-system`, `/run/opengl-driver{,-32}`, `/etc`,
//!   `/sys`, `/run/systemd/resolve` and `/run/nscd` if present, `/bin` and `/usr` if present
//!   (`/bin/sh`, `/usr/bin/env`), the profile's app env (`/run/hinteros/apps/<user>`, resolved).
//!   A new `/proc`, a minimal `/dev` with `/dev/dri`, empty `/tmp` and `/run`.
//! - Home: `~/.local/share/hinteros/sandbox/<id>` (0700), read-write at the real `$HOME` path.
//!   The app's data starts fresh there; its Firefox kiosk profile lands inside it.
//! - Runtime dir: a new `$XDG_RUNTIME_DIR` with only `pulse/native` (`[sandbox] pulse_socket`,
//!   or `pulse_record_socket` with `mic`) and, for kiosk apps (web apps and keyboard-profile
//!   apps), `hinteros/osk.sock` (text field reports from Firefox's native-messaging host).
//! - Display: only `/tmp/.X11-unix/X<n>` of the app's display; `DISPLAY` only.
//! - Permissions ([`crate::permissions`]): `gamepad` = `/dev/input` and `/run/udev`; `mic` = the
//!   record-capable sound socket; `downloads` = `~/Downloads`, read-write.
//! - Hidden by construction (never bound): `/run/hinteros` (but the app env),
//!   `$XDG_RUNTIME_DIR/hinteros` (but `osk.sock`), `session.sock`, `bus`, `pipewire-0`,
//!   `gamescope-0`, `/run/dbus`, `/dev/uinput`, `/dev/input` without `gamepad`, the real home.
//! - Environment: no Wayland displays, no D-Bus addresses, no other sockets.
//!
//! [`args`] is a pure function of [`Host`] and [`Spec`]; [`prepare`] does the file system work
//! before the spawn (the sandbox home, `~/Downloads`). A launch fails when bwrap is missing; it
//! never runs the app without the sandbox.

use std::io;
use std::path::{Path, PathBuf};

use crate::permissions::Grants;

/// Where the sandbox homes are, relative to the profile's home.
pub const HOMES_DIR: &str = ".local/share/hinteros/sandbox";

/// Where an app's sound socket is inside, relative to `$XDG_RUNTIME_DIR`.
pub const PULSE_INSIDE: &str = "pulse/native";

/// The variable naming the text field socket (see [`crate::textfocus`]).
pub const OSK_SOCKET_ENV: &str = crate::textfocus::SOCKET_ENV;

/// Variables an app never gets: gamescope's Wayland displays, the D-Bus buses, other sockets.
pub const UNSET_ENV: &[&str] = &[
    "WAYLAND_DISPLAY",
    "GAMESCOPE_WAYLAND_DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
    "DBUS_SYSTEM_BUS_ADDRESS",
    "PULSE_SERVER",
    "PIPEWIRE_REMOTE",
    "SSH_AUTH_SOCK",
    "NOTIFY_SOCKET",
];

/// The sandbox home of app `id` in the profile's `home`.
pub fn home_of(home: &Path, id: &str) -> PathBuf {
    home.join(HOMES_DIR).join(id)
}

/// A path under the real `home` as the app sees it from outside: the same place in its
/// sandbox home (`firefox_profile = "~/.local/share/hinteros/firefox/x"` is in the sandbox
/// home). Paths outside `home` are unchanged.
pub fn outside_path(path: &Path, home: &Path, id: &str) -> PathBuf {
    match path.strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() => home_of(home, id).join(rest),
        _ => path.to_path_buf(),
    }
}

/// What this box gives every sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// The bwrap binary (resolved).
    pub bwrap: PathBuf,
    /// The profile's home (`$HOME`).
    pub home: PathBuf,
    /// `$XDG_RUNTIME_DIR`.
    pub runtime_dir: PathBuf,
    /// The X display apps use (`:<n>`, resolved).
    pub display: String,
    /// The profile's app env: the link (`/run/hinteros/apps/<user>`) and what it points to.
    pub app_env: Option<(PathBuf, PathBuf)>,
    /// The sound socket, relative to `runtime_dir`.
    pub pulse_socket: PathBuf,
    /// The sound socket that may record (`mic`), relative to `runtime_dir`.
    pub pulse_record_socket: PathBuf,
    /// The text field socket (absolute), for kiosk apps.
    pub osk_socket: Option<PathBuf>,
}

/// One app's sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// The app id (names its sandbox home).
    pub id: String,
    pub grants: Grants,
    /// A web or keyboard-profile app: it gets the text field socket.
    pub kiosk: bool,
}

/// The display number of an X display name (`:1`, `:1.0`).
pub fn display_number(display: &str) -> Option<u32> {
    let n = display.strip_prefix(':')?;
    let n = n.split_once('.').map_or(n, |(n, _)| n);
    n.parse().ok()
}

/// The full command line: bwrap, its arguments, `--`, then `program` and `args`.
pub fn args(
    host: &Host,
    spec: &Spec,
    program: &Path,
    args: &[String],
) -> Result<Vec<String>, String> {
    let n = display_number(&host.display)
        .ok_or_else(|| format!("display {:?} is not :<number>", host.display))?;
    if !crate::config::valid_app_id(&spec.id) {
        return Err(format!("bad app id {:?}", spec.id));
    }
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let mut out: Vec<String> = vec![s(&host.bwrap)];
    let mut push = |items: &[&str]| out.extend(items.iter().map(|i| i.to_string()));

    push(&[
        "--unshare-all",
        "--share-net",
        "--die-with-parent",
        "--new-session",
    ]);

    // The system, read-only.
    for p in ["/nix/store", "/etc", "/sys"] {
        push(&["--ro-bind", p, p]);
    }
    for p in ["/bin", "/usr"] {
        push(&["--ro-bind-try", p, p]);
    }
    push(&["--proc", "/proc", "--dev", "/dev"]);
    push(&["--dev-bind-try", "/dev/dri", "/dev/dri"]);
    if spec.grants.gamepad {
        push(&["--dev-bind-try", "/dev/input", "/dev/input"]);
    }

    // The display: only this app's X socket.
    let x = format!("/tmp/.X11-unix/X{n}");
    push(&["--tmpfs", "/tmp", "--ro-bind", &x, &x]);

    // /run: empty, then what apps need from it.
    push(&["--tmpfs", "/run"]);
    push(&["--ro-bind", "/run/current-system", "/run/current-system"]);
    for p in [
        "/run/opengl-driver",
        "/run/opengl-driver-32",
        "/run/systemd/resolve",
        "/run/nscd",
    ] {
        push(&["--ro-bind-try", p, p]);
    }
    if spec.grants.gamepad {
        push(&["--ro-bind-try", "/run/udev", "/run/udev"]);
    }
    if let Some((link, resolved)) = &host.app_env {
        push(&["--ro-bind", &s(resolved), &s(link)]);
    }

    // A new runtime dir: the sound socket and, for kiosk apps, the text field socket.
    let rt = &host.runtime_dir;
    push(&["--perms", "0700", "--dir", &s(rt)]);
    let pulse = if spec.grants.mic {
        &host.pulse_record_socket
    } else {
        &host.pulse_socket
    };
    push(&[
        "--ro-bind-try",
        &s(&rt.join(pulse)),
        &s(&rt.join(PULSE_INSIDE)),
    ]);
    let osk = host.osk_socket.as_deref().filter(|_| spec.kiosk).map(s);
    if let Some(osk) = &osk {
        push(&["--ro-bind-try", osk, osk]);
    }

    // The app's own home at the real home's path.
    let home = s(&host.home);
    push(&["--bind", &s(&home_of(&host.home, &spec.id)), &home]);
    if spec.grants.downloads {
        let dl = s(&host.home.join("Downloads"));
        push(&["--bind", &dl, &dl]);
    }

    // Environment.
    for k in UNSET_ENV {
        push(&["--unsetenv", k]);
    }
    match &osk {
        Some(osk) => push(&["--setenv", OSK_SOCKET_ENV, osk]),
        None => push(&["--unsetenv", OSK_SOCKET_ENV]),
    }
    push(&["--setenv", "DISPLAY", &format!(":{n}")]);
    push(&["--setenv", "HOME", &home]);
    push(&["--setenv", "XDG_RUNTIME_DIR", &s(rt)]);
    push(&["--chdir", &home]);

    push(&["--", &s(program)]);
    out.extend(args.iter().cloned());
    Ok(out)
}

/// Before the spawn: the app's sandbox home (0700, also when it exists) and, with
/// `downloads`, `~/Downloads`.
pub fn prepare(home: &Path, spec: &Spec) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = home_of(home, &spec.id);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    if spec.grants.downloads {
        std::fs::create_dir_all(home.join("Downloads"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host {
            bwrap: "/nix/store/b-bubblewrap/bin/bwrap".into(),
            home: "/home/tv".into(),
            runtime_dir: "/run/user/1000".into(),
            display: ":1".into(),
            app_env: Some((
                "/run/hinteros/apps/tv".into(),
                "/nix/store/e-hinteros-apps-tv".into(),
            )),
            pulse_socket: "pulse/play".into(),
            pulse_record_socket: "pulse/native".into(),
            osk_socket: Some("/run/user/1000/hinteros/osk.sock".into()),
        }
    }

    fn spec(grants: Grants, kiosk: bool) -> Spec {
        Spec {
            id: "youtube".into(),
            grants,
            kiosk,
        }
    }

    fn build(spec: &Spec) -> Vec<String> {
        args(
            &host(),
            spec,
            Path::new("/nix/store/f-firefox/bin/firefox"),
            &["--kiosk".into(), "https://www.youtube.com/tv".into()],
        )
        .unwrap()
    }

    /// The bwrap options as (option, operands) groups, before `--`.
    fn groups(argv: &[String]) -> Vec<Vec<String>> {
        let operands = |op: &str| match op {
            "--ro-bind" | "--ro-bind-try" | "--bind" | "--bind-try" | "--dev-bind"
            | "--dev-bind-try" | "--setenv" => 2,
            "--proc" | "--dev" | "--tmpfs" | "--dir" | "--perms" | "--unsetenv" | "--chdir" => 1,
            _ => 0,
        };
        let mut out = Vec::new();
        let mut i = 1;
        while i < argv.len() && argv[i] != "--" {
            let n = operands(&argv[i]);
            out.push(argv[i..=i + n].to_vec());
            i += n + 1;
        }
        out
    }

    fn has(argv: &[String], group: &[&str]) -> bool {
        groups(argv).iter().any(|g| g == group)
    }

    /// Every path that ends up visible inside (mount targets).
    fn targets(argv: &[String]) -> Vec<String> {
        groups(argv)
            .into_iter()
            .filter_map(|g| match g[0].as_str() {
                "--ro-bind" | "--ro-bind-try" | "--bind" | "--bind-try" | "--dev-bind"
                | "--dev-bind-try" => Some(g[2].clone()),
                "--proc" | "--dev" | "--tmpfs" | "--dir" => Some(g[1].clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn namespaces_system_and_command() {
        let argv = build(&spec(Grants::default(), false));
        assert_eq!(argv[0], "/nix/store/b-bubblewrap/bin/bwrap");
        assert_eq!(
            argv[1..5],
            [
                "--unshare-all",
                "--share-net",
                "--die-with-parent",
                "--new-session"
            ]
        );
        assert!(
            !argv.iter().any(|a| a == "--disable-userns"),
            "nested sandboxes"
        );
        for p in ["/nix/store", "/etc", "/sys", "/run/current-system"] {
            assert!(has(&argv, &["--ro-bind", p, p]), "{p}");
        }
        for p in [
            "/run/opengl-driver",
            "/run/opengl-driver-32",
            "/run/systemd/resolve",
            "/run/nscd",
        ] {
            assert!(has(&argv, &["--ro-bind-try", p, p]), "{p}");
        }
        assert!(has(&argv, &["--proc", "/proc"]));
        assert!(has(&argv, &["--dev", "/dev"]));
        assert!(has(&argv, &["--dev-bind-try", "/dev/dri", "/dev/dri"]));
        assert!(has(&argv, &["--tmpfs", "/tmp"]));
        assert!(has(&argv, &["--tmpfs", "/run"]));
        assert!(has(
            &argv,
            &[
                "--ro-bind",
                "/nix/store/e-hinteros-apps-tv",
                "/run/hinteros/apps/tv"
            ]
        ));
        let dash = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            argv[dash + 1..],
            [
                "/nix/store/f-firefox/bin/firefox",
                "--kiosk",
                "https://www.youtube.com/tv"
            ]
        );
        // Mounts under /run and /tmp come after their tmpfs (else the tmpfs hides them).
        let ts = targets(&argv);
        let at = |p: &str| ts.iter().position(|t| t == p).unwrap();
        for t in &ts {
            for base in ["/run", "/tmp"] {
                if t.starts_with(&format!("{base}/")) {
                    assert!(at(t) > at(base), "{t} before {base}");
                }
            }
        }
    }

    #[test]
    fn hidden_paths_are_never_mounted() {
        let all = Grants {
            gamepad: true,
            mic: true,
            downloads: true,
            lan: true,
        };
        for s in [spec(Grants::default(), false), spec(all, true)] {
            let argv = build(&s);
            for t in targets(&argv) {
                for hidden in [
                    "/run/dbus",
                    "/run/user/1000/bus",
                    "/run/user/1000/pipewire-0",
                    "/run/user/1000/gamescope-0",
                    "/run/user/1000/hinteros/session.sock",
                    "/dev/uinput",
                    "/tmp/.X11-unix/X0",
                ] {
                    assert_ne!(t, hidden);
                }
                assert!(
                    !t.starts_with("/run/hinteros") || t == "/run/hinteros/apps/tv",
                    "{t}"
                );
                assert!(
                    !t.starts_with("/run/user/1000/hinteros")
                        || t == "/run/user/1000/hinteros/osk.sock",
                    "{t}"
                );
                // The real home only as the app's own sandbox home (and Downloads).
                assert!(
                    !t.starts_with("/home") || t == "/home/tv" || t == "/home/tv/Downloads",
                    "{t}"
                );
            }
            // The real home is never a source either, but for Downloads.
            for g in groups(&argv) {
                if g[0].contains("bind") && g[1].starts_with("/home/tv") {
                    assert!(
                        g[1].starts_with("/home/tv/.local/share/hinteros/sandbox/youtube")
                            || g[1] == "/home/tv/Downloads",
                        "{g:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn home_display_and_runtime_dir() {
        let argv = build(&spec(Grants::default(), false));
        assert!(has(
            &argv,
            &[
                "--bind",
                "/home/tv/.local/share/hinteros/sandbox/youtube",
                "/home/tv"
            ]
        ));
        assert!(has(
            &argv,
            &["--ro-bind", "/tmp/.X11-unix/X1", "/tmp/.X11-unix/X1"]
        ));
        assert!(has(&argv, &["--perms", "0700"]));
        assert!(has(&argv, &["--dir", "/run/user/1000"]));
        assert!(has(&argv, &["--chdir", "/home/tv"]));
        // Screen numbers are fine; anything else is an error.
        let mut h = host();
        h.display = ":3.0".into();
        let argv = args(&h, &spec(Grants::default(), false), Path::new("/x"), &[]).unwrap();
        assert!(has(
            &argv,
            &["--ro-bind", "/tmp/.X11-unix/X3", "/tmp/.X11-unix/X3"]
        ));
        assert!(has(&argv, &["--setenv", "DISPLAY", ":3"]));
        for bad in ["auto", "", "host:0", ":x"] {
            h.display = bad.into();
            assert!(
                args(&h, &spec(Grants::default(), false), Path::new("/x"), &[]).is_err(),
                "{bad}"
            );
        }
        let bad_id = Spec {
            id: "../x".into(),
            ..spec(Grants::default(), false)
        };
        assert!(args(&host(), &bad_id, Path::new("/x"), &[]).is_err());
    }

    #[test]
    fn environment() {
        let argv = build(&spec(Grants::default(), false));
        for k in [
            "WAYLAND_DISPLAY",
            "GAMESCOPE_WAYLAND_DISPLAY",
            "DBUS_SESSION_BUS_ADDRESS",
            "DBUS_SYSTEM_BUS_ADDRESS",
            "HINTEROS_OSK_SOCKET",
        ] {
            assert!(has(&argv, &["--unsetenv", k]), "{k}");
        }
        assert!(has(&argv, &["--setenv", "DISPLAY", ":1"]));
        assert!(has(&argv, &["--setenv", "HOME", "/home/tv"]));
        assert!(has(
            &argv,
            &["--setenv", "XDG_RUNTIME_DIR", "/run/user/1000"]
        ));
        let set: Vec<String> = groups(&argv)
            .into_iter()
            .filter(|g| g[0] == "--setenv")
            .map(|g| g[1].clone())
            .collect();
        assert_eq!(set, ["DISPLAY", "HOME", "XDG_RUNTIME_DIR"]);
    }

    #[test]
    fn kiosk_apps_get_the_text_field_socket() {
        let osk = "/run/user/1000/hinteros/osk.sock";
        let argv = build(&spec(Grants::default(), true));
        assert!(has(&argv, &["--ro-bind-try", osk, osk]));
        assert!(has(&argv, &["--setenv", "HINTEROS_OSK_SOCKET", osk]));
        assert!(!has(&argv, &["--unsetenv", "HINTEROS_OSK_SOCKET"]));
        let argv = build(&spec(Grants::default(), false));
        assert!(!targets(&argv).iter().any(|t| t == osk));
        // No socket configured: nothing to bind.
        let mut h = host();
        h.osk_socket = None;
        let argv = args(&h, &spec(Grants::default(), true), Path::new("/x"), &[]).unwrap();
        assert!(!targets(&argv).iter().any(|t| t == osk));
        assert!(has(&argv, &["--unsetenv", "HINTEROS_OSK_SOCKET"]));
    }

    #[test]
    fn each_permission_on_and_off() {
        let off = build(&spec(Grants::default(), false));
        // Sound: the play socket at pulse/native; mic: the record one.
        assert!(has(
            &off,
            &[
                "--ro-bind-try",
                "/run/user/1000/pulse/play",
                "/run/user/1000/pulse/native"
            ]
        ));
        let mic = build(&spec(
            Grants {
                mic: true,
                ..Grants::default()
            },
            false,
        ));
        assert!(has(
            &mic,
            &[
                "--ro-bind-try",
                "/run/user/1000/pulse/native",
                "/run/user/1000/pulse/native"
            ]
        ));
        assert!(!targets(&mic).iter().any(|t| t == "/dev/input"));

        assert!(!targets(&off).iter().any(|t| t == "/dev/input"));
        assert!(!targets(&off).iter().any(|t| t == "/run/udev"));
        let pad = build(&spec(
            Grants {
                gamepad: true,
                ..Grants::default()
            },
            false,
        ));
        assert!(has(&pad, &["--dev-bind-try", "/dev/input", "/dev/input"]));
        assert!(has(&pad, &["--ro-bind-try", "/run/udev", "/run/udev"]));
        assert!(!targets(&pad).iter().any(|t| t == "/home/tv/Downloads"));

        assert!(!targets(&off).iter().any(|t| t == "/home/tv/Downloads"));
        let dl = build(&spec(
            Grants {
                downloads: true,
                ..Grants::default()
            },
            false,
        ));
        assert!(has(
            &dl,
            &["--bind", "/home/tv/Downloads", "/home/tv/Downloads"]
        ));
        // Downloads is mounted inside the sandbox home, so after it.
        let ts = targets(&dl);
        let at = |p: &str| ts.iter().position(|t| t == p).unwrap();
        assert!(at("/home/tv/Downloads") > at("/home/tv"));
        assert!(!targets(&dl).iter().any(|t| t == "/dev/input"));
    }

    #[test]
    fn paths_outside_and_prepare() {
        assert_eq!(
            outside_path(
                Path::new("/home/tv/.local/share/hinteros/firefox/youtube"),
                Path::new("/home/tv"),
                "youtube"
            ),
            Path::new(
                "/home/tv/.local/share/hinteros/sandbox/youtube/.local/share/hinteros/firefox/youtube"
            )
        );
        assert_eq!(
            outside_path(Path::new("/srv/x"), Path::new("/home/tv"), "a"),
            Path::new("/srv/x")
        );
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let s = Spec {
            id: "a".into(),
            grants: Grants {
                downloads: true,
                ..Grants::default()
            },
            kiosk: false,
        };
        let dir = home_of(t.path(), "a");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        prepare(t.path(), &s).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(t.path().join("Downloads").is_dir());
        prepare(t.path(), &s).unwrap();
    }
}
