# clipmunge

[![CI](https://github.com/rmrfus/clipmunge/actions/workflows/ci.yml/badge.svg)](https://github.com/rmrfus/clipmunge/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/rmrfus/clipmunge)](https://github.com/rmrfus/clipmunge/releases/latest)
[![License](https://img.shields.io/github/license/rmrfus/clipmunge)](LICENSE)

clipmunge rewrites your Wayland clipboard using rules you write in Lua.
Use it to remove tracking parameters from copied URLs, shorten shopping links,
or turn ticket numbers into links.

A rule can provide both plain text and HTML: copy `BUG-4471`, paste `BUG-4471`
into a terminal, or paste a clickable link into an editor that accepts HTML.

## Requirements

A Wayland compositor with `ext-data-control-v1` support, such as sway 1.11+.
The older `wlr-data-control` protocol and X11 are not supported.
Currently, rules process text and rich text; images and the PRIMARY selection
are not supported.

## Install

With Nix:

```sh
nix profile install github:rmrfus/clipmunge
```

Or build from source with Rust 1.88+ and a C compiler for the bundled Lua:

```sh
git clone https://github.com/rmrfus/clipmunge
cd clipmunge
make
make install PREFIX="$HOME/.local"
```

Both methods install the binary, man pages, example config and a systemd user
service. Ensure the installation's `bin` directory is on your `PATH`.
See [installation details](docs/install.md) for NixOS, Home Manager,
system-wide installation and `cargo install`.

## Configure

Create `~/.config/clipmunge/config.lua` (or
`$XDG_CONFIG_HOME/clipmunge/config.lua` if you use a custom config directory).
For example, this rule removes common tracking parameters from URLs:

```lua
clipmunge.rule {
  name = "detrack",
  match = [[^(https?://\S+)$]],
  handler = function(_, url)
    local clean = clipmunge.url.strip_params(url)
    if not clean then return nil end
    return clipmunge.link(clean)
  end,
}
```

Copy `https://example.com/page?utm_source=newsletter` and paste
`https://example.com/page`.

To turn ticket numbers into links, add a rule with your tracker URL:

```lua
clipmunge.rule {
  name = "tickets",
  match = [[^([A-Z]+)-(\d+)$]],
  when = "plain-only",
  handler = function(_, kind, number)
    local id = kind .. "-" .. number
    return clipmunge.link("https://tracker.example.com/" .. id, id)
  end,
}
```

`plain-only` skips a rule if any rich-text MIME payload was read successfully.
Rules run in declaration order;
the first to return a replacement wins. Returning `nil` tries the next rule.
There are no built-in rules, and clipmunge requires a config file to start.

[config.lua.example](config.lua.example) has more examples.
`man 5 clipmunge` documents the full Lua API.

## Run

```sh
clipmunge --check     # validate the config
clipmunge            # run in the foreground
```

Edits to the config reload automatically. If a reload fails, clipmunge logs
the error and keeps the previous rules.

To run the installed service in your Wayland session (Nix profile users:
[link the unit first](docs/install.md#nix-profile)):

```sh
systemctl --user daemon-reload
systemctl --user enable --now clipmunge
```

For session setup and NixOS/Home Manager services, see
[installation details](docs/install.md).

Use `clipmunge --debug` to inspect rewrites. **This logs clipboard contents**,
which may include passwords. Normal payload diagnostics show MIME types and
sizes, but Lua output and error messages can also contain copied text.
`man 1 clipmunge` lists all options.

## Rules and privacy

Only install configs you trust: they can choose a program to run for
notifications. Lua has restricted libraries and instruction and memory limits;
see `man 1 clipmunge` for details. `--no-notify` disables notifications.

Selections marked with `x-kde-passwordManagerHint` are skipped before reading.
Applications do not always supply this hint, and clipmunge cannot otherwise
recognise passwords. Keep rule patterns specific to the content you want to
change.

## Development

See [development instructions](docs/development.md), [design notes](DESIGN.md)
and the [backlog](BACKLOG.md).

MIT licensed. See [LICENSE](LICENSE).
