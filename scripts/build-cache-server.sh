#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ "$(uname -s)" == "Darwin" && "${LOCAL_BUILD_MODE:-}" != "isolated" && "${CI:-}" != "true" ]]; then
  exec python3 "${ROOT_DIR}/scripts/local_build.py" build server
fi

cargo build \
  --manifest-path "${ROOT_DIR}/Cargo.toml" \
  --package tvos-net-player-cache-server \
  --release \
  --locked
