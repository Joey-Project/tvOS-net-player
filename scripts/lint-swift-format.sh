#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG_PATH="${SWIFT_FORMAT_CONFIG:-${ROOT_DIR}/.swift-format}"

if ! xcrun --find swift-format >/dev/null 2>&1; then
  echo "swift-format was not found through xcrun. Install Xcode with swift-format support." >&2
  exit 1
fi

xcrun swift-format lint \
  --strict \
  --configuration "${CONFIG_PATH}" \
  --recursive \
  --parallel \
  "${ROOT_DIR}/Package.swift" \
  "${ROOT_DIR}/Sources" \
  "${ROOT_DIR}/Tests" \
  "${ROOT_DIR}/TVOSNetPlayer" \
  "${ROOT_DIR}/TVOSNetPlayerTests"
