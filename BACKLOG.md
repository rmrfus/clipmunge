# Backlog

Work that is not done. Each item says why it matters and what it would take;
the reasoning behind decisions *already taken* is in [DESIGN.md](DESIGN.md),
including the things that were considered and rejected.

## Blocking a release

Nothing, as of v0.1.0.

The rule engine is covered now: 46 tests, 25 of them in `config.rs`, reached
without a compositor because a `Selection` goes in and a `Rewrite` comes out.
Rule order and first-match-wins, a handler that declines, throws, or returns
something unusable, `when = "plain-only"`, the escaping in `clipmunge.link`,
and the load-time failures — bad pattern, unknown `when`, missing handler.

Three of them are named after bugs this project actually had, and each was
checked by putting the bug back:

| test | mutation | what it did |
| ------------------------------------------------- | ------------------------------- | -------------------------------------- |
| `require_cannot_resurrect_io_or_os`                | `Lua::new()` for `new_with`     | failed: "io escaped the sandbox"       |
| `the_advertised_order_is_canonical_not_lua_table_order` | drop `canonical_order()`   | failed, showing the hash order         |
| `a_runaway_handler_is_skipped_rather_than_hanging` | `MAX_TICKS = u64::MAX`          | hung; libtest reported it past 60s     |

A regression test that has never been seen to fail is a comment with a test
harness around it, so this is worth repeating for the next one.

`clipmunge install` was on this list and is not any more. It was written before
the flake and the Makefile existed, when nothing shipped the man pages. Both do
now, so the uncovered audience is one path — `cargo install --git`, which has
no checkout — and the README names the two that work. Still worth having, no
longer a blocker; see below.

## Installing more than the binary

`cargo install` has no mechanism for man pages, units or example configs: the
only relevant flags are `--bin`, `--bins` and `--example`. Confirmed by
watching `cargo install wl-screenrec` leave its own man page behind.

The plan is a self-install subcommand, with the files embedded via
`include_str!`:

```sh
cargo install --git https://github.com/rmrfus/clipmunge
clipmunge install                 # into ~/.local
clipmunge install --prefix /usr   # for packagers
```

Places man1, man5, the systemd unit, and `config.lua.example` next to where
`config.lua` is expected. That last one closes a real papercut: installed from
git, there is currently nowhere to get the example from, because the checkout
cargo used is buried in `~/.cargo/git/checkouts`.

Embedding costs 16 KB against a 3.2 MB binary. The better reason to embed is
that the documentation then cannot drift from the version that ships it, which
is the usual way a package ends up describing a flag that no longer exists.

Two things it has to get right:

- **Rewrite `ExecStart` on the way out.** The shipped unit says
  `%h/.local/bin/clipmunge` while cargo installs to `~/.cargo/bin`. The
  subcommand knows where it is (`std::env::current_exe`) and should substitute
  that, or the first `systemctl --user enable --now` fails on a path that was
  wrong before anybody touched it.
- **Never write `config.lua`.** Only the example. If a config already exists,
  say nothing; if it does not, print the one line that copies it. The daemon
  already refuses to start without one, so the hint lands at the right moment.

Note this is not the `Makefile`'s job and does not replace it. The two serve
different people: `make install PREFIX=/usr DESTDIR=$pkgdir` is what an AUR or
Debian packager runs from a checkout, and `clipmunge install` is for somebody
who ran `cargo install --git` and has no checkout to run make in.

Rejected: a `build.rs` that installs files. Build scripts are supposed to
write only into `OUT_DIR`, it would not know the install root, it would run on
every ordinary `cargo build`, and it is precisely the trick that earns crates
a reputation.

## Configuration

- **Flavour order should be a setting.** Fixed order today: the text family,
  then `text/html`, then `chromium/x-source-url`, then anything a rule
  invented, by name. It has to be *some* fixed order — a handler returns a Lua
  table, Lua seeds its string hash per process, so before `canonical_order`
  the advertised order changed between daemon restarts (six starts, five
  orders). Not cosmetic: some applications take the first type they recognise
  rather than the best one.

  Making it a setting is additive — `clipmunge.settings { flavour_order = … }`
  breaks nothing that exists — so it waits for somebody with an application
  that wants the other order. Doing it in the *handler* return value would be
  the expensive version, since that changes the shape every published rule set
  is written against.
- **PRIMARY selection.** data-control offers it and we ignore it — though the
  offer object still has to be destroyed, or every mouse drag leaks one in the
  compositor. Rewriting every mouse selection would be unbearable, so this
  wants `selections = ["clipboard", "primary"]` and stays off by default.
- **SIGHUP** was dropped once inotify landed. Might still be wanted for
  scripted reloads.
- **`STOPPING=1` and a shutdown handler.** The unit is `Type=notify` and sends
  `READY=1`, but nothing handles SIGTERM: there is nothing to flush, and the
  published selection dies with the process either way, so a handler today
  would only make the log tidier. It becomes real the moment shutdown has work
  to do - unlinking something, releasing a lock.

## Lua API gaps

- `clipmunge.url.parse` / `build`. Not obviously worth a real URL parser as a
  dependency: `strip_params` needed only the text between `?` and `#`, and
  nothing else has asked for more yet. Revisit when a rule wants to touch the
  host or the path.
- `clipmunge.regex(pat)` as a value handlers can use for a second pass. Only
  the `match` field compiles a pattern right now.

## Compatibility

- Packaging: AUR and a Fedora COPR. The nix flake is done — package (binary,
  man pages, example config, a user unit with `ExecStart` pointed at the store
  path) plus a dev shell.
