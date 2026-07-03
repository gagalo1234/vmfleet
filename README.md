# vmfleet

Autoscaling fleet of **ephemeral Multipass-VM GitHub Actions runners** for a single
host. One static Rust binary, one TOML config, guided install and complete uninstall.

Each job runs in a fresh, disposable VM (clean isolation). A supervisor keeps a small
**warm pool** ready, **bursts** up to a cap when jobs queue, and **scales back down**
when idle — all bounded by a resource-aware **admission gate** so the host never OOMs.

## Why

Running self-hosted runners as a fixed set of always-on VMs wastes memory when idle
and blocks jobs when busy. vmfleet scales the fleet to demand and provisions exactly
when work arrives, while guaranteeing the host stays within a memory/disk budget.

## Requirements

- Linux with **systemd** (user services) and **[Multipass](https://multipass.run)**
- GitHub runner-admin access. Install authenticates you via **browser device flow**
  (like `gh auth login`) — no PAT to create by hand. A PAT with runner admin (repo:
  *Administration* RW, or org: *self-hosted runners* RW) still works as a fallback.
- `linger` enabled for your user (the installer does this) so services survive logout/reboot

No `gh`, no Python — vmfleet talks to the GitHub REST API directly.

## Quickstart

```bash
# 1. build (or grab a release binary)
cargo build --release && install -m755 target/release/vmfleet ~/.local/bin/vmfleet

# 2. guided install — prompts for repo/pools, authenticates via browser device
#    flow (or paste a PAT), installs the supervisor
vmfleet install

# 3. build the base VM image (one-time; from your provisioning manifest)
vmfleet build-base

# 4. check
vmfleet doctor
vmfleet status
```

Point your workflows at the pool labels, e.g.:

```yaml
jobs:
  build:
    runs-on: [self-hosted, self-hosted-small]
```

## Commands

| Command | What it does |
|---|---|
| `vmfleet install` | Guided (or `--non-interactive`) setup: config, linger, supervisor unit |
| `vmfleet login [--with-token]` | (Re)authenticate to GitHub via browser device flow; `--with-token` pastes a PAT instead |
| `vmfleet build-base` | Build/rebuild the base VM from the provisioning manifest |
| `vmfleet status` | Pools, workers, host resources (reads the supervisor's status.json) |
| `vmfleet doctor` | Preflight: multipass, token+scope, base VM, disk, memory, linger |
| `vmfleet scale <pool> --min N --max N` | Retune a pool at runtime |
| `vmfleet prune` | Purge orphan VMs / stale runner records (namespaced, safe; alias `gc`) |
| `vmfleet self-update [--check]` | Update the binary in place from the latest GitHub Release (checksum-verified), then migrate config/units + restart supervisor |
| `vmfleet uninstall [--purge-all]` | Stop the fleet; remove VMs/runners/units (+config/base) |
| `vmfleet supervisor` / `worker` | Internal: run by systemd; not called by hand |

Preview what the control loop would do right now, without touching the fleet:
`vmfleet supervisor --once --dry-run` prints one reconcile decision as JSON (safe to
run alongside the live supervisor). See [docs/TESTING.md](docs/TESTING.md) and the
dev loop in [docs/OPERATIONS.md](docs/OPERATIONS.md).

## Updating

```bash
vmfleet self-update --check     # is a newer release available?
vmfleet self-update             # download (verified), swap binary, migrate + restart
```

`self-update` downloads the release tarball for your exact target, verifies its
SHA-256, atomically replaces the running binary, then runs `install --upgrade` and
restarts the supervisor. The running supervisor also checks periodically and surfaces
"update available" in `vmfleet status`, `status.json`, and a Prometheus gauge — it
never auto-installs. See [docs/OPERATIONS.md](docs/OPERATIONS.md#upgrades) and, for
maintainers cutting a release, [RELEASING.md](RELEASING.md).

## How it scales

Per pool each cycle: `desired = clamp(min_warm, busy + queued + min_warm, max)`.
Workers are transient systemd units with **no `Restart=`**, so a finished ephemeral
worker simply stays down until a later cycle relaunches it — that's the scale-down.
Before any VM launch, the **admission gate** (a shared flock) serializes launches and
waits until `MemAvailable`, memory-PSI and vault disk are within budget.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and [docs/OPERATIONS.md](docs/OPERATIONS.md).

## Configuration

A single [`vmfleet.toml`](examples/vmfleet.toml) (default `~/.config/vmfleet/vmfleet.toml`)
replaces scattered env vars. Everything — repo/org, pools, labels, admission
thresholds, base-image provisioning — lives there. See [docs/CONFIG.md](docs/CONFIG.md).

## License

MIT.
