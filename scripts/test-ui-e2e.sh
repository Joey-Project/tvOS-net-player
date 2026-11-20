#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ "$#" -lt 1 || "$#" -gt 2 ]]; then
  printf 'usage: %s {macos|tvos} [local-signed|isolated]\n' "${0##*/}" >&2
  exit 2
fi

platform="$1"
if [[ "${platform}" != "macos" && "${platform}" != "tvos" ]]; then
  printf 'usage: %s {macos|tvos} [local-signed|isolated]\n' "${0##*/}" >&2
  exit 2
fi

if [[ "$#" -eq 2 ]]; then
  mode="$2"
  if [[ "${mode}" != "local-signed" && "${mode}" != "isolated" ]]; then
    printf 'usage: %s {macos|tvos} [local-signed|isolated]\n' "${0##*/}" >&2
    exit 2
  fi
elif [[ "${CI:-}" == "true" || "${LOCAL_BUILD_MODE:-}" == "isolated" || "${platform}" == "tvos" ]]; then
  mode="isolated"
else
  mode="local-signed"
fi

if [[ "${mode}" == "local-signed" && "${CI:-}" == "true" ]]; then
  printf 'local-signed UI E2E mode is not allowed in CI\n' >&2
  exit 2
fi
if [[ "${mode}" == "local-signed" && "${platform}" != "macos" ]]; then
  printf 'local-signed UI E2E mode is available only for macOS\n' >&2
  exit 2
fi

if [[ "${mode}" == "isolated" ]]; then
  if [[ -n "${CACHE_SERVER_BINARY:-}" ]]; then
    export CACHE_SERVER_BINARY
  else
    LOCAL_BUILD_MODE=isolated "${ROOT_DIR}/scripts/build-cache-server.sh"
    CACHE_SERVER_BINARY="${ROOT_DIR}/target/release/tvos-net-player-cache-server"
    export CACHE_SERVER_BINARY
  fi
fi

exec python3 "${ROOT_DIR}/scripts/ui-e2e.py" --platform "${platform}" --mode "${mode}"
