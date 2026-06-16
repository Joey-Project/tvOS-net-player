---
id: 20260616-a9d4c1
title: Discovery Cache And Bilibili Roadmap
status: active
created: 2026-06-16
updated: 2026-06-16
branch: wip/bonjour-discovery
pr:
supersedes: []
superseded_by:
---

# Discovery Cache And Bilibili Roadmap

## Summary

- Physical Apple TV validation remains deferred until signing and device pairing are available.
- Deliver the next product slice as sequential PRs, each branched from updated `master` after the previous PR is merged.
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

- Add configurable HLS cache quota and watermark settings:
  - `Cache:HlsCacheMaxBytes` default: 50 GiB.
  - `Cache:HlsCacheHighWatermarkPercent` default: 90.
  - `Cache:HlsCacheLowWatermarkPercent` default: 80.
- Run eviction before new cache/full-fill/prewarm work when projected usage exceeds the high watermark.
- Add a periodic background check that cleans completed HLS cache down to the low watermark.
- Surface cache quota, watermark, usage, and last eviction summary to Swift clients.

### PR C: Progressive Weak-Network Scheduler And Prewarm

- Preserve the current fast playable-source path for active playback.
- Add cancellable background fill work that demotes old playback sessions when a new user-visible action arrives.
- Add low-priority first-frame prewarm for multi-result candidates using init plus bounded head bytes.
- Add user-visible weak/offline states for pending fill, partially prepared cache, offline-ready cache, and quota-blocked/cache-failed outcomes.

### PR D: Bilibili Resolve/Select Multi-Result Control Plane

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

## Next Steps

- After PR A merges, update `master`, branch PR B, and implement HLS cache quota plus watermark eviction.
