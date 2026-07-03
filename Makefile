# Zero-dependency fallback for the justfile (see `justfile` for the primary flow).
# Prefer `just` if you have it; these mirror the same recipes.

BIN ?= $(HOME)/.local/bin/vmfleet
SUP := vmfleet-supervisor.service

.PHONY: check build plan dev-install update update-safe

# fmt + clippy + tests — mirrors CI. Run before pushing.
check:
	cargo fmt --all --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all

build:
	cargo build --release

# Preview one reconcile cycle without touching the fleet. Extra args: `make plan ARGS="--config /p"`.
plan: build
	./target/release/vmfleet supervisor --once --dry-run $(ARGS)

# First-time local dev install (README Quickstart).
dev-install: build
	install -m755 target/release/vmfleet $(BIN)
	$(BIN) install
	$(BIN) build-base
	$(BIN) doctor

# Hot-swap the dev binary and restart the supervisor (in-flight jobs keep running).
update: build
	install -m755 target/release/vmfleet $(BIN)
	systemctl --user restart $(SUP)
	systemctl --user status --no-pager $(SUP)

# Safe hot-swap: self-check + dry-run preview first, abort on failure.
update-safe: check plan update
