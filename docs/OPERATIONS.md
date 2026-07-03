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
vmfleet gc     # purge orphan vmfleet VMs + delete stale offline runner records
```

Consider a periodic timer:

```ini
# ~/.config/systemd/user/vmfleet-gc.timer  (+ matching .service running `vmfleet gc`)
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

## Uninstall

```bash
vmfleet uninstall              # stop fleet, purge vmfleet VMs/runners/units (keep config+base)
vmfleet uninstall --purge-all  # also remove base VM, config, token, state
```

## Reboot survival

vmfleet enables `loginctl enable-linger <user>` at install so the supervisor (a
user service) starts at boot without an interactive login. Verify with
`loginctl show-user $USER --property=Linger`.
