# Operations

## Daily

```bash
vmfleet status                       # pools, workers, resources
vmfleet doctor                       # health / preflight
journalctl --user -u vmfleet-supervisor.service -f      # supervisor logs
journalctl --user -u 'vmfleet-worker-*' -f              # worker logs
```

## Tuning

```bash
vmfleet scale small --min 3 --max 20   # retune a pool (supervisor picks up next cycle)
```

Edit `~/.config/vmfleet/vmfleet.toml` for admission thresholds
(`min_avail_mib`, `psi_max`, `min_disk_gib`) or supervisor cadence; then
`systemctl --user restart vmfleet-supervisor`.

## Base image

```bash
vmfleet build-base            # first build
vmfleet build-base --force    # rebuild (purges the old base first)
```

The base disk size should fit your heaviest pool — clones inherit the cap. If a
small pool shouldn't carry a huge disk, give it its own smaller base (a second
`[base]`-style image) rather than shrinking the shared one.

## Orphans / stuck teardowns

Multipass can occasionally wedge on `delete` (hitting the systemd stop timeout and
leaving a Stopped/Running orphan VM). vmfleet bounds teardown timeouts to avoid the
long hang, and:

```bash
vmfleet prune  # purge orphan vmfleet VMs + delete stale offline runner records (alias: gc)
```

Consider a periodic timer:

```ini
# ~/.config/systemd/user/vmfleet-gc.timer  (+ matching .service running `vmfleet prune`)
[Timer]
OnCalendar=hourly
```

## Storage on a dedicated disk

To scale the pool large, host the multipass vault on a big disk and point
`storage.vault_path` at it (the admission disk gate reads that path). Migrating the
snap vault is a host-level, root operation (stop multipass → move
`/var/snap/multipass/common/data` → bind-mount back → start); do it in a quiet
window.

## Upgrades

**Preferred — in-place self-update from a published release:**

```bash
vmfleet self-update --check    # report whether a newer release exists
vmfleet self-update            # download + verify sha256 + swap binary + migrate + restart
```

`self-update` fetches the GitHub Release tarball for this host's target triple,
verifies its `.sha256`, atomically replaces the running binary, then re-runs
`install --upgrade` (config migration + unit rewrite) and restarts the supervisor.
Useful flags: `--tag vX.Y.Z` (pin a version), `--yes` (no prompt), `--no-restart`
(swap only), `--allow-prerelease`.

The supervisor also runs a periodic, best-effort **update check** (config:
`[supervisor] update_check`, default on; `update_check_interval_hours`, default 24)
and *notifies only* — it never auto-installs. When behind, it emits a log line, adds
`update_available` / `latest_version` to `status.json`, sets the Prometheus gauge
`vmfleet_update_available`, and `vmfleet status` prints a one-line hint.

**Fallback — build from source** (e.g. an unreleased commit, or air-gapped hosts):

```bash
git pull && cargo build --release && install -m755 target/release/vmfleet ~/.local/bin/vmfleet
vmfleet install --upgrade      # reinstall unit / migrate config
systemctl --user restart vmfleet-supervisor
```

Cutting a release (maintainers): see [../RELEASING.md](../RELEASING.md).

## Dev dogfooding loop

Running the dev build yourself and fixing issues in place is a first-class workflow.
The recipes live in the [`justfile`](../justfile) (or the [`Makefile`](../Makefile) if
you don't have `just`).

**First-time local install** — build, install the binary, guided config, base image,
health check. After this the supervisor runs on your host and you are running the dev
build:

```bash
just dev-install          # == README Quickstart; or `make dev-install`
```

**Fast iterate** — after a change, self-check then hot-swap:

```bash
just plan                 # preview one reconcile cycle as JSON — no side effects
just update-safe          # fmt+clippy+tests + dry-run first; abort on failure, else hot-swap
just update               # hot-swap without the self-check
```

The hot-swap (`systemctl --user restart vmfleet-supervisor`) is **safe with jobs in
flight**: workers are independent transient units with no `Restart=`, so restarting
the supervisor never interrupts a running job — the new binary just takes over
scheduling on the next cycle. A second supervisor can't start by accident either: it
holds an exclusive `supervisor.lock` singleton (a `--dry-run` preview deliberately
skips the lock so it can run alongside the live one).

**Which command for which change** (one binary is both supervisor and worker):

| Changed | Command | How it takes effect |
|---|---|---|
| supervisor / scaling logic | `just update` | next reconcile cycle |
| worker logic (`src/worker.rs`) | `just update` | newly launched workers use it; in-flight ones finish on the old binary |
| base image / provisioning (`provision/`) | `just update` + `vmfleet build-base --force` | new VMs use the new image; running VMs are unchanged |
| config (pools/labels/thresholds) | `vmfleet scale …`, or edit the TOML + restart | no rebuild needed |

**Isolating a staging fleet.** Everything vmfleet creates is namespaced under a single
fixed `vmfleet-` prefix (fixed unit names, one config/state path), so **two fleets under
the same Linux user collide** (same supervisor/worker unit names, `gc` could sweep each
other's VMs). To run a dev/staging fleet next to a production one: use a **separate host**,
or a **separate Linux user** (its own systemd user instance, `linger`, and XDG dirs), and
point it at a **different repo/org** or at least **distinct pool labels** so GitHub routes
jobs to the right fleet.

See [TESTING.md](TESTING.md) for the full test strategy.

## Authentication / re-auth

`vmfleet install` authenticates via GitHub's OAuth **device flow** (browser-authorize,
like `gh auth login`) and stores the token at `[github] token_file`. To rotate or
refresh credentials without reinstalling:

```bash
vmfleet login                 # browser device flow (re-auth); re-scopes from repo/org
vmfleet login --with-token    # paste a PAT instead (no-browser / CI / GHES)
```

`login` reads the fleet's config for scope + token path, writes the new token 0600, and
validates it against the API before returning. For GHES or a custom OAuth App, set
`VMFLEET_OAUTH_CLIENT_ID` / `VMFLEET_OAUTH_BASE` (see [CONFIG.md](CONFIG.md#authentication)).

## Uninstall

```bash
vmfleet uninstall              # stop fleet, purge vmfleet VMs/runners/units (keep config+base)
vmfleet uninstall --purge-all  # also remove base VM, config, token, state
```

## Reboot survival

vmfleet enables `loginctl enable-linger <user>` at install so the supervisor (a
user service) starts at boot without an interactive login. Verify with
`loginctl show-user $USER --property=Linger`.
