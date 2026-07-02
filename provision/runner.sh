#!/usr/bin/env bash
# Sample base-VM provisioning: install the GitHub Actions runner binary into
# ~ubuntu/actions-runner (unconfigured — vmfleet registers it per-VM at runtime).
# RUNNER_VERSION is exported by `vmfleet build-base` ("latest" or e.g. "2.317.0").
set -euo pipefail

RUNNER_VERSION="${RUNNER_VERSION:-latest}"
ARCH="x64"   # or arm64
USER_HOME="/home/ubuntu"

if [ "$RUNNER_VERSION" = "latest" ]; then
  RUNNER_VERSION="$(curl -fsSL https://api.github.com/repos/actions/runner/releases/latest \
    | jq -r .tag_name | sed 's/^v//')"
fi

url="https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-${ARCH}-${RUNNER_VERSION}.tar.gz"

sudo -u ubuntu bash -lc "
  set -euo pipefail
  mkdir -p '$USER_HOME/actions-runner'
  cd '$USER_HOME/actions-runner'
  curl -fsSL '$url' -o runner.tar.gz
  tar xzf runner.tar.gz && rm runner.tar.gz
  test ! -f .runner   # must stay unconfigured (template)
"
# install runner OS dependencies
"$USER_HOME/actions-runner/bin/installdependencies.sh"
echo "installed actions runner v${RUNNER_VERSION}"
