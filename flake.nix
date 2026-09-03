{
  description = "clipmunge — rule-driven Wayland clipboard rewriter";

  # Indirect ref: on a machine whose flake registry already has nixpkgs
  # realised (e.g. the author's), this reuses that store path. Consumers get
  # whatever the lock pins — override with inputs.clipmunge.inputs.nixpkgs.follows.
  inputs.nixpkgs.url = "flake:nixpkgs";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems
        (system: f nixpkgs.legacyPackages.${system});
    in {
      packages = forAll (pkgs: rec {
        default = clipmunge;

        clipmunge = pkgs.rustPlatform.buildRustPackage {
          pname = "clipmunge";
          # Read straight from Cargo.toml so the two never drift apart.
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = self;
          # Cargo.lock is committed, so deps resolve straight from it — no
          # cargoHash to recompute on every dependency bump.
          cargoLock.lockFile = ./Cargo.lock;

          # No buildInputs, and that is the point: wayland-client uses the pure
          # Rust backend (no libwayland, no pkg-config) and mlua is `vendored`,
          # so it compiles Lua 5.4 from source with the stdenv cc. Nothing here
          # links against a C library at all.

          postInstall = ''
            install -Dm644 man/man1/clipmunge.1 $out/share/man/man1/clipmunge.1
            install -Dm644 man/man5/clipmunge.5 $out/share/man/man5/clipmunge.5
            # The daemon refuses to start without a config and there is no
            # built-in rule set, so the example has to arrive with the package
            # — the alternative is telling people to go find the source tree.
            install -Dm644 config.lua.example \
              $out/share/doc/clipmunge/config.lua.example

            # Ship a unit that actually points at this build. The one in the
            # tree says %h/.local/bin for people installing by hand; left alone
            # it would be a unit in a real unit directory with a path that
            # exists on no NixOS box.
            #
            # Install into lib/systemd/user even though the unit ends up in
            # share/: stdenv's move-systemd-user-units hook relocates it and
            # leaves lib/systemd/user as a symlink. That symlink is the point
            # — NixOS `systemd.packages` globs etc/systemd/user and
            # lib/systemd/user and nothing else (nixos/lib/systemd-lib.nix),
            # so a unit installed straight into share/ is a unit nobody scans.
            install -Dm644 systemd/clipmunge.service \
              $out/lib/systemd/user/clipmunge.service
            substituteInPlace $out/lib/systemd/user/clipmunge.service \
              --replace-fail '%h/.local/bin/clipmunge' "$out/bin/clipmunge"
          '';

          meta = with pkgs.lib; {
            description = "Rule-driven Wayland clipboard rewriter that can put different content in different MIME types";
            homepage = "https://github.com/rmrfus/clipmunge";
            changelog = "https://github.com/rmrfus/clipmunge/releases";
            license = licenses.mit;
            mainProgram = "clipmunge";
            platforms = platforms.linux;
          };
        };
      });

      # `nix flake check` builds these. The package itself is not repeated here;
      # CI runs `nix build` for that.
      checks = forAll (pkgs: {
        # config.lua.example is Lua that lives in the documentation: the file
        # the package installs and the README tells people to copy. Nothing
        # else compiles it, so an API change breaks it silently and the first
        # person to find out is whoever copied it. `--check` needs no
        # compositor and no $HOME, so it runs in the sandbox as it stands.
        example-config =
          pkgs.runCommand "clipmunge-example-config" { }
            ''
              ${self.packages.${pkgs.stdenv.hostPlatform.system}.default}/bin/clipmunge \
                --check -c ${./config.lua.example}
              touch $out
            '';

        # mlua compiles Lua 5.4 from C, and `c_char` is signed on x86_64 and
        # unsigned on aarch64, so "it builds here" is not the same statement as
        # "it builds on the other architecture this flake claims to support".
        # A real cross-compile rather than qemu: the aarch64 toolchain is a
        # store path, while emulating a whole Rust build is twenty minutes.
        cross-aarch64 = pkgs.pkgsCross.aarch64-multiplatform.rustPlatform.buildRustPackage {
          pname = "clipmunge-cross-aarch64";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          # Cross-built, so the test binaries cannot run on the builder.
          doCheck = false;
          meta.platforms = nixpkgs.lib.platforms.linux;
        };
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer
            groff        # man page lint: groff -man -Tutf8 -ww -z man/man{1,5}/clipmunge.*
            cargo-deny   # cargo deny check advisories, same step as CI
            cargo-machete # dependencies declared and never used
            wl-clipboard # wl-copy / wl-paste -l, for poking a rewrite by hand
            libnotify    # notify-send, the default notify_command
          ];
          # Nothing to put on LD_LIBRARY_PATH: see the package above, there are
          # no shared objects to find.
        };
      });
    };
}
