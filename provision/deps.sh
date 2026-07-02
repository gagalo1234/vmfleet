#!/usr/bin/env bash
# Sample base-VM provisioning: system dependencies your CI jobs need.
# Runs inside the base VM (as root, via `vmfleet build-base`). Customize freely.
set -euo pipefail

apt-get update
apt-get install -y --no-install-recommends \
  build-essential ca-certificates curl file git jq tar zstd

# Add your project's build deps here, e.g. for Tauri/GTK desktop packaging:
# apt-get install -y --no-install-recommends \
#   libgtk-3-dev libwebkit2gtk-4.1-dev librsvg2-dev libssl-dev pkg-config patchelf
