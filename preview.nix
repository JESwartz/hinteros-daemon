# `nix run .#desktop-preview`: the session daemon on a desktop, for trying the launcher.
{
  lib,
  writeShellApplication,
  coreutils,
  firefox,
  jellyfin-desktop,
  vacuum-tube,
  hinteros-session,
}:
let
  # The YouTube profile pretends to be a TV so youtube.com/tv serves its TV UI
  # (same idea as the system repo's kiosk profile).
  tvUserAgent = "Mozilla/5.0 (PlayStation; PlayStation 5/2.26) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/13.0 Safari/605.1.15";
in
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

    args=(--socket "$socket" --no-gamescope --no-inputplumber --no-uinput --dev-gamepad --no-auth-agent)

    # The user's real Steam library, if there is one (the daemon only reads it).
    for steam in "''${HINTEROS_STEAM_DIR:-}" "$HOME/.local/share/Steam" "$HOME/.steam/steam"; do
      if [ -n "$steam" ] && [ -d "$steam/steamapps" ]; then
        args+=(--steam-dir "$steam")
        echo "Steam library: $steam (read-only)"
        break
      fi
    done

    # Sample apps, run from nixpkgs so the preview doesn't depend on what the desktop has
    # installed. Firefox uses its own profiles under ~/.local/share/hinteros-preview, never
    # the desktop's browser profile. Icons come from these packages' desktop files and from
    # the sites. Plex is left out (unfree). Use your own config with HINTEROS_SESSION_CONFIG.
    if [ -z "''${HINTEROS_SESSION_CONFIG:-}" ]; then
      profiles="$HOME/.local/share/hinteros-preview/firefox"
      for name in firefox youtube crunchyroll; do
        mkdir -p "$profiles/$name"
      done
      printf 'user_pref("general.useragent.override", "%s");\n' '${tvUserAgent}' \
        > "$profiles/youtube/user.js"
      export XDG_DATA_DIRS="${firefox}/share:${jellyfin-desktop}/share:${vacuum-tube}/share''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"

      cat > "$dir/session.toml" <<TOML
    [[apps]]
    id = "jellyfin"
    name = "Jellyfin"
    command = ["${lib.getExe jellyfin-desktop}"]
    desktop_id = "org.jellyfin.JellyfinDesktop"

    [[apps]]
    id = "firefox"
    name = "Firefox"
    command = ["${lib.getExe firefox}", "--no-remote", "--profile", "$profiles/firefox"]
    desktop_id = "firefox"

    [[apps]]
    id = "youtube"
    name = "YouTube"
    kind = "web"
    command = ["${lib.getExe firefox}", "--no-remote", "--profile", "$profiles/youtube", "--kiosk", "https://www.youtube.com/tv"]
    site_url = "https://www.youtube.com/"
    firefox_profile = "$profiles/youtube"

    [[apps]]
    id = "crunchyroll"
    name = "Crunchyroll"
    kind = "web"
    command = ["${lib.getExe firefox}", "--no-remote", "--profile", "$profiles/crunchyroll", "--kiosk", "https://www.crunchyroll.com/"]
    site_url = "https://www.crunchyroll.com/"
    firefox_profile = "$profiles/crunchyroll"

    [[apps]]
    id = "vacuumtube"
    name = "VacuumTube"
    command = ["${lib.getExe vacuum-tube}"]
    desktop_id = "VacuumTube"
    TOML
      args+=(--config "$dir/session.toml")
    fi

    echo
    echo "hinteros-session is listening on:"
    echo "  $socket"
    echo
    echo "Start the launcher against it, in another terminal:"
    echo "  HINTEROS_SESSION_SOCKET=$socket nix run git+https://justsomeshed.com/JSwartz/HinterOS/hinteros-launcher.git -- --windowed"
    echo
    echo "Launching apps/games from the launcher starts them for real on this desktop."
    echo "Kiosk windows (YouTube, Crunchyroll) close with Alt+F4 or the launcher's Close (Y)."
    echo "Ctrl-C stops the preview."
    echo

    ${lib.getExe hinteros-session} "''${args[@]}" "$@"
  '';
}
