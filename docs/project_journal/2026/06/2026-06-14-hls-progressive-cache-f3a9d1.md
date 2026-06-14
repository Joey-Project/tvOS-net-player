---
id: 20260614-f3a9d1
title: HLS Progressive Cache
status: active
created: 2026-06-14
updated: 2026-06-14
branch: wip/hls-progressive-media-pipeline
pr:
supersedes: []
superseded_by:
---

# HLS Progressive Cache

## Summary

- Deliver progressive Bilibili playback through the LAN cache server while keeping gRPC as the control plane and AVPlayer-compatible HTTP/HLS as the media plane.
- Use `BBDown-rust` `v0.2.0` playback planning APIs as the resolver layer. The cache server owns playback session lifecycle, HLS playlist/segment generation, caching, recovery, and optional LAN-side transcoding.
- Land the work as four independently reviewable PRs. Each PR must pass the full local test gate, CI, `independent-codex-pr-review`, `offline-frozen-diff-review`, and required GitHub review/conversation-resolution gates before merge. After each merge, update `master` before branching for the next PR.

## Current State

- `BBDown-rust` `v0.2.0` exposes playback planning with media URLs, backup URLs, request headers, codec/mime metadata, cache keys, ABR groups, and AVPlayer-oriented selection hints.
- The current LAN cache server adapter still uses the older complete-download path: BBDown core downloads selected media, then the server muxes a completed MP4 and publishes it as a library item.
- The cache server now has a BBDown playback-planning adapter foundation that maps core playback plans into server-owned DTOs and selects AVPlayer-friendly variants for later progressive sessions.
- The cache server now exposes progressive playback through `TaskService.CreateBilibiliPlaybackTask`: it creates a persisted `TASK_KIND_BILIBILI_PROGRESSIVE_PLAYBACK` task and returns it in `preparing` state immediately, while BBDown playback planning runs in the background.
- The runtime HLS media path registers an in-memory HLS session after planning succeeds, publishes a `playable` task with `BilibiliPlaybackSession` metadata plus a HLS `PlaybackSource`, serves master/media playlists under `/hls/{session_id}`, and proxies selected DASH video/audio requests with BBDown headers and client Range.
- The Swift cache client exposes `getTask(id:)` and `watchTasks(ids:)` so tvOS code can track background playback planning to a playable HLS source without repeating create calls.
- Runtime HLS passthrough does not persist media manifests yet. Restored `playable` tasks are marked failed after restart so callers can retry instead of receiving stale HLS URLs; PR 4 owns durable manifests and offline recovery.
- Playback planning rejects Bilibili short links until `bbdown-core` exposes a resolved-input API that lets the server choose the correct default selection after short-link expansion.
- Existing architecture already reserves HLS playlists and segments over HTTP as the media-plane direction, so progressive playback should extend the current boundary rather than introduce gRPC media streaming or direct tvOS BBDown integration.

## PR Plan

### PR 1: Upgrade BBDown 0.2.0 + Playback Planning Adapter

- Status: merged in PR #9.
- Upgrade `bbdown-core` to `v0.2.0`.
- Add a playback-planning adapter path without changing the existing complete-download MP4 task behavior.
- Map `PlaybackPlan`, `PlaybackVariant`, and `MediaRequestSpec` into cache-server-owned DTOs.
- Implement an AVPlayer-friendly variant selection policy, starting with `PlaybackCodecPreference::avplayer_default()` and a conservative H.264/AAC fallback where needed.
- Add focused Rust tests and update architecture docs for the BBDown core/server boundary.

### PR 2: Progressive Playback Control Plane

- Status: merged in PR #10.
- Extend the gRPC schema and task state model for progressive playback tasks and playback URLs.
- Persist progressive task/session metadata so planning and early playback state can survive server restart.
- Add task lifecycle states for planned, preparing, playable, completed, and failed progressive sessions.
- Reserve HTTP playlist and segment routes without requiring the full media pipeline yet.
- Update Swift client types/compile gates for the new control-plane fields and background task tracking APIs.

### PR 3: HLS Progressive Media Pipeline

- Status: implemented by this slice.
- Build the server-side pipeline that consumes `MediaRequestSpec` URLs and headers, fetches Bilibili media, and produces AVPlayer-compatible HLS playlists and segments.
- Start with runtime passthrough HLS for selected DASH MP4 video/audio requests. Transmuxing/transcoding remains a later expansion once the runtime URL path is proven.
- Support backup URLs, source retry, and deterministic fixture-based e2e tests so CI does not depend on live Bilibili availability.
- Add a local tvOS simulator AVPlayer smoke test when practical.

### PR 4: Offline Cache Finalization + Recovery

- Persist segment/cache manifests and restore playable or completed progressive items after server restart.
- Finalize progressive sessions into stable offline library items for weak-network or disconnected LAN playback.
- Add cache integrity checks, partial-item cleanup, and basic eviction metadata hooks.
- Update user-facing architecture docs and run the full local and remote merge gates.

## Next Steps

- After PR 3 lands, continue with PR 4 durable HLS/cache manifests and offline recovery.
- Keep the existing complete-download adapter path as the fallback until progressive HLS is proven by tests.
- Do not merge a PR while required CI is failing, review findings remain actionable, or GitHub reports unresolved review conversations.

## Evidence

- `BBDown-rust` `v0.2.0` release: https://github.com/Joey-Project/BBDown-rust/releases/tag/v0.2.0
- Current BBDown adapter journal: `docs/project_journal/2026/06/2026-06-09-bbdown-rust-adapter-b4e2c8.md`
- Cache server architecture: `docs/architecture/cache-server.md`
- PR 1 local gate:
  - `cargo test --package tvos-net-player-cache-server --locked`
  - `cargo clippy --package tvos-net-player-cache-server --all-targets --locked -- -D warnings`
  - `just ci`
- PR 1 local review:
  - `.codex-tmp/isolated-review-uf1kx88d` found the short-link selection issue.
  - `.codex-tmp/isolated-review-t9fdywvs` returned `LGTM` after the short-link fix.
- PR 2 local gate:
  - `cargo test --package tvos-net-player-cache-server --locked`
  - `scripts/test.sh`
  - `cargo clippy --package tvos-net-player-cache-server --all-targets --locked -- -D warnings`
  - `scripts/build-for-testing.sh`
  - `just ci`
- PR 2 local review:
  - `.codex-tmp/isolated-review-fvob3olr` found that synchronous playback planning could be cancelled by the Swift client's 10s unary deadline.
  - `.codex-tmp/isolated-review-ygrzyyxy` found that the async planning fix also needed Swift `GetTask`/`WatchTasks` APIs for callers to retrieve planned playback metadata.
  - `.codex-tmp/isolated-review-41b1nakv` found that PR 2 should not expose a playable HLS `PlaybackSource` before the HLS media route is implemented.
  - `.codex-tmp/isolated-review-cfh3mc19` found that background playback planning needed a global concurrency limit.
  - `.codex-tmp/isolated-review-f6jd79bj` returned `LGTM` after the planned-source and concurrency fixes.
  - `.codex-tmp/pr10-independent-codex-pr-review.md` found that cancellation while waiting for the planning semaphore left the task active until a permit became available, and found stale architecture wording that still described returning a HLS `PlaybackSource` in PR 2.
  - The PR 2 fix polls cancellation while waiting for the planning permit, adds a regression test for cancelled waiters, and updates the architecture doc to match the `PREPARING` -> `PLANNED` metadata-only contract.
  - PR #10 merged on 2026-06-14 as `88578f8`.
- PR 3 local gate:
  - `scripts/format.sh`
  - `cargo test --package tvos-net-player-cache-server --locked hls -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked hls_segment_proxies_upstream_media_with_required_headers_and_range -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked`
  - `cargo clippy --package tvos-net-player-cache-server --all-targets --locked -- -D warnings`
  - `scripts/test.sh`
  - `just ci`
- PR 3 review fixes:
  - Repeated progressive playback creates are request-scoped. They now create fresh `preparing` tasks instead of deduping against active playback tasks, so each HLS `PlaybackSource.uri` is derived from the current gRPC request.
  - HLS media playlist probing now caps initialization body reads to the advertised 1 MiB scan window, even when upstream omits `Content-Length`.
  - HLS segment proxying now rejects ranged upstream responses that ignore `Range`, omit `Content-Range`, or return a `Content-Range` that does not match the requested byte range, and treats them as retryable backup URL failures.
  - HLS upstream fetches now use bounded connect/read timeouts so stalled CDN attempts fail over to backup URLs or 502 instead of holding playlist/segment handlers indefinitely.
