# HinterOS Daemon

The Rust system services behind the launcher, built as two binaries:

- **`hinteros-session`** (unprivileged, runs in the user's session): controller input and the
  global Home button, launching and focusing apps in gamescope. Serves the launcher on
  `$XDG_RUNTIME_DIR/hinteros/session.sock`.
- **`hinteros-system`** (root, small, narrow API): applies settings from `state.json`, rebuilds,
  updates, rollback and power. Listens on `/run/hinteros/system.sock`, used only by the session
  daemon, and authenticates callers (`SO_PEERCRED` + polkit).

This repo owns the versioned socket API schema that the launcher (and future clients such as
a phone web admin UI) are generated from or contract-tested against.

## Layout

| Path | Contents |
|---|---|
| `schema/` | The API, as language-neutral JSON: `session-api.json` (launcher-facing), `system-api.json` (internal), and the meta-schema both follow |
| `crates/hinteros-api/` | Rust wire types, NDJSON framing, handshake, client; tests that check the types against `schema/` |
| `crates/hinteros-session/` | Session daemon |
| `crates/hinteros-system/` | Privileged system service |

The protocol is JSON-RPC 2.0, one message per line, starting with a `hello` version handshake.
The design and its reasoning live in `hinteros-docs/design/api.md`.

## Development

```sh
nix develop          # Rust toolchain, rust-analyzer, cargo-watch; installs git hooks
cargo watch -x test  # inner loop
nix flake check      # build, tests, clippy, rustfmt, nixfmt/statix/deadnix
nix build            # both binaries
```

Try it by hand: run `cargo run -p hinteros-session -- --socket /tmp/s.sock`, connect with
`socat - UNIX-CONNECT:/tmp/s.sock`, send
`{"jsonrpc":"2.0","id":1,"method":"hello","params":{"api_version":"0.1","client":{"name":"me","version":"0"}}}`,
and `kill -USR1` the daemon to emit `home_pressed` (after `subscribe`).

The systemd units live in the system repo, which consumes `overlays.default`.

Part of [HinterOS](https://justsomeshed.com/JSwartz/HinterOS/hinteros). Licensed under GPL-3.0-only, see [LICENSE](LICENSE).
