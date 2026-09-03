# clipmunge

Rewrites the Wayland clipboard as you copy, according to rules you write in
Lua. Copy a bare ticket number and paste a link; copy a URL out of a phone app
and paste it without the tracking.

```lua
clipmunge.rule {
  name = "tickets",
  match = [[^([A-Z]+)-(\d+)$]],
  when = "plain-only",
  handler = function(kind, number)
    local id = kind .. "-" .. number
    return clipmunge.link("https://tracker.example.com/" .. id, id)
  end,
}
```

Copying `BUG-4471` now puts this on the clipboard:

| flavour                 | content                                                       |
| ----------------------- | ------------------------------------------------------------- |
| `text/plain`            | `BUG-4471`                                                    |
| `text/html`             | `<a href="https://tracker.example.com/BUG-4471">BUG-4471</a>` |
| `chromium/x-source-url` | `https://tracker.example.com/BUG-4471`                        |

The shell still gets a bare identifier. Workplace, Quip, Slack and mail get a
link. One copy, no decision to make at the time.

## Why this needs its own tool

Serving **different bytes for different MIME types of one selection** is the
whole point, and it is the one thing the usual tools cannot do. `wl-copy`
hands out the same buffer for everything it advertises, so it cannot make a
selection whose plain text and HTML disagree; the requests to change that have
been open since 2021 (wl-clipboard#71, #248).

The `ext-data-control-v1` protocol answers each MIME request separately, which
is exactly the shape needed. clipmunge speaks it directly.

## What a rule can and cannot do

**Your config is trusted, the way your shell rc file is.** It can set
`notify_command`, and that runs a program with rule output as an argument —
which is the point of the setting, and also the reason a rule set from a
stranger is not something to paste in unread.

What the interpreter hands a *rule* is much less. `io`, `os`, `dofile`,
`load`, `coroutine` and the C module loader are **never loaded** — not
deleted afterwards, never loaded — and `package.path` points at the config
directory alone. At copy time a rule can compute and nothing else: it cannot
open a file, reach the network, or start a process.

That distinction is worth the words. Deleting the globals is what an earlier
version did, and it does not work: `luaL_openlibs` also files every library
under `package.loaded`, which is the first place `require` looks, so
`require("os").execute` walks straight past a nil `os`. Not loading the
library leaves nothing to find.

A handler is a pure function from selection to selection, on a budget: about
ten million VM instructions and 64 MB. That is thousands of times what a real
rule uses, and it means a `while true do end` costs one logged error rather
than a clipboard that stops working until you notice.

A handler can *describe* a notification by returning a `notify` field; the
daemon decides whether to send it, truncates it to 200 characters, and sends
at most one per rewrite. `--no-notify` switches the whole mechanism off.

## Install

### Requirements

Rust 1.88 or newer to build from source. That floor arrives with mlua, and
clipmunge now uses a let-chain of its own, so it is real on both counts.

A compositor that implements **`ext-data-control-v1`** — sway 1.11 or newer,
or equivalently recent wlroots. clipmunge does not fall back to the older
`wlr-data-control`, and will not: the whole design leans on the standard
protocol, and carrying two of them to reach releases that are already being
superseded is not a trade worth making.

Check with `sway --version`. At the time of writing:

| distribution             | sway   | works |
| ------------------------ | ------ | ----- |
| Arch, Fedora 42+         | 1.11+  | yes   |
| Debian forky / sid       | 1.12   | yes   |
| Ubuntu 26.04 (resolute)  | 1.11   | yes   |
| Debian 13 (trixie)       | 1.10.1 | no    |
| Ubuntu 25.04 / 25.10     | 1.10.1 | no    |
| Ubuntu 24.04 LTS (noble) | 1.9    | no    |

On an unsupported compositor clipmunge says so and exits rather than starting
and quietly doing nothing.

```sh
cargo install --git https://github.com/rmrfus/clipmunge
install -Dm644 man/man1/clipmunge.1 ~/.local/share/man/man1/clipmunge.1
install -Dm644 man/man5/clipmunge.5 ~/.local/share/man/man5/clipmunge.5
```

### Nix

```sh
nix run github:rmrfus/clipmunge -- --help
```

As a flake input:

```nix
inputs.clipmunge.url = "github:rmrfus/clipmunge";
# This flake's own nixpkgs input is the indirect `flake:nixpkgs`. Point it at
# yours, or the closure grows a second nixpkgs for one 3.4 MB binary.
inputs.clipmunge.inputs.nixpkgs.follows = "nixpkgs";
```

The package carries more than the binary: man pages, the example config at
`share/doc/clipmunge/config.lua.example`, and a systemd user unit whose
`ExecStart` already points at the store path. `systemd.packages` is what puts
that unit where the user manager can see it:

```nix
let clipmunge = inputs.clipmunge.packages.${pkgs.stdenv.hostPlatform.system}.default;
in {
  environment.systemPackages = [ clipmunge ];
  systemd.packages           = [ clipmunge ];
}
```

then `systemctl --user enable --now clipmunge`. With home-manager, install the
package into `home.packages` and write the config with

```nix
xdg.configFile."clipmunge/config.lua".source = ./clipmunge.lua;
```

There is no home-manager module: the unit ships with the package, and the
config is one `xdg.configFile` line. Note that `notify_command` defaults to
`notify-send`, which the user manager only finds if `libnotify` is on the
session PATH.

### Configuring it

Copy `config.lua.example` to `~/.config/clipmunge/config.lua` and edit it.
clipmunge does nothing until you do: there is no built-in rule set, and
without a config it prints the path it looked at and exits. A clipboard daemon
that starts rewriting things you never asked about is a bad neighbour.

## Using it

```sh
clipmunge                    # watch the clipboard
clipmunge --check            # load the config, report problems, exit
clipmunge --debug            # log every rewrite, before and after
```

`man 1 clipmunge` for the options, `man 5 clipmunge` for the config format.

Editing the config reloads it within about 150 ms — the directory is watched,
not the file, because editors save by writing a temporary file and renaming it
over the target. A config that fails to load is reported and the rules that
already work keep running, so a typo cannot quietly switch your clipboard back
to plain.

**`--debug` writes clipboard contents to the log**, all of it, including
whatever a password manager puts there. It is for working on a rule. Without
it the log records MIME types and byte counts and never content.

## What comes with the example config

- bare ticket identifiers become links, plain text untouched
- tracking parameters stripped from URLs
- shopping links reduced to the product identifier

The middle one is the rule everybody wants, so it is a library call rather than
something to copy around:

```lua
local clean, dropped = clipmunge.url.strip_params(url)
if not clean then return nil end   -- nothing to drop, and the loop guard
```

`clipmunge.url.default_junk` is the built-in list — the `utm_*` family,
`fbclid`, `gclid`, `msclkid`, `igshid`, `si`, `yclid`, `spm` and neighbours.
Pass your own table to replace it. Keys match case-insensitively, a trailing
`*` matches a prefix, a tracker name appearing in a *value* is left alone, and
the fragment survives.

## Hacking

```sh
nix develop                                        # or direnv allow
nix develop --command cargo build --release --locked
nix develop --command cargo test --locked
nix develop --command cargo clippy --all-targets --locked -- -D warnings
```

`git config core.hooksPath hooks` once per clone: the pre-commit hook runs the
same checks CI does — fmt, clippy, tests, `cargo deny`, `cargo machete` —
against the *staged* tree inside the dev shell, so a hunk that fails clippy
cannot sail through because the unstaged fix is still sitting on disk.

Without nix, `make && sudo make install` honours `PREFIX` and `DESTDIR`; that
is the path for an AUR or Debian package, since `cargo install` copies the
binary and leaves the man pages, the unit and the example config behind.

## Size

3.2 MB, and the number is watched: [BACKLOG.md](BACKLOG.md) carries the cost of
every dependency that was weighed, which is how the next one gets argued about.
`regex` is 917 KB of it with unicode trimmed to what clipboard rules need,
`clap` 347 KB, and image support would be another 668 KB.

## Status

Early. Text and HTML only; the read path is bytes-and-MIME throughout, so
images are a matter of adding rules rather than rewriting the core, but that
work has not happened. See [BACKLOG.md](BACKLOG.md), which also records the
things that were considered and rejected, and why.

## Licence

MIT. See [LICENSE](LICENSE).
