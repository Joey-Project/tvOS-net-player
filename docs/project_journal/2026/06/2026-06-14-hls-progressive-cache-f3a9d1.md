---
id: 20260614-f3a9d1
title: HLS Progressive Cache
status: completed
created: 2026-06-14
updated: 2026-06-14
branch: wip/offline-cache-finalization
pr: https://github.com/Joey-Project/tvOS-net-player/pull/12
supersedes: []
superseded_by:
---

# HLS Progressive Cache

## Summary

- Deliver progressive Bilibili playback through the LAN cache server while keeping gRPC as the control plane and AVPlayer-compatible HTTP/HLS as the media plane.
- Use `BBDown-rust` playback planning APIs as the resolver layer. The cache server owns playback session lifecycle, HLS playlist/segment generation, caching, recovery, and optional LAN-side transcoding.
- Land the work as four independently reviewable PRs. Each PR must pass the full local test gate, CI, `independent-codex-pr-review`, `offline-frozen-diff-review`, and required GitHub review/conversation-resolution gates before merge. After each merge, update `master` before branching for the next PR.

## Current State

- `BBDown-rust` `v0.3.0` is now the pinned core dependency; playback planning still provides media URLs, backup URLs, request headers, codec/mime metadata, cache keys, ABR groups, and AVPlayer-oriented selection hints.
- The current LAN cache server adapter still uses the older complete-download path: BBDown core downloads selected media, then the server muxes a completed MP4 and publishes it as a library item.
- The cache server now has a BBDown playback-planning adapter foundation that maps core playback plans into server-owned DTOs and selects AVPlayer-friendly variants for later progressive sessions.
- The cache server now exposes progressive playback through `TaskService.CreateBilibiliPlaybackTask`: it creates a persisted `TASK_KIND_BILIBILI_PROGRESSIVE_PLAYBACK` task and returns it in `preparing` state immediately, while BBDown playback planning runs in the background.
- The runtime HLS media path persists a server-owned HLS session manifest after planning succeeds, registers a runtime HLS session, publishes a `playable` task with `BilibiliPlaybackSession` metadata plus a HLS `PlaybackSource`, serves master/media playlists under `/hls/{session_id}`, and proxies selected DASH video/audio requests with BBDown headers and client Range when no local cached resource exists.
- The HLS cache finalizer stores selected video/audio resources under `Cache:RootPath/.tvos-net-player/hls/{session_id}`, records per-resource size/init-range/cache-key metadata, marks fully cached tasks `completed`, and exposes offline Bilibili HLS library items as `bilibili.hls.<session_id>`.
- Startup now restores HLS session manifests into the runtime HLS registry. Persisted `playable`/`completed` progressive tasks remain usable when their manifest exists; only tasks missing a matching manifest are failed during startup reconcile.
- The Swift cache client exposes `getTask(id:)` and `watchTasks(ids:)` so tvOS code can track background playback planning to a playable HLS source without repeating create calls.
- Playback planning rejects Bilibili short links until `bbdown-core` exposes a resolved-input API that lets the server choose the correct default selection after short-link expansion.
- Feed/history/watch-later style inputs added by `BBDown-rust` `v0.3.0` are accepted through the adapter with latest-item defaults; richer explicit selection and multi-item results remain deferred until the task options/result schema is designed.
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

- Status: merged in PR #11.
- Build the server-side pipeline that consumes `MediaRequestSpec` URLs and headers, fetches Bilibili media, and produces AVPlayer-compatible HLS playlists and segments.
- Start with runtime passthrough HLS for selected DASH MP4 video/audio requests. Transmuxing/transcoding remains a later expansion once the runtime URL path is proven.
- Support backup URLs, source retry, and deterministic fixture-based e2e tests so CI does not depend on live Bilibili availability.
- Add a local tvOS simulator AVPlayer smoke test when practical.

### PR 4: Offline Cache Finalization + Recovery

- Status: implemented by this slice.
- Persist HLS session manifests and restore playable or completed progressive items after server restart.
- Finalize selected progressive media resources into stable offline Bilibili HLS library items for weak-network or disconnected LAN playback.
- Add cached-resource size/init-range/cache-key metadata as integrity checks, partial retry cleanup, and basic eviction metadata hooks.
- Update user-facing architecture docs and run the full local and remote merge gates.

## Next Steps

- Keep the existing complete-download adapter path as the fallback until progressive HLS is proven on physical Apple TV and real Bilibili sources.
- Add explicit cache eviction policy, cache management UI/API, and optional LAN-side transmux/transcode in later work.
- Do not merge a PR while required CI is failing, review findings remain actionable, or GitHub reports unresolved review conversations.

## Evidence

- `BBDown-rust` `v0.2.0` release: https://github.com/Joey-Project/BBDown-rust/releases/tag/v0.2.0
- `BBDown-rust` `v0.3.0` release: https://github.com/Joey-Project/BBDown-rust/releases/tag/v0.3.0
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
- PR 3 merged on 2026-06-14 as `3ac607d`.
- PR 4 focused local validation:
  - `cargo test --package tvos-net-player-cache-server --locked hls_cache -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked cached -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked hls -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked playback_task_finalizes_cached_hls_library_item_and_restores_after_restart -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked list_library_items_paginates_from_hls_cache_to_local_library -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked hls_cache_finalizer_removes_cache_when_task_was_cancelled -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked playable_progressive -- --nocapture`
  - `cargo fmt --all -- --check`
  - `cargo clippy --package tvos-net-player-cache-server --all-targets --locked -- -D warnings`
  - `cargo test --package tvos-net-player-cache-server --locked`
  - `just ci`
- PR 4 review fixes:
  - Startup restore now keeps `PLAYABLE` tasks restorable with a session manifest, but requires `COMPLETED` tasks to have a complete HLS cache item.
  - Offline HLS finalization now rejects range-only media requests, unsolicited partial responses, and cached bodies whose size does not match BBDown/Core metadata or `Content-Length`.
  - Startup restore now resumes cache finalization for manifest-backed `PLAYABLE` HLS sessions that had not reached a completed offline library item before restart.
  - Offline HLS finalization now removes temporary resource files when post-download MP4 initialization validation fails.
  - Startup restore now repairs the crash window where HLS resources were fully cached but the progressive task had not yet persisted its `COMPLETED` state.
  - Startup restore now clears stale `library_item_id` values when a previously completed HLS cache task can no longer restore its offline library item.
  - Startup restore now removes unrestorable completed HLS sessions from the runtime registry so corrupted offline sessions do not leave active media routes.
  - Runtime HLS playback now remains `PLAYABLE` if manifest persistence fails, and offline HLS finalizers now use bounded concurrency plus task-state cancellation checks.
  - Restart-resumed HLS finalizers now fail restored `PLAYABLE` tasks, hide their runtime routes, and remove the HLS session directory when offline cache finalization cannot complete, preventing orphaned upstream request manifests.
  - HLS cache identifiers now reject `.` and `..` dot segments before path construction.
  - Completed offline HLS playback tasks now rewrite transient playback sources to the offline `library_item_id` so polling clients keep a playback entrypoint.
  - Startup restore now hides unauthorized cancelled, failed, corrupted, and orphaned disk HLS sessions from runtime routes and library listings instead of deleting their manifests/resources; explicit cancel paths still clean up cache data.
  - HLS cache finalization now retries backup URLs after a downloaded MP4 fails local initialization validation.
  - HLS cache finalization and runtime HLS media requests now filter host-scoped sensitive headers such as `Authorization` and `Cookie` when retrying cross-origin backup URLs.
  - Completed offline HLS session manifests now scrub upstream URLs, backup URLs, and request headers after the selected resources are fully cached.
  - Startup restore now rewrites fully cached crash-window HLS manifests to the scrubbed completed form before marking tasks `COMPLETED`.
  - Successful HLS cache finalization now replaces the in-memory runtime HLS session with a cache-only session so completed offline playback cannot fall back to upstream proxying with original request credentials.
  - Playback planning now marks progressive tasks `PLAYABLE` before persisting the upstream HLS session manifest, so cancelled or failed planning cannot leave unowned URL/header-bearing manifests on disk.
  - HLS cache cancellation now performs idempotent session cleanup after committed resources and in the finalizer cancellation branch, preventing partially cached video/audio resources from surviving cancellation races.
  - Local library enumeration now excludes the internal `.tvos-net-player/hls` cache tree even when `.m4s` is explicitly allowed as a local-media extension.
  - Startup restore now treats HLS cache scan failures as indeterminate instead of empty, preserving persisted progressive tasks until the cache root is readable again.
  - Restored HLS playback tasks now refresh persisted `PlaybackSource.uri` values from the current media base, avoiding stale host/base URLs after restart or config changes.
  - Cached HLS resources now reject symlinked resource files and open cached media through the same no-follow file-open helper used by local media playback.
  - Local library direct item resolution now rejects internal `.tvos-net-player/hls` cache paths, not only recursive enumeration.
  - HLS cache writes, reads, and removals now reject symlinked store roots, session directories, temporary resource paths, and final resource targets before creating, renaming, or deleting cache files.
  - HLS cache read paths now reject symlinked session directories, `session.json` manifests, metadata JSON files, and resource parent paths before startup restore, library listing, or cached media lookup can trust on-disk HLS cache entries.
  - Restart restore now preserves the originally request-derived HLS playback URI when `Cache:PublicMediaBaseUri` is unset, while still refreshing restored URIs when an explicit public media base is configured.
  - Secure no-follow media opens now use the existing `openat` implementation on Unix platforms instead of macOS only, so completed offline HLS resources can be served on Linux/macOS without falling back to scrubbed upstream URLs.
  - HLS cache downloads now stop before writing any chunk that would exceed the expected media size, including chunked responses without `Content-Length`, preventing overlong upstream bodies from filling disk before validation fails.
  - Offline HLS cache downloads now reject resources with neither BBDown-provided size nor upstream `Content-Length`, so chunked responses must still have an independent expected-size bound.
  - Startup HLS cache scans and direct completed-item lookups now reject session manifests whose persisted `id` does not match the containing cache directory, preventing mismatched restore metadata from surviving recovery cleanup or aliasing another completed cache entry.
  - Progressive HLS planning now registers the runtime HLS session before publishing a `Playable` task event, so watchers can immediately fetch the advertised playlist URI without a transient 404.
  - Cached file responses now sanitize persisted content types before writing HTTP headers, falling back to `application/octet-stream` for invalid metadata instead of panicking.
  - HLS cache path validation now rejects symlinked root ancestors, matching the local library root policy before writes, removals, scans, or cached media reads trust cache paths.
  - Cached HLS resources now require persisted resource metadata cache keys to match the current session manifest request cache key before a resource can count toward a completed offline item.
  - Completed offline HLS cache playback is now gated on secure no-follow range-file support, so unsupported platforms keep progressive tasks on the live HLS proxy path and hide completed cache-only items instead of returning unusable scrubbed URLs.
  - Completed HLS library/source authorization now lazily registers a sanitized runtime HLS session from the cache store, so playback recovers after a startup cache scan failure once the cache root becomes readable again.
  - HLS media routes now use the same authorized lazy session restoration path, so a persisted task playback URL can recover after a startup cache scan failure without first calling the library/source APIs.
  - Startup restore now fails completed HLS playback tasks whose persisted `library_item_id` does not match `bilibili.hls.<task_id>`, avoiding terminal-but-hidden corrupted tasks.
  - Lazy completed-HLS restore now also fails a stale completed task after a startup cache scan failure once a media/library request can validate the cache again, clearing the stale playback source immediately instead of waiting for another restart.
  - Persisted cached-resource metadata now rejects invalid MP4 initialization byte ranges on read, so corrupted metadata cannot expose a completed item that later 502s when building its HLS media playlist.
  - `cargo test --package tvos-net-player-cache-server --locked load_sessions_skips_manifest_with_mismatched_directory_id -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked get_completed_library_item_skips_manifest_with_mismatched_directory_id -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked cached_file_response_invalid_content_type_uses_octet_stream -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked rejects_hls_cache_root_symlink_ancestor -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked cached_resource_rejects_mismatched_request_cache_key -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked completed_hls_items_are_hidden_when_cache_playback_is_unsupported -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked playback_task_stays_playable_when_cache_playback_is_unsupported -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked completed_hls_source_registers_session_after_cache_scan_recovers -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked app_state_fails_completed_hls_task_with_stale_library_item_id -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked app_state_ -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked hls_segment_serves_cached_resource_with_range -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked hls_media_playlist_uses_cached_initialization_without_upstream_probe -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked stale_completed_hls_task_fails_after_cache_scan_recovers -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked cached_resource_rejects_invalid_initialization_length_metadata -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked app_state_resumes_incomplete_hls_cache_finalization_after_restart -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked app_state_completes_playable_hls_task_when_cache_finished_before_restart -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked removes_temp_file_when_cached_initialization_is_invalid -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked app_state_fails_restored_hls_task_when_cache_finalization_fails -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked rejects_dot_segments_as_hls_cache_ids -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked completed_progressive_playback_cache_rewrites_runtime_source_to_library_item -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked app_state_hides_cancelled_hls_cache_session_after_restart -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked app_state_hides_hls_cache_when_task_state_snapshot_is_unreadable -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked tries_backup_url_after_cached_initialization_is_invalid -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked filters_sensitive_media_request_headers_for_cross_origin_backups -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked does_not_forward_sensitive_headers_to_cross_origin_backup_url -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked completed_session_manifest_scrubs_upstream_request_data -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked`
  - `just ci` after independent review fixes; log: `.codex-tmp/pr4-final-just-ci-after-independent-fixes.log`
  - `cargo test --package tvos-net-player-cache-server --locked app_state_scrubs_completed_hls_manifest_during_restart_recovery -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked hls_cache_finalizer_sanitizes_runtime_session_after_completion -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked cancelled_playback_planning_does_not_persist_hls_manifest -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked cancellation_after_committed_resource_removes_partial_session -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked local_scan_excludes_internal_hls_cache_files -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked hls_cache_finalizer_stops_when_task_is_cancelled_during_download -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked`
  - `cargo test --package tvos-net-player-cache-server --locked` after GitHub Codex review-gate fixes: 128 lib tests and 6 integration tests passed.
  - `cargo fmt --all -- --check`
  - `git diff --check`
  - `just ci` after second independent review fixes; log: `.codex-tmp/pr4-final-just-ci-after-independent-rerun-fixes.log`
  - `just ci` after third independent review fixes; log: `.codex-tmp/pr4-final-just-ci-after-independent-third-fixes-rerun.log`
  - `just ci` after GitHub Codex review-gate fixes; log: `.codex-tmp/pr4-final-just-ci-after-github-codex-gate-fixes.log`
  - `cargo test --package tvos-net-player-cache-server --locked symlink -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked hls_cache -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked` after symlink read hardening: 136 lib tests and 6 integration tests passed.
  - `cargo fmt --all -- --check`
  - `git diff --check`
  - Project journal validation passed.
  - `just ci` after symlink read hardening; log: `.codex-tmp/pr4-final-just-ci-after-symlink-read-fixes.log`
  - `cargo test --package tvos-net-player-cache-server --locked preserves_restored_hls_uri -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked refreshes_restored_hls_uri -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked preserves_existing_restored_hls_uri -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked` after restored URI preservation: 139 lib tests and 6 integration tests passed.
  - `cargo fmt --all -- --check`
  - `git diff --check`
  - Project journal validation passed.
  - `just ci` after restored URI preservation; log: `.codex-tmp/pr4-final-just-ci-after-restored-uri-fix.log`
  - `cargo test --package tvos-net-player-cache-server --locked restored_hls_uri -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked root_availability_rejects_symlink_ancestor -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked hls_segment_serves_cached_resource_with_range -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked` after Unix no-follow open support: 139 lib tests and 6 integration tests passed.
  - `cargo fmt --all -- --check`
  - `git diff --check`
  - Project journal validation passed.
  - `just ci` after Unix no-follow open support; log: `.codex-tmp/pr4-final-just-ci-after-unix-nofollow-fix.log`
  - `cargo test --package tvos-net-player-cache-server --locked` after symlink write hardening: 132 lib tests and 6 integration tests passed.
  - `cargo fmt --all -- --check`
  - `git diff --check`
  - Project journal validation passed.
  - `just ci` after symlink write hardening; log: `.codex-tmp/pr4-final-just-ci-after-symlink-write-fixes.log`
  - `cargo test --package tvos-net-player-cache-server --locked symlink -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked hls_cache -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked rejects_lengthless_chunked_hls_cache_response -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked app_state_fails_restored_hls_task_when_cache_finalization_fails -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked rejects_overlong_chunked_hls_cache_response_with_expected_size -- --nocapture`
  - `cargo test --package tvos-net-player-cache-server --locked` after final review fixes: 140 lib tests and 6 integration tests passed.
  - `cargo fmt --all -- --check`
  - `git diff --check`
  - Project journal validation passed.
  - `just ci` after final review fixes; log: `.codex-tmp/pr4-final-just-ci-after-final-review-fixes.log`
  - `cargo test --package tvos-net-player-cache-server --locked` after lengthless-download hardening: 141 lib tests and 6 integration tests passed.
  - `cargo test --package tvos-net-player-cache-server --locked` after final review hardening: 150 lib tests and 6 integration tests passed.
  - `cargo test --package tvos-net-player-cache-server --locked` after media-route lazy restore: 150 lib tests and 6 integration tests passed.
  - `cargo test --package tvos-net-player-cache-server --locked` after final independent review restore hardening: 152 lib tests and 6 integration tests passed.
  - `cargo fmt --all -- --check`
  - `git diff --check`
  - Project journal validation passed.
  - `just ci` after lengthless-download hardening; log: `.codex-tmp/pr4-final-just-ci-after-lengthless-download-fix.log`
  - `just ci` after mismatched-manifest restore hardening; log: `.codex-tmp/pr4-final-just-ci-after-mismatched-manifest-fix.log`
  - `just ci` after final review hardening; log: `.codex-tmp/pr4-final-just-ci-after-final-review-hardening.log`
  - `just ci` after cache-key/root hardening; log: `.codex-tmp/pr4-final-just-ci-after-cache-key-root-hardening.log`
  - `just ci` after completed-HLS capability fix; log: `.codex-tmp/pr4-final-just-ci-after-completed-hls-capability-fix.log`
  - `just ci` after lazy completed-HLS restore fix; log: `.codex-tmp/pr4-final-just-ci-after-lazy-completed-hls-restore-fix.log`
  - `just ci` after media-route lazy restore fix; log: `.codex-tmp/pr4-final-just-ci-after-media-lazy-restore-fix.log`
  - `just ci` after final independent review restore hardening; log: `.codex-tmp/pr4-final-just-ci-after-final-independent-restore-hardening.log`
