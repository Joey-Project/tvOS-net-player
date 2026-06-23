---
id: 20260622-f4a7b2
title: Playback Controls Remote Endpoint And HLS Execution Roadmap
status: active
created: 2026-06-22
updated: 2026-06-23
branch:
pr:
supersedes:
  - 20260621-c9f0a2
superseded_by:
---

# Playback Controls Remote Endpoint And HLS Execution Roadmap

## Summary

- Carry the next product phase as sequential PRs after the BBDown 0.5/HLS foundation work.
- Keep tvOS as the primary target and macOS as the fast validation client.
- Keep the LAN cache server as the only Bilibili/media integration point for clients. Clients receive server-owned playback URLs and do not fetch Bilibili media URLs directly.
- Defer physical Apple TV validation and full auth/TLS hardening. Remote endpoint support may still add HTTPS/TLS transport because Cloudflare Tunnel style endpoints require it.

## Current State

- The app can discover LAN cache servers through Bonjour and can manually connect to plaintext `host[:port]`/`http://host:port` gRPC endpoints.
- Remote endpoint support now accepts `https://` cache server origin URLs, applies scheme-aware defaults, and uses HTTP/2 TLS for the gRPC control plane.
- Path-scoped gRPC URLs remain intentionally unsupported; Cloudflare Tunnel or reverse proxy deployments should route the cache control plane at the host root.
- PR 1 added shared AppCore seek/skip/playback-speed controls and exposed them in both tvOS and macOS while preserving the SwiftUI `VideoPlayer` surface.
- HLS playback has ABR metadata, multi-variant master output, first-window prefetch, adaptive weak-network policy, LAN transcoding foundation, and a conservative ffmpeg execution MVP. True fMP4 segment-index playlist splitting remains future work.

## PR Plan

### PR 1: Player Seek Skip And Speed Controls

- Status: implemented by this slice.
- Add shared AppCore playback controls for seek/skip and playback speed.
- Surface tvOS/macOS controls without regressing the existing `VideoPlayer` path.
- Add unit coverage for control state and edge cases where no player is loaded.
- Validate with `just ci` and macOS app build/test gates before merge.

### PR 2: Remote Endpoint And CF Tunnel Support

- Status: implemented by this slice.
- Extend `CacheServerEndpoint` from plaintext host/port parsing to a typed endpoint model with scheme-aware defaults.
- Support `https://` gRPC endpoints and TLS transport for remote/Cloudflare Tunnel style control-plane access.
- Preserve Bonjour/manual LAN behavior as `host:port` plaintext h2c and keep UI placeholders/error copy explicit about both local addresses and remote URLs.
- Keep server-returned media URLs as the only media source authority; clients still do not derive Bilibili/media URLs from the control-plane endpoint.
- Keep auth policy explicit and defer full credential/auth hardening.

### PR 3: LAN Transcoding Execution MVP

- Status: implemented by this slice.
- Add the ffmpeg/job-runner execution path behind the existing LAN transcoding foundation.
- Rewrite completed HLS cache sessions that require LAN transcoding to a generated AVPlayer-compatible fMP4 resource served through the existing server-owned HLS URL path.
- Pin generated output to H.264 High@4.2/AAC, cap video at 1080p60 with a 10 Mbps video VBV envelope plus 128 kbps audio, and persist matching completed-session metadata.
- Persist the generated completed session manifest so restart recovery restores the generated playback resource rather than the original incompatible source resources.
- Keep cancellation, preemption, and ffmpeg failure paths from exposing the original `ready` session as a completed library item.
- Keep original source resources lookup-only after completed-session transcoding so persisted manifests can be restored and already-fetched AVPlayer playlists can finish from local cache, while the completed master playlist does not advertise old source variants and hidden resources never fall back to upstream proxying.
- Include projected generated transcode output in pre-finalization quota eviction so fully cached sources do not bypass high-watermark protection before ffmpeg writes the temporary output.
- Keep the conservative `avplayer-h264-aac-hls-v1` target profile.
- Validate with cache-server unit/finalizer tests and the full repository gate before merge.

### PR 4: fMP4 Segment-Index HLS Splitting

- Status: pending.
- Move beyond whole-resource byte-range playlist output by deriving safe segment boundaries from fMP4 metadata such as `sidx`/`moof`.
- Keep playlist splitting conservative and avoid fabricated segment boundaries.
- Feed smaller cache units into prefetch, fill, and weak-network policy.

### PR 5: Playback-Position-Aware Weak Offline UX

- Status: pending.
- Add a lightweight app-to-server playback-position signal or equivalent control-plane hook.
- Use playback position, recent user intent, and cache state to prioritize fill/prefetch.
- Improve weak/offline UI around retrying, cache-only, partially cached, quota-blocked, and upstream-failed states.

### PR 6: Batch Cache Finalization And Sidecar Options UX

- Status: pending.
- Define UX and server/client behavior for multi-result batch cache finalization beyond the primary selected resource.
- Complete client-facing controls for sidecar artifacts and download options such as subtitle, danmaku, cover, and audio-language preferences.
- Ensure the complete-download workflow and progressive-playback workflow stay distinct where their cache semantics differ.

## Validation Contract

- Each PR starts from updated `master` and lands on a focused `wip/<topic>` branch.
- Each PR must pass the full local gate, including `just ci`, plus any relevant focused tests or live/macOS validation called out by that PR.
- Each PR must pass GitHub CI and required repository checks.
- Each PR must complete the requested review gates before merge:
  - GitHub `codex/review-gate` when present, required, or explicitly triggered for the PR.
  - `independent-codex-pr-review`.
  - `offline-frozen-diff-review`.
- All actionable PR comments and unresolved conversations must be addressed or resolved before merge.
- After each merge, update local `master` from `origin/master` before branching the next PR.

## Deferred

- Physical Apple TV deployment and real-device playback validation.
- Full LAN control-plane auth/TLS hardening beyond what is required for remote endpoint transport.
- Automatic quality policy and expensive transcoding quality heuristics beyond the execution MVP.

## Evidence

- Previous BBDown 0.5/HLS roadmap: `docs/project_journal/2026/06/2026-06-21-bbdown-050-hls-roadmap-c9f0a2.md`
- Current architecture note: `docs/architecture/cache-server.md`
- PR 1 local gate:
  - `scripts/format.sh`
  - `swift test --filter PlayerViewModelTests`
  - `python3 /Users/joey/.codex/personal-sync/overlays/private/releases/5f1ab3fa5d9f7d534507216a2d6f765694f9b710/personal_codex/skills/project-journal/scripts/project_journal.py validate --repo /Users/joey/Program/Codex-workspace/tvOS-net-player`
  - `git diff --check`
  - `just ci`
- PR 2 focused validation:
  - `swift test --filter CacheServerEndpointTests`
  - `swift test --filter CacheLibraryViewModelTests/testRefreshAcceptsHTTPSCacheServerURL`
  - `swift test --filter CacheLibraryViewModelTests`
  - Full `just ci` passed with tvOS simulator, macOS app, Swift package, and Rust cache server tests.
- PR 3 focused validation:
  - `cargo test transcoding_ -- --nocapture`
  - `cargo test hls_ffmpeg_args_pin_declared_h264_level -- --nocapture`
  - `cargo test transcoded_completed_session_advertises_capped_output_profile -- --nocapture`
  - `cargo test finalization_projection_includes_generated_transcode_output -- --nocapture`
  - `cargo test caches_transcoded_session_and_restores_generated_manifest -- --nocapture`
  - `cargo test hidden_alternate_resources_are_lookup_only -- --nocapture`
  - `cargo test hls_segment_serves_hidden_completed_source_from_cache_only -- --nocapture`
  - `cargo test hls_segment_rejects_hidden_completed_source_without_cache -- --nocapture`
  - `cargo test completed_runtime_session_scrubs_hidden_lookup_resources -- --nocapture`
  - `cargo test completed_playback_task_scrubs_runtime_hls_alternates_after_finalization -- --nocapture`
  - `cargo test hls_cache_finalizer_transcodes_ready_session_to_generated_runtime -- --nocapture`
  - `just ci`
