#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

"${ROOT_DIR}/scripts/lint.sh"

if [[ "${PRE_COMMIT_RUN_BUILD:-0}" == "1" ]]; then
  "${ROOT_DIR}/scripts/build.sh"
fi

if [[ "${PRE_COMMIT_RUN_TESTS:-0}" == "1" ]]; then
  "${ROOT_DIR}/scripts/test.sh"
fi
