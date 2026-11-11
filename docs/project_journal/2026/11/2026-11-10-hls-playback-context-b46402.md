---
id: 20261110-b46402
title: Native HLS Playback Snapshot
status: completed
created: 2026-11-10
updated: 2026-11-11
branch: codex/hls-playback-context
pr:
supersedes: []
superseded_by:
---

# Native HLS Playback Snapshot

## Scope
- Continue the [shared upshift assessment](2026-11-10-hls-upshift-policy-b46401.md) with actual native playback inputs in shared AppCore, used by both macOS and tvOS.
- Capture an immutable playhead, actual player rate, playing state, contiguous loaded-media frontier and local monotonic observation time. Build an explicitly aged assessment context only when its required inputs are known.
- Keep this slice read-only: no timer, automatic RPC, download, buffering preference, seek, item replacement, fill promotion or active quality selection.

## Evidence Boundaries
- Read the actual player rate, not its desired default rate or UI speed. Waiting, paused and finished playback must not become a playing opportunity. Do not capture an unresolved resumption or pending replay as ready playback.
- Contiguous loaded media starts with a valid interval covering the current playhead and extends only through overlapping or touching intervals. A later disjoint range cannot bridge an unknown gap. Invalid times and oversized range collections cannot become positive buffer evidence.
- Reject invalid, indefinite, nonfinite, negative, nonzero-epoch or overflowed native timing rather than converting unknowns to zero. Keep storage and range processing bounded.
- Compare intervals as native rational `CMTime` values before exporting seconds. Historical rounding flags do not change their stored rational values; reject newly rounded or overflowing addition. Export exact representable whole seconds or a conservative downward-rounded fractional endpoint, and retain unknown when numeric precision cannot be established.
- Observation age is local monotonic elapsed time, rounded conservatively to milliseconds. A regressed clock cannot yield fresh evidence. Do not compare device uptimes or expose the clock through the wire protocol.
- A snapshot does not prove HLS identity, the currently decoded rendition, decoder support, target bandwidth, source availability or client-to-server delivery capacity. Do not infer those from URLs, aggregate bitrate, decoded dimensions or progress reports.
- The context bridge accepts explicitly supplied exact target identity/window and optional target evidence. Unknown target fields remain unknown and the existing policy remains the assessment authority. The snapshot is not a handover authorization or lease; later async integration must revalidate player/item/session ownership and current state.

## Remaining Metadata Contract
- Existing `HlsCacheStatus` playback information is submitted progress, not an authoritative currently served rendition. Existing task metadata has selected and candidate variants but does not expose all independent video/audio rates required for origin preparation.
- Add a separately scoped, backward-compatible server-owned rendition metadata contract before production assisted switching. It must match the normal serving path and advertised variants, preserve missing measurements, and never expose upstream URLs or credentials.
- Agree on the production handover mechanism separately. Keep pure AVPlayer-managed recovery evidence distinct from any future assisted path, and preserve automatic forward buffering and whole-asset fill.

## Validation
- Development validation passes 20 snapshot tests and six shared-player integration tests, strict Swift formatting and existing `just lint`. Earlier compile, fixture and environment failures remain separate development evidence; these focused checks do not replace frozen-head full validation.
- Cover native range arithmetic, gaps, ordering, bounds, actual rates, paused/waiting states and monotonic aging with focused Swift tests.
- Exercise the real shared PlayerViewModel reader through an injected controller; assert it does not invoke transport controls or alter resumption behavior.
- Require full local lint/format/CI, fixed-signed three-case native acceptance with system mute, exact-head GitHub CI/UI/native evidence, current-head GitHub Codex review and zero unresolved conversations before normal protected squash merge.
- Muted media tests do not establish audible audio acceptance. No authenticated live Bilibili or physical Apple TV validation belongs to this slice.

## Next Steps
1. Use the separately scoped [read-only rendition catalog](2026-11-11-hls-rendition-catalog-b46403.md) and bind target context to the current native item/session before any asynchronous production action. A catalog is not the currently decoded rendition or a handover lease.
2. Implement the agreed prewarm/handover path separately, with real decoded-frame, continuity and latency acceptance; do not label this snapshot work as a recovery-latency improvement.

## Evidence
- [Shared upshift assessment PR #94](https://github.com/Joey-Project/tvOS-net-player/pull/94).
- [Recovery latency workstream](2026-11-10-native-abr-latency-b46399.md).
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
