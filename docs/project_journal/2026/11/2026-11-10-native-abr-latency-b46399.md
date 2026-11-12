---
id: 20261110-b46399
title: Native ABR Recovery Latency
status: active
created: 2026-11-10
updated: 2026-11-12
branch: codex/native-abr-recovery-latency
pr: https://github.com/Joey-Project/tvOS-net-player/pull/92
supersedes: []
superseded_by:
---

# Native ABR Recovery Latency

## Scope
- Continue after completed [native acceptance](2026-11-09-native-abr-recovery-b46397.md) and [CI regression gating](2026-11-10-native-media-ci-b46398.md). Optimize recovery responsiveness separately from eventual automatic recovery acceptance.
- First add passive, bounded diagnostics that distinguish a delayed high-rendition origin request from a request for media far ahead of the playhead. Instrumentation alone is not a latency improvement.
- Preserve automatic client buffering, server whole-asset fill, representation identity, all native frame/control/fault assertions, the 600-second recovery case and the historical failed 130-second recovery evidence.

## Network And Buffer Boundaries
- AVPlayer selects a rendition from the LAN HLS source. A fast LAN does not guarantee fast delivery of an uncached fragment: the server may still be waiting for its origin. Distinguish LAN delivery, origin availability/throughput, server cache coverage and the client's loaded media window.
- Already cached higher-quality media can support a more aggressive recovery policy. Uncached higher-quality media needs an origin-supply estimate and a playback safety margin before any production assisted switch is considered. Global server health is not a substitute for per-session/rendition evidence.
- Reuse safe buffered low-quality content. Opportunistic replacement after the current fragment or an approximately 20-second safety window remains a separate playback-scheduling option, not a hard forward-buffer cap or an in-place rewrite of cached bytes.
- Do not infer that changing a bitrate preference forces an immediate switch. Do not use a test-only seek, player-item replacement or forced rendition to manufacture automatic recovery evidence.

## Diagnostic Slice
- Record compact player samples approximately every ten seconds, plus a recovery observation, with explicit storage bounds and truncation. Correlate common monotonic time, playhead, the contiguous loaded-media frontier, decoded dimensions and the latest indicated bitrate. Do not serialize URLs or credentials.
- Map recovery-phase high-origin byte requests to actual inspected MP4 fragment media windows, not a file-offset proportion. Preserve the existing first-range record and separately retain the first successful media request; initialization-only requests are not media.
- Derive bounded recovery-to-request timing and request-to-decoded timing without asserting that an individual origin request caused the first high frame. A nearby preceding player sample is useful only within an explicit freshness bound; missing or stale context stays unknown.
- Validate finite ordered clocks, byte/media bounds and report size. Keep a bounded raw report and a small derived diagnostic artifact inside the existing aggregate artifact budget. No gRPC or persistent schema change is needed.

## Validation
- Run focused Swift probe self-tests, Rust fragment/request diagnostics and Python report/summary admission regressions before the complete repository checks.
- Run the existing three-case native suite through fixed signed local products with system mute verified before and after playback. Collect a fresh long-case timeline; do not relabel it as an optimized recovery result.
- Require current-head GitHub CI/UI/native-media checks, GitHub Codex review and every conversation resolved before normal protected merge. Keep transient delivery receipts outside this tracked journal.

## Current State
- The probe adds passive interval/recovery sampling without changing its playback controls. Strict Swift 6 compilation, pinned fixed-path signing, 67 self-tests and strict Swift formatting pass during development; these dirty-candidate checks do not replace final-head validation.
- Actual fragment-window mapping and separate first-range/first-206-media recording are implemented, with eight focused non-audio Rust tests passing during development. A 206 request is not proof of a completely delivered body or a causal explanation for a decoded frame.
- Ranged `HEAD` probes remain request/metadata observations, not media-body requests. Exclude them from first-media timing and media-payload counters; an actual ranged `GET` is required for those observations.
- Both nested probe capture and outer Rust reports use coherent 64 KiB limits; retained logs/reports together remain capped at 32 MiB. Python admission and the derived summary retain their own validation and size bounds.
- Python helper regressions cover malformed/missing timelines, clocks, stale context, media windows and the exact unsigned-64-bit range boundary. The runner includes derived diagnostics in its existing bounded summary for both fixed-signed local and isolated CI modes.
- The diagnostic slice is delivered through PR #92, including complete local CI, fixed-signed native acceptance, GHA native/UI evidence and current-head GitHub Codex review. No production upshift controller or demonstrated recovery-latency improvement is included.
- The local baseline's first accepted high-media request was about 76.123 seconds after recovery and overlapped indexed fragments at 528-532 seconds. A preceding sample had playhead 111.351 seconds and loaded end 529.364 seconds; the first high frame followed that request by about 412.695 seconds. GHA showed comparable future-media context. These correlations do not prove a unique cause, fixed retry interval or complete cache coverage.

## Next Steps
1. Keep the bounded diagnostic slice in the native gate and retain exact-run raw evidence for baseline comparison.
2. Use the [read-only server readiness evidence](2026-11-10-hls-readiness-b46400.md) and [shared upshift assessment](2026-11-10-hls-upshift-policy-b46401.md) to distinguish a cached-window opportunity from an origin-dependent prewarm estimate. Neither is a playback safety guarantee.
3. Use the [owned assessment](2026-11-11-hls-owned-assessment-b46404.md) for bound observations and the agreed [single-player prewarm comparison](2026-11-11-single-player-prewarm-b46405.md) for the next validation slice. Preserve one player/item, default buffering and the pure AVPlayer-managed long gate. Cache readiness alone is not decoded recovery or a serving/continuity guarantee; do not introduce dual players or forced replacement without a new discussion.
4. Integrate the delivered [bounded playback-preparation scheduling prerequisite](2026-11-12-playback-preparation-scheduling-b46406.md) into the separate OFF/ON comparison in PR #98. Retain the recovery-owned deadline and real attempt/coverage evidence without requiring global foreground quiescence; a failed preparation is not evidence that preparation does or does not improve decoded recovery latency.
5. Continue the remaining native cache lifecycle/offline and home LAN roadmap work after recovery optimization.

## Evidence
- [Passive diagnostics PR #92](https://github.com/Joey-Project/tvOS-net-player/pull/92).
- [Delivery PR #91](https://github.com/Joey-Project/tvOS-net-player/pull/91), merged foundation `d974dbbb8187b1e035dc2af7d072bae511f29f59`.
- [Home LAN roadmap](../10/2026-10-23-home-lan-readiness-roadmap-b46388.md).
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
