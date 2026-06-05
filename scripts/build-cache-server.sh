#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

dotnet build "${ROOT_DIR}/CacheServer/TVOSNetPlayer.CacheServer/TVOSNetPlayer.CacheServer.csproj" --configuration Release
