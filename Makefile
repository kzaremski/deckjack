# Deckjack's everyday commands.
#
# Thin on purpose: each target is one or two commands, and the logic lives in
# `cargo xtask` (xtask/src/main.rs), which is what knows that the web bundle
# has to exist before the core compiles, and what works on a machine without
# make. `make help` lists the targets.
#
# Variables:
#   PREFIX=DIR   where `make install` installs (default $DECKJACK_HOME, else
#                ~/.deckjack)
#   PORT=N       the dev core's port for `make dev` (default 8777)
#   DEV_HOME=DIR the dev install and working directory (default ~/.deckjack-dev)

CARGO ?= cargo
NPM ?= npm

INSTALL_ARGS := $(if $(PREFIX),--prefix $(PREFIX))
DEV_ARGS := $(if $(PORT),--port $(PORT)) $(if $(DEV_HOME),--home $(DEV_HOME))

.DEFAULT_GOAL := help
.PHONY: help build release web test test-rust test-web lint fmt check dev install install-debug run

help:
	@echo "make build          debug build: the web bundle if stale, then cargo build"
	@echo "make release        the same, release"
	@echo "make web            just the web bundle (web/dist)"
	@echo "make test           every Rust and web test"
	@echo "make lint           cargo fmt --check, clippy -D warnings, vue-tsc"
	@echo "make fmt            cargo fmt"
	@echo "make check          lint, then test: what must pass before a commit"
	@echo "make dev            core (--debug, :8777, ~/.deckjack-dev) and Vite (:5273) together"
	@echo "make install        release build into ~/.deckjack (PREFIX=... to change)"
	@echo "make install-debug  the same, debug build"
	@echo "make run            run the installed server in the foreground"

build:
	$(CARGO) xtask build

release:
	$(CARGO) xtask build --release

web:
	$(CARGO) xtask web

test: test-rust test-web

test-rust:
	$(CARGO) test --workspace

test-web:
	cd web && $(NPM) test

lint:
	$(CARGO) fmt --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	cd web && $(NPM) run typecheck

fmt:
	$(CARGO) fmt

check: lint test

dev:
	$(CARGO) xtask dev $(DEV_ARGS)

install:
	$(CARGO) xtask install $(INSTALL_ARGS)

install-debug:
	$(CARGO) xtask install --debug $(INSTALL_ARGS)

# The installed core, serving its default directory. Foreground; Ctrl+C stops
# it. DECKJACK_HOME is honored by the binary itself.
run:
	"$${DECKJACK_HOME:-$$HOME/.deckjack}/bin/deckjack"
