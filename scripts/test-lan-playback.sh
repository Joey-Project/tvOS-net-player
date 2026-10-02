#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_DIR="${LAN_PLAYBACK_PROBE_BUILD_DIR:-${ROOT_DIR}/build/LanPlaybackProbe}"
PROBE_PATH="${BUILD_DIR}/probe-lan-playback"

mkdir -p "${BUILD_DIR}"
xcrun swiftc \
  -swift-version 6 \
  -strict-concurrency=complete \
  -warnings-as-errors \
  -parse-as-library \
  "${ROOT_DIR}/scripts/probe-lan-playback.swift" \
  -o "${PROBE_PATH}"

if [[ -z "${LAN_PLAYBACK_URL:-}" ]]; then
  exec "${PROBE_PATH}" --self-test
fi

exec "${PROBE_PATH}" \
  --url "${LAN_PLAYBACK_URL}" \
  --duration "${LAN_PLAYBACK_DURATION_SECONDS:-60}" \
  --deadline "${LAN_PLAYBACK_DEADLINE_SECONDS:-120}"
