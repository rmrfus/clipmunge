# Development

Enter the development shell with `nix develop` or `direnv allow`.
Run checks using the pinned environment:

```sh
nix develop --command cargo build --release --locked
nix develop --command cargo fmt --all --check
nix develop --command cargo clippy --all-targets --locked -- -D warnings
nix develop --command cargo test --locked
nix develop --command cargo deny check advisories sources
nix develop --command cargo machete
nix develop --command groff -man -Tutf8 -ww -z man/man1/clipmunge.1
nix develop --command groff -man -Tutf8 -ww -z man/man5/clipmunge.5
nix flake check
nix build
```

`nix flake check` builds the package to validate the example config, and builds
an aarch64 target from each supported host. Cross-built tests are disabled.
CI also checks the Rust version declared in `Cargo.toml`.

Install the pre-commit hook with `git config core.hooksPath hooks`. It runs
fmt, clippy, tests, dependency advisories/source checks and unused-dependency
checks against the staged tree.

The rule engine tests build configs from Lua strings without a compositor.
When adding a regression test, verify that it fails with the bug restored.

## Layout

- `src/clipboard.rs`: Wayland offers, reads, writes and event dispatch.
- `src/config.rs`: Lua engine, configuration and rule API.
- `src/selection.rs`: clipboard payloads and MIME ordering.
- `src/urlclean.rs`: tracking parameter removal.
- `src/watch.rs`: config directory watches and reload debounce.
- `src/notify_ready.rs`: systemd readiness notification.
- `src/main.rs`: CLI and main loop.

## Packaging

Nix and `make install` ship the binary, man pages, example config and service.
Both substitute the installed binary's path into `ExecStart`.

The flake installs the service into `lib/systemd/user` so NixOS can discover
it. The stdenv relocation hook moves it to `share/systemd/user` and leaves
a symlink at the original path.

## Dependency size

Keep dependencies small. Compare package builds with the same toolchain,
features and build settings; `nix build` includes cargo-auditable metadata
and is not directly comparable to `cargo build --release`.
Report the size change when adding a substantial dependency. Preserve results
and their conditions with the [recorded measurements](../DESIGN.md#recorded-measurements).

## Releases

Create an annotated `v*` tag with release notes. The release workflow builds
the tag and uses its annotation as the GitHub release body. It does not
publish prebuilt binaries.
