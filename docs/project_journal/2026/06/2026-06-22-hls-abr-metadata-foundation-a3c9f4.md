---
id: 20260622-a3c9f4
title: HLS ABR Metadata Foundation
status: completed
created: 2026-06-22
updated: 2026-06-22
branch: wip/hls-abr-metadata-foundation
pr:
supersedes:
  - 20260621-c9f0a2
superseded_by:
---

# HLS ABR Metadata Foundation

## Summary

- PR 7 persists BBDown playback ABR metadata in server-owned HLS session manifests without changing current single-variant playback behavior.
- HLS manifests now carry ABR groups, level metadata, all planned variant metadata, and per-media cache keys for future multi-variant master playlists and adaptive policy.
- Completed HLS manifest sanitization still removes upstream URLs, backup URLs, and headers while retaining non-secret ABR/cache-key metadata.

## Current State

- `HlsPlaybackSession` still serves the selected runtime `HlsVariant` for AVPlayer playback.
- New metadata fields record ABR groups and candidate variants beside the selected runtime variant.
- `HlsCacheStore` persists and restores the metadata with backward-compatible defaults for older manifests.
- Playback task planning now writes BBDown adapter ABR metadata into runtime and persisted HLS sessions.

## Next Steps

- Continue with PR 8: emit multi-variant HLS master playlists from persisted ABR metadata.
- Keep cache-first and upstream fallback behavior single-variant until PR 8 changes playback behavior deliberately.

## Evidence

- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server hls_session_manifest --lib`
- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server completed_session_manifest_scrubs_upstream_request_data --lib`
- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server create_bilibili_playback_task_returns_preparing_and_plans_hls_session_in_background --lib`
- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server --lib`
- `scripts/format.sh`
- `scripts/lint.sh`
- `git diff --check`
- `python3 /Users/joey/.codex/skills/project-journal/scripts/project_journal.py validate --repo /Users/joey/Program/Codex-workspace/tvOS-net-player`
- `just ci` (live Bilibili e2e remains opt-in and ignored by default)
