{
  description = "HinterOS daemon: session daemon, privileged system service and socket API schema";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      git-hooks,
    }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      inherit (pkgs) lib;

      # One version for the whole workspace (hello reports it as the build version).
      version = (lib.importTOML ./Cargo.toml).workspace.package.version;

      # Everything the Rust build needs: sources, manifests and the API schema
      # (the API crate embeds schema/*.json).
      src = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./rustfmt.toml
          ./crates
          ./schema
        ];
      };

      mkRust =
        pkgs:
        let
          craneLib = crane.mkLib pkgs;
          commonArgs = {
            inherit src;
            strictDeps = true;
            pname = "hinteros-daemon";
            inherit version;
          };
          # Dependencies are built once and shared by every package and check.
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          mkBin =
            name: extra:
            craneLib.buildPackage (
              commonArgs
              // extra
              // {
                inherit cargoArtifacts;
                pname = name;
                cargoExtraArgs = "--locked -p ${name}";
                # Tests run once for the whole workspace in checks.cargo-test.
                doCheck = false;
                meta = {
                  description = "HinterOS ${name}";
                  license = lib.licenses.gpl3Only;
                  mainProgram = name;
                  platforms = lib.platforms.linux;
                };
              }
            );
        in
        {
          inherit craneLib commonArgs cargoArtifacts;
          # Also the Firefox native-messaging host of the on-screen keyboard extension: the same
          # binary under the name Firefox starts, and its host manifest where Firefox (and
          # nixpkgs' `programs.firefox.nativeMessagingHosts.packages`) look for it.
          hinteros-session = mkBin "hinteros-session" {
            postInstall = ''
              ln -s hinteros-session $out/bin/hinteros-osk-native-host
              mkdir -p $out/lib/mozilla/native-messaging-hosts
              substitute ${./firefox-extension/native-messaging/hinteros_osk.json} \
                $out/lib/mozilla/native-messaging-hosts/hinteros_osk.json \
                --replace-fail @path@ $out/bin/hinteros-osk-native-host
            '';
          };
          hinteros-system = mkBin "hinteros-system" { };
        };

      rust = mkRust pkgs;

      # The on-screen keyboard's Firefox extension, as an unsigned .xpi (a zip of the
      # extension directory). Installed by the system through Firefox's enterprise policies.
      mkFirefoxExtension =
        pkgs:
        pkgs.stdenvNoCC.mkDerivation {
          pname = "hinteros-osk-firefox-extension";
          inherit (lib.importJSON ./firefox-extension/manifest.json) version;
          src = lib.fileset.toSource {
            root = ./firefox-extension;
            fileset = lib.fileset.unions [
              ./firefox-extension/manifest.json
              ./firefox-extension/background.js
              ./firefox-extension/content.js
            ];
          };
          nativeBuildInputs = [
            pkgs.zip
            pkgs.jq
            pkgs.nodejs
          ];
          buildPhase = ''
            runHook preBuild
            jq -e '.browser_specific_settings.gecko.id == "hinteros-osk@hinteros"' manifest.json
            node --check background.js
            node --check content.js
            # Reproducible: fixed order and timestamps, no extra attributes.
            touch -d @315619200 manifest.json background.js content.js
            zip -X -D -q hinteros-osk@hinteros.xpi manifest.json background.js content.js
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            install -Dm644 hinteros-osk@hinteros.xpi \
              $out/share/hinteros/firefox/hinteros-osk@hinteros.xpi
            runHook postInstall
          '';
          passthru.extensionId = "hinteros-osk@hinteros";
          meta = {
            description = "HinterOS on-screen keyboard: Firefox extension reporting text field focus";
            license = lib.licenses.gpl3Only;
            platforms = lib.platforms.all;
          };
        };
      firefox-extension = mkFirefoxExtension pkgs;

      preCommit = git-hooks.lib.${system}.run {
        src = ./.;
        hooks = {
          nixfmt.enable = true;
          statix.enable = true;
          deadnix.enable = true;
          # rustfmt directly on the staged files: works offline (no cargo metadata).
          rustfmt-files = {
            enable = true;
            name = "rustfmt";
            entry = "${pkgs.rustfmt}/bin/rustfmt --check --config-path ${./rustfmt.toml}";
            types = [ "rust" ];
          };
        };
      };
    in
    {
      packages.${system} = {
        desktop-preview = pkgs.callPackage ./preview.nix { inherit (rust) hinteros-session; };
        inherit (rust) hinteros-session hinteros-system;
        hinteros-osk-firefox-extension = firefox-extension;
        default = pkgs.symlinkJoin {
          name = "hinteros-daemon-${version}";
          paths = [
            rust.hinteros-session
            rust.hinteros-system
            firefox-extension
          ];
        };
      };

      apps.${system}.desktop-preview = {
        type = "app";
        program = lib.getExe self.packages.${system}.desktop-preview;
        meta.description = "Session daemon for trying the launcher on a desktop (development only)";
      };

      overlays.default = final: _prev: {
        inherit (mkRust final) hinteros-session hinteros-system;
        hinteros-osk-firefox-extension = mkFirefoxExtension final;
      };

      checks.${system} = {
        inherit (rust) hinteros-session hinteros-system;
        hinteros-osk-firefox-extension = firefox-extension;
        # The installed host manifest points at the installed host.
        native-messaging-host = pkgs.runCommand "native-messaging-host-check" { } ''
          m=${rust.hinteros-session}/lib/mozilla/native-messaging-hosts/hinteros_osk.json
          ${lib.getExe pkgs.jq} -e --arg p ${rust.hinteros-session}/bin/hinteros-osk-native-host \
            '.path == $p and .name == "hinteros_osk" and .allowed_extensions == ["hinteros-osk@hinteros"]' $m
          test -x ${rust.hinteros-session}/bin/hinteros-osk-native-host
          touch $out
        '';
        cargo-test = rust.craneLib.cargoTest (
          rust.commonArgs
          // {
            inherit (rust) cargoArtifacts;
            cargoTestExtraArgs = "--workspace";
            # Stand-ins for gamescope's Xwayland (Xvfb) and the system bus (a private
            # dbus-daemon, also for the system service's fake logind/polkit/systemd), plus git
            # and ssh-keygen for update verification; required here, optional elsewhere. bwrap for
            # the system call filter (its test skips where bwrap cannot run).
            nativeCheckInputs = [
              pkgs.xvfb
              pkgs.dbus
              pkgs.git
              pkgs.openssh
              pkgs.bubblewrap
            ];
            HINTEROS_REQUIRE_TEST_TOOLS = "1";
          }
        );
        cargo-clippy = rust.craneLib.cargoClippy (
          rust.commonArgs
          // {
            inherit (rust) cargoArtifacts;
            cargoClippyExtraArgs = "--workspace --all-targets -- -D warnings";
          }
        );
        cargo-fmt = rust.craneLib.cargoFmt { inherit (rust.commonArgs) src pname version; };
        pre-commit = preCommit;
      };

      devShells.${system}.default = rust.craneLib.devShell {
        checks = self.checks.${system};
        packages =
          preCommit.enabledPackages
          ++ (with pkgs; [
            rust-analyzer
            cargo-watch
            nixfmt
            jq
          ]);
        RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        inherit (preCommit) shellHook;
      };

      formatter.${system} = pkgs.nixfmt-tree;
    };
}
