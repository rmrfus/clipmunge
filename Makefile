# Build and install the binary, docs and user service. See docs/install.md.

PREFIX  ?= /usr/local
BINDIR  ?= $(PREFIX)/bin
MANDIR  ?= $(PREFIX)/share/man
DOCDIR  ?= $(PREFIX)/share/doc/clipmunge

# systemd searches share/systemd/user for home installs and
# lib/systemd/user under /usr and /usr/local; see systemd.unit(5).
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

# Keep build separate so sudo make install does not compile as root.
install:
	@test -x '$(BIN)' || { echo 'clipmunge: $(BIN) is missing — run `make` first' >&2; exit 1; }
	$(INSTALL) -Dm755 $(BIN)                   $(DESTDIR)$(BINDIR)/clipmunge
	$(INSTALL) -Dm644 man/man1/clipmunge.1     $(DESTDIR)$(MANDIR)/man1/clipmunge.1
	$(INSTALL) -Dm644 man/man5/clipmunge.5     $(DESTDIR)$(MANDIR)/man5/clipmunge.5
	$(INSTALL) -Dm644 config.lua.example       $(DESTDIR)$(DOCDIR)/config.lua.example
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
