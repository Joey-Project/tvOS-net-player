---
id: 20260616-a9d4c1
title: Discovery Cache And Bilibili Roadmap
status: completed
created: 2026-06-16
updated: 2026-06-18
branch: wip/bilibili-resolve-select-schema
pr:
supersedes: []
superseded_by:
---

# Discovery Cache And Bilibili Roadmap

## Summary

- Physical Apple TV validation remains deferred until signing and device pairing are available.
- Delivered the product slice as sequential PRs, each branched from updated `master` after the previous PR was merged.
- Keep gRPC as the control plane and HTTP/HLS/Range URLs as the media plane.
- Prioritize online playback responsiveness while making completed and partially prepared HLS cache more useful on weak networks.

## Decisions

- Bonjour discovery should be automatic, with a picker and manual address fallback.
- The Rust LAN cache server should advertise `_tvos-net-player._tcp` with TXT metadata for `server_id`, `server_name`, and version.
- The Rust server should only advertise Bonjour when the gRPC listener includes a non-loopback address; the default localhost listener stays manual-only.
- Swift clients should use `Network.framework` browsing and persist the last selected server.
- Automatic eviction is HLS-only for this sequence.
- The default HLS cache capacity is 50 GiB; setting it to `0` disables automatic eviction.
- Start proactive cleanup at a 90% high watermark and clean down toward an 80% low watermark.
- Eviction should skip the current playback session, in-progress work, prewarm work, and incomplete cache sessions.
- Progressive playback should become playable as early as possible, then continue filling the cache in the background.
- When the user exits a video, old fill work should demote behind newer user-visible tasks, using FILO order for old video fill work.
- Multi-result Bilibili inputs should use a resolve-then-create flow: resolve candidates first, then create a playback/cache task for the selected item.
- Multi-result prewarm should prepare initial bytes for unplayed items at low priority. Because current HLS output uses large byte-range media resources, the first-frame prewarm target is the MP4 init range plus a bounded head byte window, not true HLS segment prefetch.

## PR Plan

### PR A: Bonjour Discovery

- Status: implemented by this slice.
- Add server-side Bonjour/mDNS publication for the Rust LAN cache server.
- Add shared AppCore discovery state and Swift `Network.framework` browsing.
- Add tvOS and macOS automatic connect, picker, and manual fallback UX.
- Add `NSBonjourServices` entries to app Info.plist files.
- Keep cleartext trusted-LAN h2c behavior unchanged.

### PR B: HLS Cache Quota And Watermark Eviction

- Status: implemented by this slice.
- Add configurable HLS cache quota and watermark settings:
  - `Cache:HlsCacheMaxBytes` default: 50 GiB.
  - `Cache:HlsCacheHighWatermarkPercent` default: 90.
  - `Cache:HlsCacheLowWatermarkPercent` default: 80.
- Run eviction before HLS cache finalization when projected usage exceeds the high watermark.
- Add a periodic background check that cleans completed HLS cache down to the low watermark.
- Surface cache quota, watermark, completed-HLS usage, and last eviction summary to Swift clients.
- Keep eviction scoped to completed HLS sessions. It skips protected/current playback work and incomplete sessions.

### PR C: Progressive Weak-Network Scheduler And Prewarm

- Status: implemented by this slice for selected playable sessions; multi-result candidate prewarm remains tied to PR D's resolve/select schema.
- Preserve the current fast playable-source path for active playback.
- Add server-internal foreground/demoted fill scheduling so new playback can preempt older cache fills and older fills resume in FILO order.
- Split user cancellation from scheduler preemption: cancellation deletes partial sessions, while preemption keeps the playable manifest and committed resources.
- Add first-frame prewarm sidecars for selected HLS sessions using the MP4 init range plus a bounded head byte window.
- Use prewarm metadata and prefix files in the media path before falling back to upstream proxying.
- Add user-visible weak/offline states for pending fill, partially prepared cache, offline-ready cache, and quota-blocked/cache-failed outcomes.

### PR D: Bilibili Resolve/Select Multi-Result Control Plane

- Status: implemented by this slice.
- Add a resolve RPC that maps a Bilibili input into selectable candidates.
- Keep playback/cache task creation single-selection and accept an opaque `selection_id`.
- Return enough candidate metadata for tvOS/macOS selection UI: title, index/subtitle, source kind/content id, and optional duration/cover when the core provides it.
- Use BBDown 0.3.x page/feed/history/watch-later capabilities through the LAN cache server, without letting clients talk to internet media URLs directly.

## Validation Contract

- Each PR must pass the full local gate, including `just ci`.
- Each PR must pass GitHub CI and the required review gate.
- Each PR must complete the three review lanes before merge:
  - GitHub `codex/review-gate` when present or required.
  - `independent-codex-pr-review`.
  - `offline-frozen-diff-review`.
- All actionable PR comments and unresolved conversations must be addressed before merge.
- After each merge, update local `master` from `origin/master` before branching the next PR.

## Deferred

- Physical Apple TV deployment validation.
- LAN-side transcoding policy and UI.
- True segmented HLS output and segment-level prefetch.
- Authentication/TLS for the LAN control plane.

## Evidence

- PR C full local gate:
  - `just ci` passed after implementing weak-network progressive scheduling, prewarm sidecars, Swift cache status mapping, and tvOS/macOS status UX.
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server -- --nocapture`
  - `scripts/test.sh`
  - `scripts/format.sh`
  - `scripts/lint.sh`
  - `python3 /Users/joey/.codex/personal-sync/overlays/private/releases/bb9b591d6375c3c11482cb4fa99394132419c816/personal_codex/skills/project-journal/scripts/project_journal.py validate --repo /Users/joey/Program/Codex-workspace/tvOS-net-player`
  - `git diff --check`
- Internal review:
  - Found and fixed discovery session lifetime retention in `BonjourCacheServerDiscoveryClient.snapshots()`.
  - Found and fixed default localhost-only server advertisement by skipping Bonjour when all gRPC listeners are loopback.
  - Found and fixed the explicit `Combine` import required by `@Published` and `ObservableObject`.
  - Found and fixed discovered-server address persistence so Bonjour selections are only saved after a successful refresh.
  - Found and fixed concrete LAN IP advertisement so Bonjour publishes only the configured listener IP, while wildcard listeners keep automatic address publication.
  - Found and fixed tvOS discovery failure visibility so `Discovery failed` status remains visible when browsing stops with no servers.
  - Final `codex-readonly` isolated review after all fixes: LGTM.
- GitHub `codex/review-gate` fixes:
  - Added retry/backoff after transient Bonjour `NetService` resolve failures while the browser still reports the service.
  - Changed auto-discovery so failed refreshes clear the staged server and try other discovered candidates instead of consuming the first attempt forever.
  - Tightened server-side Bonjour publication so a LAN-reachable gRPC listener is only advertised when playback media is also reachable through a LAN media listener or public media base URI.
- Post-review hardening fixes:
  - Bound gRPC and media listeners and spawned their server tasks before starting Bonjour advertisement, so port/listener failures do not publish an unusable service.
  - Changed auto-discovery failures from permanent service skips to a 30-second retry backoff while still trying other discovered servers immediately.
  - Recreated the `NWBrowser` after transient browser failures with a bounded restart delay.
- Full local gate:
  - `just ci` after local, GitHub Codex, independent, and offline review fixes.
- Formatting and metadata validation:
  - `scripts/format.sh`
  - `plutil -lint TVOSNetPlayer/Info.plist`
  - `plutil -lint MacOSNetPlayer/Info.plist`
  - `python3 /Users/joey/.codex/skills/project-journal/scripts/project_journal.py validate --repo /Users/joey/Program/Codex-workspace/tvOS-net-player`
- Targeted Rust validation:
  - `cargo test --package tvos-net-player-cache-server bonjour` after the concrete listener-address fix, media-plane advertisement gate, and listener-before-advertise hardening.
- Targeted Swift validation:
  - `swift test --filter CacheServerDiscoveryViewModelTests` after the discovered-server persistence, discovery error-visibility, failed-auto-connect recovery, browser restart, and retry-backoff fixes.
- PR D targeted local validation:
  - `just ci` after the PR D implementation and local review fixes.
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server create_bilibili_playback_task_returns_preparing_and_plans_hls_session_in_background --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server resolve_bilibili_input_returns_selectable_candidates --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server create_bilibili_playback_task_passes_selection_id_to_planner --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server get_server_info_advertises_bilibili_resolve_capability --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server resolve_selection_limits_candidates_to_first_page_window --lib`
  - `scripts/test.sh`
  - `scripts/test-cache-server.sh`
  - `scripts/format.sh`
  - `cargo fmt --all --manifest-path CacheServer/RustCacheServer/Cargo.toml`
  - `python3 /Users/joey/.codex/skills/project-journal/scripts/project_journal.py validate --repo /Users/joey/Program/Codex-workspace/tvOS-net-player`
  - `git diff --check`
- PR D final local validation:
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server resolve_selection_preserves_current_episode_inputs --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server resolve_selection_uses_bounded_indices_for_broad_inputs --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server resolve_bilibili_input_returns_selectable_candidates --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server parses_collection_item_selection_id_as_single_index --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server collection_resolution_candidates_round_trip_as_item_selections --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server bounded_resolve_fallback_only_retries_short_selection_errors --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server bbdown_adapter::tests --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server create_bilibili_playback_task_passes_selection_id_to_planner --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server bbdown_adapter::tests --lib` after preserving collection-item source context.
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server create_bilibili_playback_task_passes_selection_id_to_planner --lib` after preserving collection-item source context.
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server mux_download_report_cancels_running_ffmpeg --lib` after moving the fake ffmpeg start marker before partial-output creation.
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server --lib` after moving the fake ffmpeg start marker before partial-output creation.
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server bbdown_adapter::tests --lib` after removing unbounded `Selection::All` resolve fallback.
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server bbdown_adapter::tests --lib` after replacing fixed retry windows with largest bounded-prefix probing.
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server create_bilibili_playback_task_passes_selection_id_to_planner --lib` after replacing fixed retry windows with largest bounded-prefix probing.
  - `just ci` after the concrete-episode selection fix.
  - `just ci` after the collection item selection fix.
  - `just ci` after the bounded short-result resolve fallback fix.
  - `just ci` after the collection-item source-context fix.
  - `just ci` after moving the fake ffmpeg start marker before partial-output creation.
  - `just ci` after removing unbounded `Selection::All` resolve fallback and replacing it with smaller bounded retry windows.
  - `just ci` after replacing fixed retry windows with largest bounded-prefix probing.
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server --lib` after adding stale collection selection identity validation and sharing the playback-planning semaphore with resolve RPCs.
  - `just ci` after adding stale collection selection identity validation and sharing the playback-planning semaphore with resolve RPCs.
- PR D pre-commit internal review:
  - Found and fixed old-server compatibility by advertising a `bilibiliResolve` capability and falling back to direct task creation when the resolve RPC returns `UNIMPLEMENTED`.
  - Initially separated resolve from playback task creation; final review later re-shared the playback-planning semaphore for BBDown resolve work that can fetch broad collections.
  - Found and fixed unbounded resolve fan-out by asking BBDown core for a bounded first candidate window and capping returned candidates to 100.
  - Found and fixed concrete episode, cheese episode, and international episode inputs so resolve planning preserves `Selection::Current` instead of forcing a first-page candidate window.
  - Found and fixed collection, favorite, history, and watch-later candidates so their opaque selection IDs round-trip to single-item `Selection::Indices` instead of video-page `Selection::Page`.
  - Found and fixed bounded resolve for short videos, seasons, and lists by retrying with smaller bounded index windows when BBDown core reports a missing selected page, episode, or collection item; returned candidates remain capped to 100.
  - Found and fixed collection/feed selection IDs so playback planning keeps the original collection/feed source and applies the selected list index instead of rewriting the input to a single BVID or aid.
  - Found and fixed unbounded short-result fallback by replacing `Selection::All` retry with smaller bounded index windows.
  - Found and fixed fixed-window bounded retries truncating common 2-4 candidate inputs by probing for the largest valid bounded `1..N` prefix instead of accepting the first smaller successful window.
  - Earlier `codex-readonly` isolated review after largest bounded-prefix probing: LGTM.
- PR D triple-review fixes:
  - `independent-codex-pr-review` found dynamic collection/feed candidate selection could silently play a different item when the list changed between resolve and create.
  - `offline-frozen-diff-review` found the same index-only collection/feed selection risk and found resolve RPCs bypassed the playback-planning concurrency limiter.
  - Fixed collection/feed selection IDs by preserving the original source context, adding expected BVID/aid identity to opaque `item` IDs, and failing stale selections when the planned entry no longer matches the resolved candidate.
  - Fixed resolve RPC resource protection by acquiring `playback_planning_permits` while the resolver runs.
- PR D final review follow-up:
  - `independent-codex-pr-review` found selected candidate submissions were only bound to the source text, so changing the cache server endpoint or playback options after resolve could send a server-specific selection ID to the wrong create request.
  - `independent-codex-pr-review` also found the new selected-create method replaced the old public `CacheControlClient.createBilibiliPlaybackTask(urlOrID:options:)` requirement, breaking existing external conformers.
  - `offline-frozen-diff-review` found common Bilibili resolve inputs could require multiple upstream resolve calls before returning candidates, risking the UI timeout budget.
  - Fixed the Swift view model by binding resolved candidates to normalized source text, cache server endpoint, and playback options, and forcing a fresh resolve when any of those change before selected submission.
  - Restored the legacy `CacheControlClient.createBilibiliPlaybackTask(urlOrID:options:)` requirement and added a default selected-create overload that forwards nil selections while rejecting non-empty selections for conformers that do not support Bilibili resolve.
  - Fixed the Rust adapter to use single overview resolve requests for common inputs and extract full episode candidates from returned metadata, keeping the old bounded-prefix fallback only for short-result recovery paths.
  - The next `independent-codex-pr-review` and GitHub `codex/review-gate` pass found that ordinary BV/av resolve used `Selection::Current`, hiding multi-page candidates, collection/list resolve exposed full fetched lists before local truncation, arbitrary `selection_id` strings could request unbounded playback planning, stable collection identities only failed stale plans instead of planning by BVID/aid, and the picker state could not be cleared from the platform UIs.
  - Fixed BV/av resolve by using `Selection::All` and adding an adapter test that multi-page videos produce page candidates.
  - Fixed collection/list resolve to return only BBDown's selected item until `bbdown-core` exposes a bounded candidate-page API, avoiding adapter-level full-list candidate exposure.
  - Restricted playback `selection_id` parsing to resolver-generated single-item `page:`, `episode:`, and `item:` forms; `item:` IDs carrying BVID/aid now override planning to the stable video input before falling back to original index selection.
  - Added shared Bilibili `canClear` state and wired tvOS/macOS controls to clear resolved candidate pickers.
  - The next `independent-codex-pr-review` found `page:` and bare `item:` IDs could still be paired with collection/feed sources and trigger high-index BBDown fetch windows.
  - The next `offline-frozen-diff-review` found direct selected-create calls against old gRPC servers could silently ignore the new `selection_id` field and play the default candidate.
  - Fixed playback `selection_id` parsing to validate IDs against the source input kind, reject unstable bare item IDs, and cap resolver-index IDs to the advertised candidate window.
  - Fixed `GRPCCacheControlClient` selected-create calls to require the `bilibiliResolve` capability before sending non-empty `selectionID` values.
- PR D final review follow-up validation:
  - `scripts/format.sh`
  - `cargo fmt --all --manifest-path CacheServer/RustCacheServer/Cargo.toml`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server bbdown_adapter::tests --lib`
  - `swift test --filter TVOSNetPlayerCacheClientTests`
  - `swift test --filter BilibiliTaskViewModelTests`
  - `swift test --filter CacheLibraryPaginationTests`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server resolve_bilibili_input_returns_selectable_candidates --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server resolve_bilibili_input_waits_for_playback_planning_permit --lib`
  - `cargo test --manifest-path CacheServer/RustCacheServer/Cargo.toml --package tvos-net-player-cache-server playback_plan_rejects_stale_collection_selection_identity --lib`
  - `just ci`

## Next Steps

- Finish the PR D triple review, GitHub CI, resolved-conversation check, merge, and `master` sync.
- Defer candidate prewarm beyond selected-item first-frame prewarm until we have real usage data for multi-result browsing.
