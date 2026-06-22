---
id: 20260621-c9f0a2
title: BBDown 0.5 And Progressive HLS Roadmap
status: active
created: 2026-06-21
updated: 2026-06-22
branch: wip/bbdown-050-roadmap-upgrade
pr:
supersedes:
  - 20260615-c8f4d2
  - 20260614-f3a9d1
  - 20260616-a9d4c1
  - 20260619-b7e3f1
superseded_by:
---

# BBDown 0.5 And Progressive HLS Roadmap

## Summary

- Carry the next product phase as sequential PRs: BBDown 0.5 upgrade, native download progress/cancellation, richer Bilibili fetch coverage, authenticated page validation, progressive HLS ABR, weak-network policy, and LAN transcoding foundation.
- Keep the LAN cache server as the only Bilibili integration point visible to tvOS/macOS clients. Clients receive LAN HTTP/HLS playback sources and never fetch Bilibili media URLs directly.
- Use the macOS app as the primary practical validation client for this phase. Physical Apple TV validation is explicitly deferred and is not part of this roadmap.
- Keep gRPC as the control plane and HTTP/HLS/Range URLs as the media plane.

## Current State

- PR 0 pins the cache server's `bbdown-core` dependency to `BBDown-rust` `v0.5.0`.
- PR 1 maps BBDown native complete-download progress/cancellation into server task state.
- Progressive Bilibili playback, HLS passthrough, durable HLS cache, watermark eviction, weak-network fill scheduling, Bonjour discovery, macOS/tvOS Bilibili task UI, and multi-result selection are implemented.
- The live e2e skill records canonical ordinary video, multi-part video, Bangumi media, and Bangumi episode samples. Restricted Bangumi validation has passed locally with a private credential file and a web-mode restricted API proxy.
- Public BiliRoaming reverse proxies are tracked as web-mode restricted API proxies and must not be paired with BBDown TV playurl mode.

## Decisions

- PR 0 should first upgrade the embedded BBDown dependency to `v0.5.0` and refresh stale docs, without taking on progress/cancellation mapping yet.
- BBDown native `DownloadProgressEvent` and `DownloadCancellationToken` should be a separate PR so task-state semantics can be reviewed independently from the dependency bump.
- Authenticated web-page fetch validation should stay opt-in and local because it depends on real credentials, account state, and upstream Bilibili availability.
- Collection/list fetch validation should focus on stable selection identity, candidate truncation, and avoiding accidental full-list fan-out.
- ABR work should start by persisting metadata and cache keys before changing playback behavior.
- Multi-variant HLS, segment-level prefetch, adaptive weak-network policy, and LAN transcoding should land as separate PRs because each changes playback/cache behavior and needs focused tests.

## PR Plan

### PR 0: Upgrade BBDown 0.5.0 Baseline

- Status: implemented by this slice.
- Upgrade `bbdown-core` to `BBDown-rust` `v0.5.0`.
- Prefer builder-style BBDown APIs where needed to absorb new pre-1.0 fields.
- Keep existing playback and complete-download behavior unchanged.
- Re-run deterministic cache-server tests, Swift/macOS tests, and the existing opt-in Bilibili live e2e suite.
- Refresh architecture and project docs for the new baseline and macOS validation direction.

### PR 1: Native BBDown Progress And Cancellation

- Status: implemented by `docs/project_journal/2026/06/2026-06-21-bbdown-native-progress-cancellation-d1a7b4.md`.
- Map `DownloadProgressEvent` into `BilibiliTaskProgress` for complete-download tasks.
- Bridge `BilibiliTaskCancellation` into `DownloadCancellationToken`.
- Report terminal success, failure, and cancellation using BBDown summaries instead of only coarse adapter phases.
- Preserve existing persisted task-state semantics and add cancellation/progress regression tests.

### PR 2: Download Options Schema

- Add backward-compatible control-plane fields for audio language, AI subtitle policy, and sidecar/danmaku controls.
- Map the new options into BBDown `StreamSelection`, `SubtitleAiPolicy`, and sidecar settings.
- Add shared AppCore models and basic tvOS/macOS controls.
- Keep existing defaults unchanged.

### PR 3: Credential Control-Plane Foundation

- Expose redacted credential health through the LAN cache server without returning secrets.
- Cover WEB cookie, generic `access_key`, and TV `tv_access_key` status.
- Prepare server-owned credential profile/login flows for later work while keeping actual credential files local to the Mac mini.
- Add macOS-first status UX for validation and debugging.

### PR 4: Authenticated Page Fetch Live Fixtures

- Status: implemented by `docs/project_journal/2026/06/2026-06-21-authenticated-page-fetch-fixtures-f6a8b0.md`.
- Extend the repo-local live e2e skill with authenticated history, watch-later, following feed, and dynamic feed cases.
- Use local credential files and keep these cases outside default CI.
- Classify failures as credential, empty account state, upstream schema/availability, restricted proxy, or server bug.
- Validate through the LAN cache server and generated LAN HLS sources.

### PR 5: Collection/List Fetch Coverage

- Status: implemented by `docs/project_journal/2026/06/2026-06-21-collection-list-fetch-coverage-d3b6e1.md`.
- Add live and deterministic coverage for favorites, space videos, collections, series, and homepage recommendations.
- Stress candidate windows, truncation flags, range/all behavior, and stable selection identity.
- Ensure list changes cannot silently play a different item after resolve/create.
- Avoid client-side direct Bilibili media access.

### PR 6: Bilibili Fetch UX Polish

- Status: implemented by `docs/project_journal/2026/06/2026-06-21-bilibili-fetch-ux-polish-b6f2a9.md`.
- Improve tvOS/macOS presentation for login-required inputs, empty lists, truncated candidate windows, dynamic-feed volatility, and retryable upstream failures.
- Add clear re-resolve, clear selection, and recovery actions where they fit the existing UI.
- Prioritize macOS validation ergonomics while preserving tvOS functional parity.

### PR 7: ABR Metadata Foundation

- Status: implemented by `docs/project_journal/2026/06/2026-06-22-hls-abr-metadata-foundation-a3c9f4.md`.
- Persist BBDown playback ABR group, level, variant, and media cache-key metadata in server-owned manifests.
- Keep current single-variant playback behavior unchanged.
- Add cache-status and recovery tests for variant-aware metadata.

### PR 8: Multi-Variant HLS Master

- Status: implemented by `docs/project_journal/2026/06/2026-06-22-hls-multi-variant-master-e6b4a8.md`.
- Emit compatible multi-variant HLS master playlists from persisted ABR metadata.
- Prefer AVPlayer-safe H.264/AAC variants, adding HEVC/AV1 only when metadata and platform constraints make them safe.
- Make cache-first and upstream fallback logic variant-aware.

### PR 9: Segment-Level Fill And Prefetch

- Status: implemented by `docs/project_journal/2026/06/2026-06-22-hls-first-window-prefetch-b2d9c6.md`.
- Move from whole-resource byte-range fill toward smaller cache units suitable for playback-position-aware prefetch.
- Prioritize init data and the first playable segment window.
- Schedule background fill by current playback position, recent user intent, and cache pressure.

### PR 10: Adaptive Weak-Network Policy

- Status: implemented by `docs/project_journal/2026/06/2026-06-22-hls-adaptive-weak-network-policy-d8e7c1.md`.
- Detect slow or failing upstream paths and downgrade to lower compatible variants when possible.
- Allow later upgrade when network behavior recovers.
- Surface retrying, cache-only, partially cached, quota-blocked, and upstream-failed states in the shared UI.

### PR 11: LAN Transcoding Foundation

- Add the server-side configuration, task-state, and media-pipeline boundary for optional LAN transcoding/transmuxing.
- Start with conservative H.264/AAC HLS output goals for AVPlayer compatibility.
- Keep automatic transcoding policy and expensive quality heuristics out of the first foundation PR.

## Validation Contract

- Each PR starts from updated `master` and lands on a focused `wip/<topic>` branch.
- Each PR must pass the full local gate, including `just ci`, plus any relevant live e2e or macOS validation called out by that PR.
- Each PR must pass GitHub CI and required repository checks.
- Each PR must complete all three review lanes before merge:
  - GitHub `codex/review-gate` when present or required.
  - `independent-codex-pr-review`.
  - `offline-frozen-diff-review`.
- All actionable PR comments and unresolved conversations must be addressed or resolved before merge.
- After each merge, update local `master` from `origin/master` before branching the next PR.

## Deferred

- Physical Apple TV deployment and real-device playback validation.
- Authentication/TLS for the LAN control plane.
- Automatic transcoding quality policy beyond the initial foundation.

## Evidence

- `BBDown-rust` `v0.5.0` release: https://github.com/Joey-Project/BBDown-rust/releases/tag/v0.5.0
- `BBDown-rust` `v0.5.0` peeled commit used by PR 0: `b5dde066561fc39c6387198f6e9a61513ee44eee`
- PR 0 local gate:
  - `cargo test --package tvos-net-player-cache-server --locked`
  - `cargo fmt --all -- --check`
  - `just ci`
  - `just test-bilibili-live` with default non-restricted live cases.
- PR 0 restricted Bangumi live cases remain explicit credential/proxy validation and are covered by the later authenticated fixture PRs, not by the default PR 0 gate.
- PR 1 local gate:
  - `cargo fmt --all`
  - `cargo test --package tvos-net-player-cache-server --locked`
  - `python3 /Users/joey/.codex/skills/project-journal/scripts/project_journal.py validate --repo /Users/joey/Program/Codex-workspace/tvOS-net-player`
  - `cargo fmt --all -- --check`
  - `git diff --check`
  - `just ci`
  - `just test-bilibili-live`
- PR 5 local gate:
  - `cargo fmt --all`
  - `cargo test --package tvos-net-player-cache-server --lib --locked`
  - `cargo test --package tvos-net-player-cache-server --test bilibili_live_e2e --locked`
  - `just test-bilibili-live`
  - `BILIBILI_LIVE_E2E_CASES=space-collection just test-bilibili-live`
  - Live probes showed `space-videos` and `homepage-recommendations` currently need a web cookie, `favorite-list` sample availability can fail upstream selection, and `space-series` sample can stall during playback planning; authenticated list/feed cases therefore need web-cookie opt-in, and favorite/series need URL overrides before joining unfiltered collection/list smoke coverage.
- PR 6 local gate:
  - `swift test --filter BilibiliTaskViewModelTests`
  - `scripts/format.sh`
  - `git diff --check`
  - `scripts/lint.sh`
  - `just ci`
- PR 7 local gate:
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server hls_session_manifest --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server completed_session_manifest_scrubs_upstream_request_data --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server create_bilibili_playback_task_returns_preparing_and_plans_hls_session_in_background --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server --lib`
- PR 8 local gate:
  - `cargo fmt --manifest-path CacheServer/RustCacheServer/Cargo.toml`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server hls::tests:: --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server hls_cache::tests:: --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server --lib`
- PR 9 local gate:
  - `cargo fmt --manifest-path CacheServer/RustCacheServer/Cargo.toml`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server hls_cache::tests:: --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server --lib`
- PR 10 local gate:
  - `cargo fmt --manifest-path CacheServer/RustCacheServer/Cargo.toml`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server hls_network_policy::tests --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server hls::tests::master_playlist_filter --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server get_hls_cache_status_reports --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server hls_master_playlist_demotes_unhealthy_variant_from_network_policy --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server --lib`
  - `swift test --filter CacheLibraryViewModelTests/testHLSCacheSummaryIncludesWeakNetworkPolicyStatus`
  - `swift test`
  - `git diff --check`
  - `just ci`
- Current HLS progressive cache journal: `docs/project_journal/2026/06/2026-06-14-hls-progressive-cache-f3a9d1.md`
- Current discovery/cache/Bilibili roadmap journal: `docs/project_journal/2026/06/2026-06-16-discovery-cache-bilibili-roadmap-a9d4c1.md`
- Current Bilibili task schema roadmap journal: `docs/project_journal/2026/06/2026-06-19-bilibili-task-schema-roadmap-b7e3f1.md`
