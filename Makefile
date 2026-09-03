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

# Not $(PREFIX)/lib/systemd/user. systemd looks in different places depending
# on who installed the unit, and the two are separate namespaces rather than
# one path with a prefix swapped — systemd.unit(5), Table 2:
#
#   /usr/lib/systemd/user         distribution package manager
#   /usr/local/lib/systemd/user   administrator
#   ~/.local/share/systemd/user   "packages installed in the home directory"
#
# There is no ~/.local/lib/systemd/user on that list, so the obvious template
# puts the unit somewhere `systemctl --user enable` will never look, and says
# nothing while doing it. /usr/share/systemd/user is searched as well, but only
# through XDG_DATA_DIRS, which anybody may set to something else; a packager
# wants the unconditional lib path.
ifeq ($(filter /usr%,$(PREFIX)),)
UNITDIR ?= $(PREFIX)/share/systemd/user
else
UNITDIR ?= $(PREFIX)/lib/systemd/user
endif

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
	@# ExecStart is rewritten rather than left for the reader to notice. The
	@# shipped unit says %h/.local/bin/clipmunge, which is wrong for every
	@# PREFIX but one, and a unit that points at nothing fails at enable time
	@# with an error about a path nobody typed.
	$(INSTALL) -d $(DESTDIR)$(UNITDIR)
	sed -e 's|^ExecStart=.*|ExecStart=$(BINDIR)/clipmunge|' \
	    systemd/clipmunge.service > $(DESTDIR)$(UNITDIR)/clipmunge.service
	chmod 644 $(DESTDIR)$(UNITDIR)/clipmunge.service
	@echo
	@echo 'clipmunge: unit    -> $(DESTDIR)$(UNITDIR)/clipmunge.service'
	@echo 'clipmunge: ExecStart set to $(BINDIR)/clipmunge'
	@echo 'clipmunge: then    systemctl --user daemon-reload'
	@echo 'clipmunge:         systemctl --user enable --now clipmunge'

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/clipmunge
	rm -f $(DESTDIR)$(MANDIR)/man1/clipmunge.1
	rm -f $(DESTDIR)$(MANDIR)/man5/clipmunge.5
	rm -f $(DESTDIR)$(DOCDIR)/config.lua.example
	rm -f $(DESTDIR)$(UNITDIR)/clipmunge.service

clean:
	$(CARGO) clean

# Ask where things would go without installing them: `make show PREFIX=…`.
.PHONY: show
show:
	@echo 'PREFIX  = $(PREFIX)'
	@echo 'BINDIR  = $(BINDIR)'
	@echo 'MANDIR  = $(MANDIR)'
	@echo 'DOCDIR  = $(DOCDIR)'
	@echo 'UNITDIR = $(UNITDIR)'
