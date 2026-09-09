# clipmunge conventions

Rust Wayland clipboard daemon with Lua rules. See
[development instructions](docs/development.md) for commands and layout,
and [design notes](DESIGN.md) for architecture.

## Checks

Use the Nix development environment and `--locked` for Cargo build, test and
clippy commands. Install the staged-tree hook with
`git config core.hooksPath hooks`. Run checks relevant to the change; lint
edited man pages with groff and validate edited example configs with `--check`.
For regression tests, confirm they fail with the bug restored.

## Code

- Return `ExitCode` from `main`; send diagnostics to stderr.
- No unwrap, expect, panic, unreachable, todo or unimplemented in production
  code. Clippy permits unwrap, expect and panic in tests.
- Document each unsafe block with `SAFETY` and keep one unsafe operation per
  block.
- Keep dependencies small; measure substantial additions using comparable
  Nix builds. Preserve measurements and conditions in
  [DESIGN.md](DESIGN.md#recorded-measurements).

## Behaviour to preserve

- The config is trusted and can set `notify_command`. Exclude unwanted Lua
  libraries at state creation; clearing globals leaves `package.loaded`
  accessible. Keep instruction, memory and I/O limits.
- Gate payload previews on `--debug`. Lua print output and errors are not
  redacted; do not promise that every other log line is free of clipboard text.
- Missing config fails startup. Failed reload keeps the previous rules.
  Successful reload preserves the runtime notification setting.
- Resolve the config path on every load to follow replaced symlinks. Honour
  `XDG_CONFIG_HOME` only when absolute; otherwise use `$HOME/.config`.
- Check the marker and secret MIME list before reading an offer. Secret MIME
  matching is case-insensitive; selection `get`/`has` use exact names.
- Pass the incoming selection first, then capture groups. Its read-only Lua
  userdata is scoped to the handler call.
- Publish MIME types in `Selection::canonical_order`, never Lua table order.
- Drain events before the generation check. Do not sleep in `tick` while
  `got_selection` is set.
- Poll reads with deadlines and use non-blocking writes. Explicitly destroy discarded
  Wayland offers; dropping a proxy does not send its protocol destructor.
- Send `READY=1` only after all fallible startup steps succeed.
- Keep `ext-data-control-v1` as the only clipboard backend.

## Build and docs

- Read the MSRV from `Cargo.toml` in CI; do not duplicate the version in the
  workflow. Keep the Rust toolchain action on a pinned `v1` revision and pass
  the version as an input.
- Run both `cargo deny check advisories sources`; these are independent checks.
- Lua is vendored C; Wayland uses the Rust backend. Neither needs a separately
  installed Lua or Wayland library. The build still needs a C compiler.
- Install Nix user units into `lib/systemd/user` for NixOS discovery; let
  stdenv relocate them. Substitute the store path into `ExecStart`.
- Keep README focused on setup and use. Put API details in man pages and
  implementation rationale beside the relevant code or in DESIGN.md.
- Comments should explain constraints or non-obvious behaviour. Omit bug-fix
  narratives, rhetorical comparisons and claims about how well tested code is.
