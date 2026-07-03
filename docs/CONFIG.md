# Configuration reference

Single TOML file (default `~/.config/vmfleet/vmfleet.toml`). See
[examples/vmfleet.toml](../examples/vmfleet.toml) for a full annotated sample.

## `[github]`
| key | default | meaning |
|---|---|---|
| `repo` | — | `owner/name` for a repo-level fleet (xor `org`) |
| `org` | — | org login for an org-level fleet (xor `repo`) |
| `token_file` | — | path to PAT file; env `VMFLEET_TOKEN` overrides at runtime |
| `runner_group_id` | `1` | runner group for JIT registration |
| `api_base` | `https://api.github.com` | override for GitHub Enterprise Server |

PAT scopes: repo → *Administration: Read and write*; org → *Self-hosted runners: RW*.

## `[storage]`
| key | default | meaning |
|---|---|---|
| `vault_path` | `/var/snap/multipass/common` | filesystem the disk admission gate checks |

## `[admission]`
| key | default | meaning |
|---|---|---|
| `min_avail_mib` | `4096` | don't launch below this MemAvailable |
| `psi_max` | `20.0` | don't launch at/above this memory PSI some-avg10 (%) |
| `min_disk_gib` | `30` | don't launch below this vault free space |
| `settle_seconds` | `15` | hold launch lock this long after start (stagger) |
| `lock_wait_seconds` | `60` | base wait for the launch lock |
| `poll_seconds` | `10` | backpressure re-check interval |

## `[base]`
| key | default | meaning |
|---|---|---|
| `image` | — | multipass image alias, e.g. `24.04` |
| `name` | `vmfleet-base` | base VM name / clone source |
| `disk` | `80G` | base disk (clones inherit; size for heaviest pool) |
| `cpus` / `memory` | `4` / `8G` | base build resources |
| `runner_version` | `latest` | exported as `RUNNER_VERSION` to provision scripts |
| `setup_scripts` | `[]` | scripts run in order inside the base VM by `build-base` (paths relative to the config file) |

## `[[pool]]` (one per runner class)
| key | default | meaning |
|---|---|---|
| `name` | — | pool id (no dashes) |
| `labels` | — | runner labels; workflows target these via `runs-on` |
| `cpus` / `memory` | — | per-VM resources |
| `swap` | — | optional in-VM swapfile size |
| `min_warm` | `0` | always-ready idle runners |
| `max` | — | hard cap for this pool |
| `slot_base` | — | first slot number; range `slot_base..slot_base+max` must not overlap other pools |

## `[supervisor]`
| key | default | meaning |
|---|---|---|
| `poll_seconds` | `15` | reconcile interval |
| `idle_timeout_seconds` | `900` | stop over-provisioned idle workers after this |
| `max_launch_per_poll` | `2` | stagger: max launches per cycle |
| `busy_reserve_mib` | `2048` | per-VM busy working-set estimate for headroom math |
| `use_jit` | `true` | JIT registration (no long-lived token inside VMs) |
| `update_check` | `true` | periodically check GitHub Releases and surface (never auto-install) when a newer vmfleet is available |
| `update_check_interval_hours` | `24` | how often to run the passive update check |

## `[worker]`
| key | default | meaning |
|---|---|---|
| `config_timeout_seconds` | `180` | in-VM config/setup timeout |
| `registration_timeout_seconds` | `180` | wait for runner to register after `run.sh` |
| `offline_grace_seconds` | `90` | reap a runner offline/absent this long (zombie) |
| `health_poll_seconds` | `10` | health-monitor interval |
| `cloud_init_timeout_seconds` | `180` | cloud-init readiness timeout |
