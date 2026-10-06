# `nix run .#desktop-preview`: the session daemon on a desktop, for trying the launcher.
{
  lib,
  writeShellApplication,
  coreutils,
  hinteros-session,
}:
writeShellApplication {
  name = "hinteros-desktop-preview";
  runtimeInputs = [ coreutils ];
  text = ''
    # Development only. Starts hinteros-session without gamescope, InputPlumber and uinput,
    # reading gamepads directly (--dev-gamepad, never grabbed), on a private socket.
    # Launching an app or game from the launcher STARTS IT FOR REAL on this desktop.
    runtime="''${XDG_RUNTIME_DIR:-/tmp}"
    dir="$(mktemp -d "$runtime/hinteros-preview.XXXXXX")"
    trap 'rm -rf "$dir"' EXIT
    socket="$dir/session.sock"

    args=(--socket "$socket" --no-gamescope --no-inputplumber --no-uinput --dev-gamepad)

    # The user's real Steam library, if there is one (the daemon only reads it).
    for steam in "''${HINTEROS_STEAM_DIR:-}" "$HOME/.local/share/Steam" "$HOME/.steam/steam"; do
      if [ -n "$steam" ] && [ -d "$steam/steamapps" ]; then
        args+=(--steam-dir "$steam")
        echo "Steam library: $steam (read-only)"
        break
      fi
    done

    # Sample apps (icons come from your installed desktop files and from the sites).
    # Use your own with HINTEROS_SESSION_CONFIG=<file>.
    if [ -z "''${HINTEROS_SESSION_CONFIG:-}" ]; then
      cat > "$dir/session.toml" <<'TOML'
    [[apps]]
    id = "jellyfin"
    name = "Jellyfin"
    command = ["jellyfin-desktop"]
    desktop_id = "org.jellyfin.JellyfinDesktop"

    [[apps]]
    id = "firefox"
    name = "Firefox"
    command = ["firefox"]
    desktop_id = "firefox"

    [[apps]]
    id = "youtube"
    name = "YouTube"
    kind = "web"
    command = ["firefox", "--new-window", "https://www.youtube.com/tv"]
    site_url = "https://www.youtube.com/"

    [[apps]]
    id = "crunchyroll"
    name = "Crunchyroll"
    kind = "web"
    command = ["firefox", "--new-window", "https://www.crunchyroll.com/"]
    site_url = "https://www.crunchyroll.com/"

    [[apps]]
    id = "plex"
    name = "Plex"
    command = ["plex-htpc"]
    desktop_id = "tv.plex.PlexHTPC"

    [[apps]]
    id = "vacuumtube"
    name = "VacuumTube"
    command = ["vacuumtube"]
    desktop_id = "rocks.shy.VacuumTube"
    TOML
      args+=(--config "$dir/session.toml")
    fi

    echo
    echo "hinteros-session is listening on:"
    echo "  $socket"
    echo
    echo "Start the launcher against it, in another terminal:"
    echo "  HINTEROS_SESSION_SOCKET=$socket nix run <launcher flake> -- --windowed"
    echo
    echo "Launching apps/games from the launcher starts them for real on this desktop."
    echo "Ctrl-C stops the preview."
    echo

    ${lib.getExe hinteros-session} "''${args[@]}" "$@"
  '';
}
