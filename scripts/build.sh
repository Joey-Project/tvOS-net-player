#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_PATH="${PROJECT_PATH:-${ROOT_DIR}/TVOSNetPlayer.xcodeproj}"
SCHEME="${SCHEME:-TVOSNetPlayer}"
CONFIGURATION="${CONFIGURATION:-Debug}"
DESTINATION="${TVOS_SIMULATOR_DESTINATION:-generic/platform=tvOS Simulator}"
DERIVED_DATA_PATH="${DERIVED_DATA_PATH:-${ROOT_DIR}/build/DerivedData}"

xcodebuild \
  -onlyUsePackageVersionsFromResolvedFile \
  -skipPackagePluginValidation \
  -project "${PROJECT_PATH}" \
  -scheme "${SCHEME}" \
  -configuration "${CONFIGURATION}" \
  -destination "${DESTINATION}" \
  -derivedDataPath "${DERIVED_DATA_PATH}" \
  CODE_SIGNING_ALLOWED=NO \
  build
