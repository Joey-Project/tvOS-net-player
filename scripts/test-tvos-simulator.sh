#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_PATH="${PROJECT_PATH:-${ROOT_DIR}/TVOSNetPlayer.xcodeproj}"
SCHEME="${SCHEME:-TVOSNetPlayer}"
CONFIGURATION="${CONFIGURATION:-Debug}"
DERIVED_DATA_PATH="${DERIVED_DATA_PATH:-${ROOT_DIR}/build/DerivedData}"

if [[ -n "${TVOS_TEST_DESTINATION:-}" ]]; then
  DESTINATION="${TVOS_TEST_DESTINATION}"
else
  SIMULATOR_ID="$(
    xcrun simctl list devices available |
      awk '
        /^-- tvOS / { in_tvos = 1; next }
        /^-- / { in_tvos = 0 }
        in_tvos && match($0, /[0-9A-Fa-f-]{8}-[0-9A-Fa-f-]{4}-[0-9A-Fa-f-]{4}-[0-9A-Fa-f-]{4}-[0-9A-Fa-f-]{12}/) {
          print substr($0, RSTART, RLENGTH)
          exit
        }
      '
  )"

  if [[ -z "${SIMULATOR_ID}" ]]; then
    echo "No available tvOS simulator was found. Install a tvOS runtime or set TVOS_TEST_DESTINATION manually." >&2
    exit 1
  fi

  DESTINATION="id=${SIMULATOR_ID}"
fi

xcodebuild \
  -skipPackagePluginValidation \
  -project "${PROJECT_PATH}" \
  -scheme "${SCHEME}" \
  -configuration "${CONFIGURATION}" \
  -destination "${DESTINATION}" \
  -derivedDataPath "${DERIVED_DATA_PATH}" \
  CODE_SIGNING_ALLOWED=NO \
  test
