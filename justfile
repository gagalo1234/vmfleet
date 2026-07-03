# vmfleet dev workflow. Requires `just` (https://github.com/casey/just).
# No `just`? The same commands live in the Makefile — run `make check` / `make update`.

# Where the dev binary is installed, and the supervisor unit it feeds.
bin := env_var_or_default("VMFLEET_BIN", env_var("HOME") / ".local/bin/vmfleet")
sup := "vmfleet-supervisor.service"

# List available recipes.
default:
    @just --list

# fmt + clippy + tests — mirrors CI. Run before pushing.
check:
    cargo fmt --all --check
    cargo clippy --all-targets -- -D warnings
    cargo test --all

# Build the release binary.
build:
    cargo build --release

# Preview one reconcile cycle as JSON without touching the fleet. Pass extra args
# through, e.g. `just plan --config /path/to/vmfleet.toml`.
plan *ARGS: build
    ./target/release/vmfleet supervisor --once --dry-run {{ARGS}}

# First-time local dev install (README Quickstart): build, install the binary,
# guided config, base image, health check. After this the supervisor runs on your
# host — you are now running the dev build.
dev-install: build
    install -m755 target/release/vmfleet {{bin}}
    {{bin}} install
    {{bin}} build-base
    {{bin}} doctor

# Hot-swap the dev binary and restart the supervisor. Safe with jobs in flight:
# workers are independent transient units, so running jobs are not interrupted.
update: build
    install -m755 target/release/vmfleet {{bin}}
    systemctl --user restart {{sup}}
    systemctl --user status --no-pager {{sup}}

# Safe hot-swap: self-check (fmt/clippy/tests) and a dry-run preview first; if
# either fails, `just` stops before touching the running fleet.
update-safe: check plan update
