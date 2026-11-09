#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
platform="${1:-}"
if [[ "${platform}" != "macos" && "${platform}" != "tvos" ]]; then
  printf 'usage: %s {macos|tvos}\n' "${0##*/}" >&2
  exit 2
fi

if [[ -n "${CACHE_SERVER_BINARY:-}" ]]; then
  export CACHE_SERVER_BINARY
elif [[ "${CI:-}" == "true" || "${LOCAL_BUILD_MODE:-}" == "isolated" ]]; then
  "${ROOT_DIR}/scripts/build-cache-server.sh"
  CACHE_SERVER_BINARY="${ROOT_DIR}/target/release/tvos-net-player-cache-server"
  export CACHE_SERVER_BINARY
else
  CACHE_SERVER_BINARY="$(python3 "${ROOT_DIR}/scripts/local_build.py" prepare server --print-path server)"
  if [[ -z "${CACHE_SERVER_BINARY}" ]]; then
    printf 'local server preparation returned an empty CACHE_SERVER_BINARY\n' >&2
    exit 1
  fi
  export CACHE_SERVER_BINARY
fi

exec python3 "${ROOT_DIR}/scripts/ui-e2e.py" --platform "${platform}"
