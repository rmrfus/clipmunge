# Backlog

## Installation

- Add `clipmunge install` for users of `cargo install`. Embed man pages, the
  service and example config; default to `~/.local` and support `--prefix`.
  Set `ExecStart` to the installed executable. Never overwrite `config.lua`.
  Nix and `make install` already ship these files.
- Add AUR and Fedora COPR packaging.

## Configuration

- Configurable MIME order, if a client needs something other than the fixed
  plain-text-first order. Prefer a setting over changing handler return values.
- Optional PRIMARY selection handling, disabled by default. Ignored offers
  must still be destroyed.
- SIGHUP for scripted reloads if directory watching is insufficient.
- A shutdown handler and `STOPPING=1` if shutdown acquires cleanup work.

## Lua API

- URL parse/build helpers when a rule needs to edit hosts or paths.
- `clipmunge.regex(pat)` for matching inside handlers. Currently only a rule's
  `match` field uses Rust regex.
- Image rules and helpers; see [design notes](DESIGN.md#possible-image-support).
