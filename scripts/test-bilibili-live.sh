#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

export BILIBILI_LIVE_E2E_FIXTURE="${BILIBILI_LIVE_E2E_FIXTURE:-${ROOT_DIR}/.agents/skills/bilibili-live-e2e/references/live-cases.json}"

cargo test \
  --manifest-path "${ROOT_DIR}/Cargo.toml" \
  --package tvos-net-player-cache-server \
  --test bilibili_live_e2e \
  --locked \
  -- --ignored --nocapture
