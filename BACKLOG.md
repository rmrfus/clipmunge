# Backlog

Ideas and deferred decisions, with the reasoning that produced them. Anything
here was considered and consciously postponed, not forgotten.

## Blocking a release

Nothing, as of v0.1.0.

The rule engine is covered now: 43 tests, 22 of them in `config.rs`, reached
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

Embedding costs 16 KB against a 3.4 MB binary. The better reason to embed is
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

## Other MIME types

Text and HTML only, for now. The read path is already bytes-and-MIME rather
than a string, so a new flavour does not need the core rewritten — that seam
was cut on purpose.

Use cases worth having, in order of obviousness:

- strip EXIF/GPS from a copied image, the exact analogue of dropping `utm_*`
- re-encode a 20 MB PNG screenshot into something pasteable
- downscale to a maximum long edge

Measured costs, on a release build with LTO:

| addition                                  | binary | delta   |
| ----------------------------------------- | ------ | ------- |
| baseline (empty binary)                   | 289 KB | —       |
| `img-parts` (EXIF surgery, no decode)     | 297 KB | +8 KB   |
| `image` with png+jpeg+webp, decode+encode | 957 KB | +668 KB |

Cheaper than `regex`, so no need for a cargo feature. Stripping EXIF is
essentially free because it walks JPEG APP segments and PNG chunks rather than
decoding anything, which makes it a reasonable default for everyone.

What images break that text does not:

- **Size caps must become per-type.** `READ_LIMIT` is one number today. Text
  wants tens of KB, an image wants tens of MB, and a decoded 4K RGBA buffer is
  33 MB on its own.
- **The generation counter is already load-bearing.** Decode, resize and
  re-encode of a 1920x1080 PNG measured 150 ms. That is long enough to copy
  something else, and publishing a rewrite of the previous clipboard is worse
  than doing nothing. It fires today because `drain_events` reads the socket
  before the check; it used to be dead code, because nothing dispatched
  between taking the generation and comparing it, so the two could not
  differ. Anything added to the slow path keeps that drain in front of the
  publish, and `tick` has to keep skipping its sleep while `got_selection` is
  set, or the selection the drain picked up waits for an unrelated event.
- **Pixels stay in Rust.** Lua is policy; `clipmunge.image.*` does the work.
  Decoding a PNG in a sandboxed interpreter is not a plan.
- The `image` crate's WebP encoder is **lossless only** — 730 KB against JPEG's
  96 KB on the same frame. Lossy WebP needs libwebp over FFI and gives up the
  pure-Rust build.

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

## Domain-blind tracker list

`strip_params` matches parameter names with no idea what host they sit on, so
a short entry means what it means everywhere. `si` is Spotify and YouTube;
`spm` is Alibaba; `ref_src` is a Twitter embed. On a site that uses one of
those names for something real, the rule quietly drops it.

ClearURLs solves this with per-domain rule sets, which is a data file, a
matcher and a maintenance burden. Not obviously worth it here: the config is
Lua, so a rule that cares passes its own list, and the pattern in `match`
already scopes a rule to a host if written that way. Revisit when somebody
turns up with a URL the default list breaks.

## Compatibility

- Packaging: AUR and a Fedora COPR. The nix flake is done — package (binary,
  man pages, example config, a user unit with `ExecStart` pointed at the store
  path) plus a dev shell.

## Decided against

- **Shelling out from *rules*.** Killed the `command = [...]` escape hatch on
  a rule, and it cost nothing: the URL table it existed to reach just lives in
  the Lua config now. Handlers stay fast enough that the stale-rewrite race
  never opens for text.

  What this does *not* buy is a config you can run unread. `notify_command` is
  still a program the config names, with rule output as an argument — that is
  the feature, and the trust boundary is therefore the file, not the
  interpreter. Said plainly in the README and `clipmunge(1)` rather than
  implied away.
- **Rule chaining.** First match wins, in declaration order. Chaining reads
  well right up until two rules feed each other, and the marker MIME cannot
  catch a loop that happens inside one pass.
- **A declarative TOML rule format.** It had already grown `when`, `command`,
  per-MIME templates and escaping rules before a line was written — a bad
  programming language in a config file. Lua is a good one.
- **A `zwlr_data_control_v1` fallback.** The two protocols are identical in
  shape, so this is cheap — a trait over both, or a macro. It would also be
  the difference between running and not running on Debian 13 (sway 1.10.1)
  and Ubuntu 24.04 LTS (sway 1.9), which is not a small audience. Declined
  anyway: ext-data-control is the standard, those releases are already being
  superseded, and every protocol carried twice is carried for years. The
  requirement is stated in the README instead, and the daemon says so on
  startup rather than failing obscurely.
- **Trimming `regex` in favour of Lua patterns.** Lua patterns have no
  alternation and no bounded quantifiers, so `D\d{6,9}` is inexpressible. The
  crate costs 917 KB with unicode trimmed to `\d`/`\w`/`\s` and case folding,
  and it cannot backtrack, so a hostile pattern in a third-party rule set
  cannot hang the daemon.

## Documentation debts

Things that will otherwise arrive as issues:

- `match` is a real regular expression, not a Lua pattern. Lua's own
  `string.match` still exists inside handlers, and the two syntaxes look
  similar enough to confuse everyone once.
- No lookaround, no backreferences — that is the price of linear-time matching.
- Escaping belongs to the helpers. `clipmunge.link` escapes for you; a table
  you assemble by hand is your own responsibility, and getting it wrong means
  clipboard content injected into markup.
- Unicode in patterns is trimmed: `\p{Greek}` and friends are not compiled in.
  Restoring them costs about 250 KB.
