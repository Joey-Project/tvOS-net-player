#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_PATH="${PROJECT_PATH:-${ROOT_DIR}/TVOSNetPlayer.xcodeproj}"
SCHEME="${SCHEME:-TVOSNetPlayer}"
CONFIGURATION="${CONFIGURATION:-Debug}"
DERIVED_DATA_PATH="${DERIVED_DATA_PATH:-${ROOT_DIR}/build/DerivedData}"
BUNDLE_IDENTIFIER="${PRODUCT_BUNDLE_IDENTIFIER:-dev.joey.tvos-net-player}"
DEVICE_IDENTIFIER="${TVOS_DEVICE_ID:-${TVOS_DEVICE:-}}"
TEAM_IDENTIFIER="${DEVELOPMENT_TEAM:-${APPLE_DEVELOPMENT_TEAM:-}}"
LAUNCH_AFTER_INSTALL="${LAUNCH_AFTER_INSTALL:-1}"

if [[ -z "${DEVICE_IDENTIFIER}" ]]; then
  echo "Set TVOS_DEVICE_ID to an Apple TV identifier from: xcrun devicectl list devices" >&2
  exit 1
fi

if [[ -z "${TEAM_IDENTIFIER}" ]]; then
  echo "Set DEVELOPMENT_TEAM to your Apple development team ID for device signing." >&2
  exit 1
fi

xcodebuild \
  -allowProvisioningUpdates \
  -skipPackagePluginValidation \
  -project "${PROJECT_PATH}" \
  -scheme "${SCHEME}" \
  -configuration "${CONFIGURATION}" \
  -destination "id=${DEVICE_IDENTIFIER}" \
  -derivedDataPath "${DERIVED_DATA_PATH}" \
  DEVELOPMENT_TEAM="${TEAM_IDENTIFIER}" \
  PRODUCT_BUNDLE_IDENTIFIER="${BUNDLE_IDENTIFIER}" \
  CODE_SIGN_STYLE=Automatic \
  build

APP_PATH="${DERIVED_DATA_PATH}/Build/Products/${CONFIGURATION}-appletvos/TVOSNetPlayer.app"

xcrun devicectl device install app \
  --device "${DEVICE_IDENTIFIER}" \
  "${APP_PATH}"

if [[ "${LAUNCH_AFTER_INSTALL}" == "1" ]]; then
  xcrun devicectl device process launch \
    --device "${DEVICE_IDENTIFIER}" \
    "${BUNDLE_IDENTIFIER}"
fi
