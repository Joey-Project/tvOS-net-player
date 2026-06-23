---
id: 20260623-c1e4d8
title: HLS Segment Index Splitting
status: completed
created: 2026-06-23
updated: 2026-06-23
branch: wip/segment-index-hls-splitting
pr:
supersedes:
superseded_by:
---

# HLS Segment Index Splitting

## Summary

- Added conservative fMP4 fragment range parsing for completed cached resources.
- Persisted optional segment byte ranges in HLS cached-resource metadata with legacy fallback.
- Updated HLS media playlist generation to emit multiple `EXTINF`/`EXT-X-BYTERANGE` entries when verified fragment ranges are available.

## Decisions

- Segment splitting is currently completed-cache-only. Runtime upstream playback and first-window prewarm continue to use the existing single-range playlist shape.
- The parser only trusts top-level fMP4 `moof` plus following `mdat` byte ranges. Single-fragment files, malformed fragments, or unsupported layouts fall back to the single-range playlist instead of fabricating boundaries.
- Playlist requests consume persisted segment metadata and do not rescan large cached media files.
- Segment durations are distributed from known resource duration across verified fragment boundaries until a later PR adds deeper timing metadata parsing.

## Validation

- `cargo test -p tvos-net-player-cache-server media_playlist_ -- --nocapture`
- `cargo test -p tvos-net-player-cache-server segment -- --nocapture`
