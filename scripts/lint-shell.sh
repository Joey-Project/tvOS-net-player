#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

scripts=()
while IFS= read -r script; do
  scripts+=("${script}")
done < <(find "${ROOT_DIR}/scripts" -type f \( -name '*.sh' -o -path '*/hooks/*' \) -print | sort)

if [[ "${#scripts[@]}" -eq 0 ]]; then
  exit 0
fi

for script in "${scripts[@]}"; do
  bash -n "${script}"
done

if command -v shellcheck >/dev/null 2>&1; then
  shellcheck "${scripts[@]}"
else
  echo "shellcheck is not installed; validated shell syntax with bash -n only." >&2
fi
