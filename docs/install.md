# Installation details

Create a config before starting clipmunge; see the [README](../README.md#configure).

## Nix profile

After `nix profile install github:rmrfus/clipmunge`, link the packaged service into
the user manager's search path. For the default profile:

```sh
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
ln -s "$HOME/.nix-profile/share/systemd/user/clipmunge.service" \
  "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/clipmunge.service"
```

Then follow [session setup](#session-and-notifications). After a package
upgrade, run `systemctl --user daemon-reload` and
`systemctl --user restart clipmunge` to use the new binary.

## NixOS

The module snippets below assume `pkgs` and `inputs` are in scope. Pass
`inputs` through NixOS `specialArgs` or Home Manager `extraSpecialArgs`.

Add the flake input and reuse your nixpkgs:

```nix
inputs.clipmunge.url = "github:rmrfus/clipmunge";
inputs.clipmunge.inputs.nixpkgs.follows = "nixpkgs";
```

Install the package and expose its user service in your NixOS configuration:

```nix
let
  clipmunge = inputs.clipmunge.packages.${pkgs.stdenv.hostPlatform.system}.default;
in {
  environment.systemPackages = [ clipmunge ];
  systemd.packages = [ clipmunge ];
}
```

After rebuilding, run `systemctl --user enable --now clipmunge` in your
Wayland session. The example config is at
`${clipmunge}/share/doc/clipmunge/config.lua.example`.

## Home Manager with sway

Using the same flake input:

```nix
let
  clipmunge = inputs.clipmunge.packages.${pkgs.stdenv.hostPlatform.system}.default;
  unit = "${clipmunge}/lib/systemd/user/clipmunge.service";
in {
  home.packages = [ clipmunge ];
  systemd.user.startServices = true;
  xdg.configFile."clipmunge/config.lua".source = ./clipmunge.lua;
  xdg.configFile."systemd/user/clipmunge.service".source = unit;
  xdg.configFile."systemd/user/sway-session.target.wants/clipmunge.service".source = unit;
}
```

Linking the unit through `xdg.configFile` lets Home Manager's service switching
track changes to its store path and restart it on upgrades.
`systemd.user.packages` installs the unit outside that tracked directory.
See Home Manager's [systemd module](https://github.com/nix-community/home-manager/blob/master/modules/systemd.nix).

The enable symlink assumes `wayland.windowManager.sway.systemd.enable = true`.
That [integration](https://github.com/nix-community/home-manager/blob/master/modules/services/window-managers/i3-sway/sway.nix)
imports the display environment and starts `sway-session.target`, which binds
to `graphical-session.target`. For another session setup, use
`graphical-session.target.wants` if that target is started by your session.

Replacing the config symlink triggers a reload. Watches are fixed at startup;
restart if the new target is in a different editable directory whose changes
you want watched. This does not affect immutable Nix store configs.

## From source

Rust 1.88+ and a C compiler are required. Lua is built from bundled source.

```sh
git clone https://github.com/rmrfus/clipmunge
cd clipmunge
make
```

Choose an install location:

```sh
make install PREFIX="$HOME/.local"          # current user
sudo make install                          # /usr/local
make install DESTDIR="$pkgdir" PREFIX=/usr  # package staging
```

The installer includes man pages, `share/doc/clipmunge/config.lua.example`,
and a user service with `ExecStart` set to the installed binary.
`make show PREFIX=…` prints destinations without installing.

## cargo install

```sh
cargo install --locked --git https://github.com/rmrfus/clipmunge
```

This installs only the binary. Create the config from the
[README example](../README.md#configure). For man pages and the service,
use Nix or `make install`.

## Session and notifications

The systemd user service requires `WAYLAND_DISPLAY` in the user manager's
environment and uses `graphical-session.target` for session lifetime.
Your compositor's session integration must supply these. To import the display
from a running Wayland session:

```sh
systemctl --user import-environment WAYLAND_DISPLAY
systemctl --user daemon-reload
systemctl --user enable --now clipmunge
```

Importing the variable does not configure session startup. If your setup uses
a different session target, adjust the service's `PartOf`, `After` and
`WantedBy` accordingly, or start `clipmunge` from your compositor's autostart.

The default notification command is `notify-send`. Install `libnotify` and
make it available on the service's `PATH` if your rules send notifications.
You can also set an absolute path in `notify_command`, or use `--no-notify`.

For startup errors and rule failures, read `journalctl --user -u clipmunge`.
