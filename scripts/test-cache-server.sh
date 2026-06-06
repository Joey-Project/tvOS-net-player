#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cargo test \
  --manifest-path "${ROOT_DIR}/Cargo.toml" \
  --package tvos-net-player-cache-server \
  --locked
