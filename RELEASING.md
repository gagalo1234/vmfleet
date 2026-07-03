# Releasing vmfleet

vmfleet follows [Semantic Versioning](https://semver.org/) and a trunk-based,
tag-driven, human-gated release flow. The version in `Cargo.toml` is the single
source of truth; CI enforces that the release tag matches it.

## How a release ships (overview)

```
bump Cargo.toml  ->  PR  ->  merge to main  ->  tag vX.Y.Z  ->  release.yml (draft)
                                                                      |
                                          reviewer-gated promote.yml (publish)
                                                                      |
                                        users: `vmfleet self-update`
```

- **`release.yml`** (on tag `v*.*.*`): verifies the tag matches `Cargo.toml` and is
  an ancestor of `origin/main`, runs fmt/clippy/test, cross-compiles four targets
  (`{x86_64,aarch64}-unknown-linux-{gnu,musl}`), packages each as
  `vmfleet-<version>-<target>.tar.gz` + a `.sha256`, and stages a **draft** Release.
- **`promote.yml`** (`workflow_dispatch`, `production` environment / required
  reviewer): verifies the draft and its checksums, then flips it to published.
- Publishing the release is what makes `vmfleet self-update` (and the supervisor's
  passive check) offer it — drafts and prereleases are ignored by default.

## Steps

1. **Choose the version** per SemVer (MAJOR breaking / MINOR feature / PATCH fix).
2. **On a branch** (never push to `main` directly — a pre-push hook blocks it):
   - Bump `version` in `Cargo.toml`.
   - Move `CHANGELOG.md` `[Unreleased]` entries under a new `## [X.Y.Z]` heading and
     update the compare links at the bottom.
   - `cargo build` so `Cargo.lock` updates.
3. **Open a PR** and merge it to `main` once CI is green.
4. **Tag the merged commit** and push the tag:
   ```bash
   git checkout main && git pull
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
   `release.yml` builds the artifacts into a draft Release.
5. **Promote**: run the `promote` workflow (Actions → promote → Run workflow) with
   `tag = vX.Y.Z`. A reviewer approves the `production` environment; the release is
   published.
6. **Verify**: on a runner host, `vmfleet self-update --check` should now report the
   new version; `vmfleet self-update` installs it.

## Prereleases

Tag with a suffix, e.g. `v0.3.0-rc.1`. `release.yml` marks the draft as a prerelease
automatically. Users opt in with `vmfleet self-update --allow-prerelease` or
`--tag v0.3.0-rc.1`.

## Notes

- The asset name embeds the target triple; `vmfleet self-update` selects the asset
  matching the running binary's `VMFLEET_TARGET` (set at build time by `build.rs`).
- Never retag a published version. To fix a bad release, publish a new patch.
