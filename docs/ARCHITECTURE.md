# Architecture

vmfleet is a single static binary invoked as different roles. It shells out to
`multipass` and `systemctl`/`systemd-run` (the OS interfaces) and talks to the
GitHub REST API natively over HTTPS.

```
                         ┌──────────────────────────┐
   systemd --user  ───►  │ vmfleet supervisor        │  (one long-running unit)
                         │  reconcile loop, poll=15s │
                         └──────────┬───────────────┘
                                    │ systemd-run --user (no Restart)
                    ┌───────────────┼───────────────┐
                    ▼               ▼               ▼
             vmfleet-worker-  vmfleet-worker-  vmfleet-worker-   (transient units)
                  101             102             201
                    │ each: one ephemeral VM, one job, then exit
                    ▼
             multipass clone → start → run.sh (JIT) → health-monitor → purge
```

## Roles

- **supervisor** — the control plane. Every cycle it reads GitHub runners + queued
  jobs + host resources and, per pool, drives the worker count toward
  `desired = clamp(min_warm, busy + queued + min_warm, max)`. It launches workers
  into free slots (capped by `max_launch_per_poll` and memory headroom) and stops
  idle workers that exceed `idle_timeout` when over-provisioned. It writes
  `status.json` each cycle.
- **worker** — one ephemeral VM's whole life: sweep stale siblings → **admission
  gate** → clone base → set cpus/mem → start → wait cloud-init → register
  (JIT or token) → `run.sh` → health-monitor → cleanup. Cleanup (deregister +
  purge VM) runs on normal exit, error, and on SIGTERM/SIGINT via a dedicated
  `sigwait` thread — the equivalent of the original bash `trap`.

## Scale-down for free

Worker transient units have **no `Restart=`**. An ephemeral runner takes exactly
one job then exits; its unit goes inactive and stays down until a later reconcile
decides to relaunch it. So load simply draining reduces the fleet to `min_warm`
with no explicit action. Active stop only happens for warm runners kept above
`min_warm` past `idle_timeout`.

## Admission gate (never OOM)

Idle VMs cost almost nothing (qemu commits lazily); danger comes only from many
*busy* VMs at once. Before every launch a worker takes a shared `flock`, which:

1. **serializes** launches so multipassd isn't stampeded and boots are staggered, and
2. **backpressures**: it waits until `MemAvailable ≥ min_avail_mib`, memory
   `PSI some avg10 < psi_max`, and vault free `≥ min_disk_gib`.

The gate is held across clone+start plus a short settle, then released. The
supervisor additionally estimates memory *headroom* to bound launches per cycle.
Combined with a large host swap, the host degrades to backpressure (jobs queue)
rather than OOM.

## Health monitor

`runner_state` distinguishes: **online** (healthy), **UNKNOWN** (the GitHub query
itself failed — host/proxy jitter, do *not* count), **ABSENT** (ephemeral
deregistered = zombie) / **offline**. ABSENT/offline past `offline_grace` reaps the
worker so systemd/supervisor recycle it — the only way to kill a zombie `run.sh`
that retries `acquirejob` forever.

## Namespacing / safety

Everything created is prefixed `vmfleet-` (VMs `vmfleet-<pool>-<slot>-<ts>-<pid>`,
units `vmfleet-worker-<slot>` / `vmfleet-supervisor`). Cleanup and `gc` only ever
act on this prefix, so vmfleet can never purge VMs/runners it doesn't own.

## Testability

Command execution goes through a `Runner` trait; the pure decision logic
(`admission::decide`, `supervisor::desired`, `headroom_vms`, all parsers) is unit
tested. `reconcile` is a thin impure shell over those pure functions.
