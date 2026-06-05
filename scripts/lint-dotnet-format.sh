#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

projects=(
  "${ROOT_DIR}/CacheServer/TVOSNetPlayer.CacheServer/TVOSNetPlayer.CacheServer.csproj"
  "${ROOT_DIR}/CacheServer/TVOSNetPlayer.CacheServer.Tests/TVOSNetPlayer.CacheServer.Tests.csproj"
)

for project in "${projects[@]}"; do
  dotnet format "${project}" --verify-no-changes --verbosity minimal
done
