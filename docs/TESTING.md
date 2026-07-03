# Testing

vmfleet orchestrates three things that are awkward to test together — systemd
`--user` services, Multipass VMs, and the GitHub runner API. Rather than mock the
world, the code is built around narrow seams so most of it can be tested hermetically:

- **`cmd::Runner`** ([src/cmd.rs](../src/cmd.rs)) abstracts "run a command, get output",
  so systemd/Multipass calls swap for a recording mock in tests.
- **`github.api_base`** ([src/config.rs](../src/config.rs)) is configurable, so the real
  `github::Client` can be pointed at a localhost fixture server — no network, no token.

Shared scaffolding (the recording runner + fixture GitHub server) lives in
[src/testsupport.rs](../src/testsupport.rs).

## The pyramid

| Layer | What it covers | Where it runs | Free on hosted Actions? |
|---|---|---|---|
| **L1 unit** | scale math, config validation, naming, admission gate, exact systemd arg vectors | `cargo test` | ✅ (already in `ci`) |
| **L2 offline e2e** | one full `reconcile` pass: launch / scale-down / admission-blocked / dry-run, driven through the real `Client` against a localhost fixture, systemd mocked | `cargo test` ([src/e2e_offline.rs](../src/e2e_offline.rs)) | ✅ |
| **L3 CLI smoke** | the compiled binary end-to-end for the no-infra surface: `--version`/`--help`, `config-check` (valid+invalid), preview-flag presence | `cargo test --test cli_smoke` ([tests/cli_smoke.rs](../tests/cli_smoke.rs)) | ✅ |
| **L4 live smoke** | real Multipass VM launch + real runner registration | your own host with Multipass, as a self-hosted runner | ❌ — hosted runners have no reliable nested KVM |

L1–L3 are all driven by `cargo test --all`, which the [`ci`](../.github/workflows/ci.yml)
workflow runs on every push/PR. Because they touch no Multipass, systemd or network,
they cost nothing on GitHub-hosted runners (and public-repo minutes are unlimited).

## Running each layer

```bash
cargo test --all               # L1 + L2 + L3
cargo test --test cli_smoke    # just the CLI smoke tests
just check                     # fmt + clippy + the whole suite (mirrors CI)
```

**L4 (live)** cannot run on GitHub-hosted runners — Multipass needs nested
virtualisation the standard runners don't provide. Run it on a machine that has
Multipass and is registered as a self-hosted runner with the `live` label, via the
manual [`live-smoke`](../.github/workflows/live-smoke.yml) workflow
(`workflow_dispatch`). It runs `doctor` + `supervisor --once --dry-run` by default and
can optionally rebuild the base image. Locally, the same preview is just:

```bash
vmfleet supervisor --once --dry-run     # print the reconcile decision; no side effects
```

## Adding to the offline e2e

To cover a new reconcile branch, add a `#[test]` in [src/e2e_offline.rs](../src/e2e_offline.rs):
build a config with `config(&fx, pools_toml)`, seed the fixture with a runners JSON and
the mock with `RecordingRunner::with_active_units(...)`, call `run_once(...)`, then assert
on the returned `Report` (decision) and `runner.calls()` (the systemd commands emitted).
Tune `cfg.admission` / `cfg.supervisor` to force the branch you want (e.g. `min_avail_mib
= u64::MAX` to exercise the admission block).
