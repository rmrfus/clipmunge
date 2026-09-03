# Installing clipmunge on a box without nix.
#
# `cargo install` copies binaries and nothing else — no man pages, no unit, no
# example config — so the rest needs a rule of its own. Everything here honours
# the usual PREFIX and DESTDIR, which is the language an AUR or Debian packager
# already speaks. On nix, use the flake instead; the package installs all of
# this and rewrites the unit's ExecStart to the store path.
#
#   make && sudo make install                 # /usr/local
#   make && make install PREFIX="$HOME/.local"
#   make install DESTDIR="$pkgdir" PREFIX=/usr

PREFIX  ?= /usr/local
BINDIR  ?= $(PREFIX)/bin
MANDIR  ?= $(PREFIX)/share/man
DOCDIR  ?= $(PREFIX)/share/doc/clipmunge
UNITDIR ?= $(PREFIX)/lib/systemd/user

CARGO   ?= cargo
INSTALL ?= install

BIN := target/release/clipmunge

.PHONY: all build install uninstall clean

all: build

build:
	$(CARGO) build --release --locked

# Deliberately not dependent on `build`: this is the target run under sudo, and
# rebuilding as root leaves target/ owned by root for the rest of time.
install:
	@test -x '$(BIN)' || { echo 'clipmunge: $(BIN) is missing — run `make` first' >&2; exit 1; }
	$(INSTALL) -Dm755 $(BIN)                   $(DESTDIR)$(BINDIR)/clipmunge
	$(INSTALL) -Dm644 man/man1/clipmunge.1     $(DESTDIR)$(MANDIR)/man1/clipmunge.1
	$(INSTALL) -Dm644 man/man5/clipmunge.5     $(DESTDIR)$(MANDIR)/man5/clipmunge.5
	$(INSTALL) -Dm644 config.lua.example       $(DESTDIR)$(DOCDIR)/config.lua.example
	$(INSTALL) -Dm644 systemd/clipmunge.service $(DESTDIR)$(UNITDIR)/clipmunge.service
	@echo 'clipmunge: the unit ships with ExecStart=%h/.local/bin/clipmunge — edit it'
	@echo "clipmunge: to $(BINDIR)/clipmunge, or systemctl --user will fail on the path"

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/clipmunge
	rm -f $(DESTDIR)$(MANDIR)/man1/clipmunge.1
	rm -f $(DESTDIR)$(MANDIR)/man5/clipmunge.5
	rm -f $(DESTDIR)$(DOCDIR)/config.lua.example
	rm -f $(DESTDIR)$(UNITDIR)/clipmunge.service

clean:
	$(CARGO) clean
