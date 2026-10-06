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
            version = "0.1.0";
          };
          # Dependencies are built once and shared by every package and check.
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          mkBin =
            name:
            craneLib.buildPackage (
              commonArgs
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
          hinteros-session = mkBin "hinteros-session";
          hinteros-system = mkBin "hinteros-system";
        };

      rust = mkRust pkgs;

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
        default = pkgs.symlinkJoin {
          name = "hinteros-daemon-0.1.0";
          paths = [
            rust.hinteros-session
            rust.hinteros-system
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
      };

      checks.${system} = {
        inherit (rust) hinteros-session hinteros-system;
        cargo-test = rust.craneLib.cargoTest (
          rust.commonArgs
          // {
            inherit (rust) cargoArtifacts;
            cargoTestExtraArgs = "--workspace";
            # Stand-ins for gamescope's Xwayland (Xvfb) and the system bus (a private
            # dbus-daemon) in the session daemon's tests; required here, optional elsewhere.
            nativeCheckInputs = [
              pkgs.xvfb
              pkgs.dbus
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
