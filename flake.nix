{
  description = "clipmunge — rule-driven Wayland clipboard rewriter";

  # Reuse the registry nixpkgs; consumers can override with inputs.nixpkgs.follows.
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
          # Use the committed lockfile for dependency resolution.
          cargoLock.lockFile = ./Cargo.lock;

          # Wayland uses the Rust backend; vendored Lua is compiled with stdenv cc.
          # No separately installed Wayland or Lua library is needed.

          postInstall = ''
            install -Dm644 man/man1/clipmunge.1 $out/share/man/man1/clipmunge.1
            install -Dm644 man/man5/clipmunge.5 $out/share/man/man5/clipmunge.5
            install -Dm644 config.lua.example \
              $out/share/doc/clipmunge/config.lua.example

            # NixOS discovers units in lib; stdenv moves them to share and leaves a symlink.
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

      # CI also builds the package explicitly; example-config depends on it here.
      checks = forAll (pkgs: {
        # Validate the shipped Lua example without a compositor.
        example-config =
          pkgs.runCommand "clipmunge-example-config" { }
            ''
              ${self.packages.${pkgs.stdenv.hostPlatform.system}.default}/bin/clipmunge \
                --check -c ${./config.lua.example}
              touch $out
            '';

        # Cross-compile for aarch64, including the vendored C build of Lua.
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
            cargo-deny   # cargo deny check advisories sources
            cargo-machete # dependencies declared and never used
            wl-clipboard # wl-copy / wl-paste -l, for poking a rewrite by hand
            libnotify    # notify-send, the default notify_command
          ];
        };
      });
    };
}
