---
id: 20260622-e6b4a8
title: HLS Multi-Variant Master
status: completed
created: 2026-06-22
updated: 2026-06-22
branch: wip/hls-multi-variant-master
pr:
supersedes:
  - 20260622-a3c9f4
superseded_by:
---

# HLS Multi-Variant Master

## Summary

- PR 8 emits runtime HLS master playlists with multiple AVPlayer-safe DASH variants when BBDown provides compatible H.264/AAC candidates.
- Each alternate variant gets stable variant-specific playlist and media resource IDs such as `v1-video.m3u8` and `v1-video.m4s`.
- Existing HLS media handlers remain cache-first and then upstream-fallback; the lookup path is now variant-aware because every playable resource has its own cache/resource ID.

## Decisions

- The selected variant keeps the existing `video.m3u8` / `audio.m3u8` and `video.m4s` / `audio.m4s` IDs for compatibility with existing manifests and tests.
- Alternate runtime variants are limited to DASH variants with H.264 video and AAC audio metadata. HEVC/AV1 variants stay out of the master playlist until platform capability and transcoding policy are explicit.
- Completed offline HLS manifests still guarantee only the selected cached variant. When a session is finalized, upstream URLs/headers are scrubbed and runtime alternate variants are removed so offline master playlists cannot advertise uncached variants.

## Implementation

- Added `HlsPlaybackSession.alternate_variants` and backward-compatible persistence with `#[serde(default)]`.
- Built multi-variant master generation from the selected variant plus safe alternates, with per-variant audio groups and variant-specific media playlist IDs.
- Updated media playlist/resource lookup to iterate all playable variants, allowing the existing cache/prewarm/upstream fallback path to work per variant.
- Extended completed-manifest sanitization to clear alternate variants after finalization.

## Validation

- `cargo fmt --manifest-path CacheServer/RustCacheServer/Cargo.toml`
- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server hls::tests:: --lib`
- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server hls_cache::tests:: --lib`
- `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server --lib`

## Follow-Ups

- PR 9 should move toward segment-level fill/prefetch so the server can cache or prewarm variant windows instead of only selected whole resources.
- PR 10 should add adaptive weak-network policy that can downgrade to lower safe variants and surface the reason in shared UI.
- PR 11 should add the server-side transcoding boundary for media that cannot be safely exposed as AVPlayer-compatible HLS.
