# clipmunge — conventions

A Wayland clipboard daemon: watch the selection over `ext-data-control-v1`,
run it past Lua rules, put a rewritten one back that can serve different bytes
for each MIME type. Rust, no C libraries — the wayland backend is the pure
Rust one and Lua 5.4 is vendored and compiled by the stdenv cc.

Run them the way CI does, or you get findings CI has not got and miss the ones
it has. `--locked` everywhere: the lockfile is committed.

- build: `nix develop --command cargo build --release --locked`
- test: `nix develop --command cargo test --locked`
- lint: `nix develop --command cargo clippy --all-targets --locked -- -D warnings`
- fmt: `nix develop --command cargo fmt --all --check`
- audit: `nix develop --command cargo deny check advisories sources`
- dead deps: `nix develop --command cargo machete`
- man lint: `nix develop --command groff -man -Tutf8 -ww -z man/man{1,5}/clipmunge.*`
- package: `nix build`

Install the hook once per clone: `git config core.hooksPath hooks`.

## Layout

- `clipboard.rs` — the ext-data-control-v1 plumbing and the poll loop.
- `config.rs` — the Lua engine: sandbox, rule table, the `clipmunge.*` API.
- `selection.rs` — a selection as bytes-and-MIME, deliberately not a string.
- `urlclean.rs` — `strip_params` and the default junk list. Pure, and the only
  part with tests.
- `watch.rs` — inotify on the config *directory*, with a settle delay.

## Layout note

`notify_ready.rs` is the sd_notify half of `Type=notify`, hand-rolled on std
(one `READY=1` datagram) rather than a crate.

## This crate watches its binary size

Stated so that `codegen-units = 1` and the dependency arguments in Cargo.toml
are grounded in something rather than taste. The size table in BACKLOG.md is
how new dependencies get argued about — `regex` costs 917 KB with unicode
trimmed, `image` would cost 668 KB, `clap` costs 347 KB — and a change that
moves the binary noticeably is expected to say by how much.

Measure with `nix build`, not `cargo build --release`: nixpkgs wraps cargo in
`cargo-auditable`, so the two are not comparable, and **measure both sides on
the same tree**. Comparing against a figure from an older commit is how a clap
trim was once recorded here as making the binary *larger*.

## Non-negotiables

- **The trust boundary is the config FILE, not the interpreter.** The file can
  name a `notify_command`, so it can run a program; a *rule* cannot. Say it
  that way round in the docs. The earlier phrasing promised a sandbox the tool
  does not have, and it took a live exploit to notice.
- **Unwanted stdlib is never loaded, not deleted.** `Lua::new_with` picks the
  library set. Setting a global to nil does nothing: `luaL_openlibs` also
  files each library under `package.loaded`, and `require("os").execute` finds
  it there. This was a real hole; do not reintroduce it by "just nil-ing" a
  new global.
- **Anything that leaves the interpreter is on a budget.** Instruction hook,
  memory limit, per-flavour read timeout, whole-read budget. A runaway rule is
  a bug, and a bug must cost a log line, not a dead clipboard.
- **`--debug` is the only thing that may log clipboard content.** Every other
  path logs MIME types and byte counts.
- **The daemon refuses to start without a config.** No built-in rule set. A
  clipboard daemon that rewrites things nobody asked for is a bad neighbour.
- **A failed config reload keeps the old rules.** A typo must not silently
  disarm the clipboard.
- **The config path is resolved on every load, never cached.** A config
  manager publishes a new file and moves the symlink; a path resolved once at
  startup pins the daemon to the version it booted with.
- **Publish order is `Selection::canonical_order`, never Lua table order.**
  Lua seeds its string hash per process, so `pairs` order changes between
  runs, and clients that take the first flavour they recognise then paste
  something different after a restart.
- **The secret guard runs on the announced MIME list, before any read.** Not
  in Lua, and not as a `when` value on a rule: a handler is reached only after
  the text has been pulled out of the pipe, at which point `--debug` has
  already put it in the journal. The list is configurable, the decision is
  not. Its log line says nothing about the content, on purpose.
- **`application/x-clipmunge` is the loop guard**, checked against the
  *advertised* MIME list before anything is read — not after.
- **Nothing blocks inside an event dispatch.** The pipe a pasting client hands
  us holds 64 KB and a rewrite may be four times that, so the send path is
  non-blocking with a deadline. A blocking write there stops the clipboard for
  as long as the client feels like not reading.
- **`tick` does not sleep while `got_selection` is set.** `drain_events` can
  pick a selection up mid-rewrite; going to the socket first would leave it
  unhandled until some unrelated event arrived. Found by running the race, not
  by reading the code.
- **`cargo deny` runs `advisories sources`, both named.** They are independent
  checks: `advisories` alone never reads the `[sources]` section, so a repo
  that configures `unknown-git = "deny"` and runs only `advisories` has a
  setting nothing enforces. Verified by pointing `allow-registry` at a
  nonexistent index - `advisories` still said ok, `sources` failed.
- **`rust-version` and the MSRV job's `toolchain:` input move in the same
  commit.** Nothing enforces the pairing. Raise the floor alone and the job
  keeps passing on the old toolchain, so it verifies a floor the crate no
  longer claims. The pin is `dtolnay/rust-toolchain@<sha> # v1` with the
  version as an input precisely so dependabot can keep the action current
  without touching the toolchain - do not pin a version branch instead, that
  puts the version back in the ref.
- **The unit is `Type=notify` and something has to send READY=1.** Under
  `Type=simple` a start "succeeds" for a daemon that is about to exit because
  no compositor answered. If the startup path grows a step that can fail, it
  goes *before* `notify_ready::ready()`, not after.
- **`XDG_CONFIG_HOME` counts only when absolute.** Empty or relative falls back
  to `$HOME/.config`; taking it naively makes `XDG_CONFIG_HOME=""` a path
  relative to `WorkingDirectory`, which under systemd is not where anyone put
  their config.
- **Every wayland object we are handed gets destroyed.** wayland-rs does not
  send destructors on drop, and `ext-data-control-v1` says the client *must*
  destroy the offer it replaces. Forgetting one leaks a compositor resource
  per copy for the whole session.
- **No `zwlr_data_control_v1` fallback.** Decided in BACKLOG.md; every
  protocol carried twice is carried for years.

## Nix

`flake.nix` has no `buildInputs` and that is load-bearing, not an oversight:
nothing here links a C library. If a dependency ever needs `pkg-config`, that
is a fact worth arguing about before it lands.

The package installs the systemd user unit into `lib/systemd/user`, with
`ExecStart` substituted to the store path. Installing straight into
`share/systemd/user` would look equivalent and would not be: NixOS
`systemd.packages` globs `etc/systemd/user` and `lib/systemd/user` only
(`nixos/lib/systemd-lib.nix`). stdenv's `move-systemd-user-units` hook then
moves the file to `share/` and leaves `lib/systemd/user` as a symlink, so the
glob still finds it — install to `lib`, let the hook do the rest.
