---
id: 20261110-b46401
title: Shared HLS Upshift Assessment
status: completed
created: 2026-11-10
updated: 2026-11-11
branch: codex/hls-upshift-policy
pr:
supersedes: []
superseded_by:
---

# Shared HLS Upshift Assessment

## Scope
- Bridge the [server readiness evidence](2026-11-10-hls-readiness-b46400.md) to a typed Swift client and deterministic shared AppCore assessment. Both macOS and tvOS use the same library.
- This slice returns a bounded opportunity or a typed reason to hold. It does not change AVPlayer preferences, replace player items, seek, prefetch, promote fill jobs or claim lower recovery latency.
- Preserve automatic client buffering, server whole-asset fill, safe existing low-quality buffer reuse and the pure AVPlayer-managed long acceptance case.

## Decision Boundaries
- Separate a readable cached target window from an uncached target that would depend on the origin. A fast LAN is not evidence that missing media can be delivered from the origin fast enough.
- A readable exact video/audio window can yield a cache-backed opportunity without fresh origin measurements or a bitrate estimate. It is still a snapshot, not a lease or a continuity guarantee.
- Cache evidence does not establish client-to-server delivery capacity, especially through a remote tunnel. A later handover must still validate the normal serving path, advertised rendition, decoder and client delivery; do not treat a fast origin or cached bytes as proof of a fast client link.
- A missing window can yield only an origin-prewarm opportunity, never permission to switch. Require explicit target video and required-audio rates, fresh measured goodput and first-byte latency for every required representation, and a future window bridged by the current contiguous low-quality buffer.
- Protect at least 20 media seconds by default. Convert future-media distance to wall time using actual playback speed; conservatively include observation age, RPC time, latency and a 1.5-times supply margin in estimates. Neither nominal bitrate nor recent goodput guarantees future delivery.
- Unknown cache state, unfamiliar wire values, inconsistent state/reason, mismatched identity/window/generation, stale observations, missing audio evidence or invalid numeric context produces a hold. Optional origin evidence is not synthesized as zero.
- A cached window must still contain upcoming media after conservatively advancing the observed playhead by context and RPC age. Entirely passed windows hold, even inside the snapshot freshness limit. A future target must be bridged by the known contiguous current buffer; cached future bytes alone do not bridge an unobserved gap.
- Server generation identifies a process-local session only. Do not use it as a server-restart epoch or export URLs, credentials or absolute monotonic clocks.
- Mirror server identifier validation: preserve the exact bounded UTF-8 value, reject surrounding whitespace and Unicode control characters, and compare identifier bytes in RPC identity checks rather than Swift's canonically equivalent string equality. Do not silently trim, normalize or impose an extra ASCII-only restriction.

## Integration
- Use the existing read-only RPC and capability 19. Preserve unsupported-server fallback and task cancellation without publishing raw transport errors.
- The assessment service is explicitly invoked with immutable, aged playback context. It has no polling loop and no playback side effects. Later playback integration must reject obsolete session results and revalidate before any handover.
- The production handover mechanism and rendition context acquisition remain separate work, not hidden inside this evidence-to-policy bridge.

## Validation
- Development validation passes 46 focused Swift tests with no failures or skips, plus existing `just lint`. Coverage includes model/wire presence, UTF-8 identity, unknown enums, capability fallback, stale/passed/unbridged windows, native-rate bounds, arithmetic, required audio and cancellation.
- Nine loopback HTTP/2 tests exercise the production gRPC client, including shared assessment composition. This is protocol integration, not native playback acceptance.
- Require complete local lint/CI, the existing fixed-signed three-case native suite with system mute, current-head GitHub CI/UI/native artifacts, GitHub Codex review and every conversation resolved before normal protected squash merge.

## Next Steps
1. Use the [owned assessment](2026-11-11-hls-owned-assessment-b46404.md) to join actual native timing, exact advertised rendition metadata and current item/session ownership; agree on production handover before active quality changes.
2. Measure the separately implemented assisted path against the retained automatic baseline; do not relabel a policy unit test or eventual long recovery as optimized latency.

## Evidence
- [Server readiness PR #93](https://github.com/Joey-Project/tvOS-net-player/pull/93).
- [Recovery diagnostics](2026-11-10-native-abr-latency-b46399.md).
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
