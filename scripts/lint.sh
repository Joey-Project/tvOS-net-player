#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

"${ROOT_DIR}/scripts/lint-shell.sh"
"${ROOT_DIR}/scripts/lint-swift-format.sh"
"${ROOT_DIR}/scripts/lint-dotnet-format.sh"
