# Changelog

All notable changes to vmfleet are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and vmfleet adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `vmfleet self-update` — update the binary in place from the latest published
  GitHub Release: downloads the tarball for this host's target triple, verifies its
  SHA-256, atomically swaps the running binary, then runs `install --upgrade`
  (config migration + unit rewrite) and restarts the supervisor. Flags: `--check`,
  `--tag <vX.Y.Z>`, `--allow-prerelease`, `--yes`, `--no-restart`. Alias: `update`.
- Passive update-available check in the supervisor (config `[supervisor]
  update_check` / `update_check_interval_hours`): logs once, adds `update_available`
  and `latest_version` to `status.json`, exposes the Prometheus gauge
  `vmfleet_update_available`, and prints a hint in `vmfleet status`. Notify only —
  never auto-installs.
- `build.rs` embeds the compilation target triple (`VMFLEET_TARGET`) so self-update
  always selects the matching release asset.
- `CHANGELOG.md` and `RELEASING.md`.

### Changed
- Command naming pass for consistency (old names kept as aliases): `gc` → `prune`,
  `config-check` → `check`. The internal `supervisor` / `worker` subcommands are now
  hidden from `--help`.

## [0.1.0]

Initial release: single-host autoscaling fleet of ephemeral Multipass-VM GitHub
Actions runners. Guided install/uninstall, resource-aware admission gate, warm-pool
autoscaling, orphan GC, native GitHub REST client, tag-driven cross-platform release
pipeline.

[Unreleased]: https://github.com/gagalo1234/vmfleet/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/gagalo1234/vmfleet/releases/tag/v0.1.0
