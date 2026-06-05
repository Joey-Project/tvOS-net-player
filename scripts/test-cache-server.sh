#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

dotnet run --project "${ROOT_DIR}/CacheServer/TVOSNetPlayer.CacheServer.Tests/TVOSNetPlayer.CacheServer.Tests.csproj" --configuration Release
