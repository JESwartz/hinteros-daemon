# HinterOS Daemon

The Rust system services behind the launcher, built as two binaries:

- **Session daemon** (unprivileged, runs in the user's session): controller input and the
  global Home button, launching and focusing apps in gamescope.
- **Privileged service** (small, narrow API): applies settings from `state.json`, rebuilds,
  updates, rollback and power. It authenticates callers (`SO_PEERCRED` + polkit).

This repo owns the versioned socket API schema that the launcher (and future clients such as
a phone web admin UI) are generated from.

Part of [HinterOS](https://justsomeshed.com/JSwartz/HinterOS/hinteros). Licensed under GPL-3.0-only, see [LICENSE](LICENSE).
