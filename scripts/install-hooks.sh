#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOOKS_DIR="$(git -C "${ROOT_DIR}" rev-parse --git-path hooks)"

mkdir -p "${HOOKS_DIR}"
install -m 0755 "${ROOT_DIR}/scripts/hooks/pre-commit" "${HOOKS_DIR}/pre-commit"

echo "Installed pre-commit hook at ${HOOKS_DIR}/pre-commit"
