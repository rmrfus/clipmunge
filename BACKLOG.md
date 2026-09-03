# Backlog

Work that is not done. Each item says why it matters and what it would take;
the reasoning behind decisions *already taken* is in [DESIGN.md](DESIGN.md),
including the things that were considered and rejected.

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

No longer a release blocker: it was one when nothing but the binary shipped,
and the flake and the Makefile both install the man pages now, so the
uncovered audience is the single `cargo install --git` path.

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

### A handler cannot see the incoming selection

The largest open question here, and half-built already. A handler receives the
capture groups of `match`, which is a regex over the plain text, and nothing
else. It cannot look at the `text/html` that arrived, or at the MIME list, or
at anything the rule did not itself capture.

That rules out a whole class of rule nobody can write today: fix the href
inside HTML somebody copied out of a page, rewrite text and markup together so
they agree, decide based on which flavours are present rather than on what the
text looks like.

The read path is already done. `worth_reading` deliberately covers
`RICH_MIMES` and not just the text family, so `text/html`, the source URL,
`text/uri-list` and `text/rtf` are fetched and sitting in the `Selection` by
the time a rule runs — see the comment at `clipboard.rs:50`. Only the handler
API has to change.

Two shapes, and they are not equivalent:

- **A second argument.** `handler = function(caps..., incoming)`. Breaks every
  handler declared with a fixed arity that a caller now passes one more
  argument to — which in Lua is silently harmless, so it breaks nothing
  loudly, which is worse. It also puts the incoming selection after a variable
  number of capture groups, so a rule has to count.
- **`clipmunge.incoming` as a table**, valid for the duration of the call.
  Additive: existing rules never mention it and are unaffected. Costs a table
  built per rewrite, and it is ambient rather than passed, which reads worse
  but is the only one that does not touch the existing shape.

The second is probably right for the same reason `flavour_order` should be a
setting rather than a change to what a handler returns: the shape every
published rule set is written against is the expensive thing to move.

Wait for a rule that actually wants it. But do not let the read-path work go
unrecorded in the meantime, which is what this entry is for.

### Smaller

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
