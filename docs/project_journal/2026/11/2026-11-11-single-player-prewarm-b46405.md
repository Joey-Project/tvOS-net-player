---
id: 20261111-b46405
title: Single-Player HLS Prewarm Comparison
status: completed
created: 2026-11-11
updated: 2026-11-12
branch: codex/single-player-prewarm-diagnostic
pr: https://github.com/Joey-Project/tvOS-net-player/pull/98
supersedes: []
superseded_by:
---

# Single-Player HLS Prewarm Comparison

## Decision
- Keep one application-level AVPlayer and AVPlayerItem during upshift through a multivariant HLS master. Do not introduce a standby player, cold item replacement, forced seek or a new default buffer cap.
- Prioritize playback continuity, then recovery responsiveness, then server preparation cost. Extra server downloads are acceptable, but preparation must not take priority over the current playback supply.
- Keep compatible encoding, frame rate, dynamic range and audio/timeline association in the first slice. All media remains server-owned HTTP HLS; clients do not fetch Bilibili origins or receive credentials.
- Preserve default client buffering and server whole-asset fill. A roughly twenty-second protected media window is not a forward-buffer limit, a fixed waiting period or an upshift deadline.

## Validation Slice
- Add an explicitly selected, credential-free comparison of prewarm disabled/enabled with matched synthetic media, origin impairment/recovery and fresh per-run player/cache state. Keep the existing strict three-case native gate independent and unchanged.
- Prepare only the target higher rendition, including initialization/video/separate-audio data. Record actual preparation coverage and completion; a request, response header or throughput estimate alone is not cache readiness.
- Publish a bounded passive position/buffer snapshot at the actual recovery trigger in both modes. Select full fragments beyond the greater of the continuous loaded boundary and playhead plus twenty seconds. Keep the logical selection to twenty seconds of media and eight MiB. Before execution, derive the real bounded-preparation fine-range/startup plan from the cache planner, deduplicate it and enforce a separate sixteen-MiB transfer-plan bound and fifteen-second operation deadline. Do not delay the recovery clock until preparation completes.
- Preparation alignment may fetch bytes adjacent to the logical window or already buffered media. Report those expanded spans separately before executing them; they only populate the cache and do not replace client buffers. Selected readable-range coverage, attempted expanded spans, committed cache data and actual wire traffic are different quantities. A test-only observation must record execution, not infer successful work from the logical input or returned range-vector length.
- Cooperate with foreground playback using the delivered bounded-preparation API, below current playback and above whole-asset fills. Global foreground quiescence is not a prerequisite. Only a typed priority preemption may defer and retry; cancellation, identity, removal, quota and transport errors remain failures. Keep one recovery-owned absolute fifteen-second preparation deadline, at most 128 actual attempts, and a separate sixteen-MiB cumulative attempted-span budget that charges every retry. Invoking an inner batch, retrying or waiting must not reset these bounds.
- Diagnostic report schema 4 distinguishes the empty-cache transfer-plan upper bound, actual range-send invocations, validated bodies, confirmed cache-publication union and selected-range readability. Request records identify cache fill, size discovery or identity probes without URLs, headers or credentials; requested spans are not wire-byte measurements. Resource IDs and total lengths bind every range. Publication bytes are a non-overlapping union, not a sum of overlapping attempts.
- Optional request-size metadata stays unknown when absent, never zero. The measured media-index length still binds the plan to validated cache/range-response totals; contradictory declared or observed lengths must not produce completed preparation. Pre-send and operation-level typed failures do not invent HTTP attempts, and their root-cause categories are separate from per-attempt lifecycle events.
- Publish partial observations before each await and retain typed failure context and recovery-owned elapsed time. Before/after readability lists contain only exact selected ranges whose bytes were actually read and validated; unavailable observations remain null, never zero. Success requires all selected logical ranges, not every optional startup/alignment plan span. Existing or coalesced cache readiness may complete with zero preparation-origin attempts/publications. Admission deferrals can occur before any HTTP attempt and are counted separately.
- Keep at most 128 selected/planned/attempt/publication entries and 128-byte safe ASCII resource IDs. Only this opt-in v4 diagnostic allows a 192-KiB prewarm object inside a 256-KiB outer report; its scalar summary remains 64 KiB, normal native/probe reports remain 64 KiB and the aggregate artifact budget remains 32 MiB. Schema 3 evidence is retained as historical data, not reinterpreted. A failed or unavailable preparation is not an effective OFF/ON comparison, not a censored decoded-upshift result and not an optimization pass.
- Keep shared origin counters explicitly aggregate: proactive fetches can overlap ordinary playback. Use separate LAN ingress and decoded-frame evidence for actual client demand and display recovery, rather than resetting or relabelling origin events.
- Distinguish proactive server origin requests from player demand. Never label a prewarm GET as the player's first high-rendition request or use it as evidence of decoded upshift.
- Measure recovery to an observed decoded high frame, actual observed rendition sequence, playback timeline/stall observations and bounded decoded-frame observation gaps. Report unavailable observations and recovery not seen within the bound explicitly; do not convert missing evidence into zero latency or successful optimization.
- Observe each diagnostic player uninterrupted for six hundred seconds before exercising pause, seek and playback-rate controls. Freeze recovery and window-specific frame-gap evidence before controls; control-stage frames cannot establish natural upshift. The cooperative deadline also covers those subsequent controls.
- Headless decoded-frame observations do not prove the visible App has no black screen. Actual presentation/audio continuity needs separate macOS UI validation; simulator results do not establish physical Apple TV behavior.
- Use pinned signed local products and verified system mute. Keep source/media/artifact/time budgets finite, no live credentials and no default CI runtime expansion for this opt-in comparison.

## Delivery Plan
1. Deliver the opt-in comparison, admission/parser regression tests and this decision record. A valid negative result completes the diagnostic slice, not the broader latency objective.
2. Use the paired evidence to decide whether server preparation materially improves actual display latency before specifying a production preparation/retention API or enabling automatic behavior.
3. If client buffering still delays recovery, discuss a separate media-delivery policy experiment. Do not silently add pacing, mutate published segments, cap client buffers, replace items or introduce dual players as a fallback.

## Current State
- The opt-in `just test-media-prewarm-comparison` implements this bounded comparison with pinned signed products, fresh sequential control/treatment state and validated window-specific observations. Three distinct high presentation timestamps confirm recovery; the first timestamp in that confirmed batch supplies its measured onset.
- The existing [owned assessment](2026-11-11-hls-owned-assessment-b46404.md) remains read-only. This completed diagnostic-tool slice introduces no production control-plane schema or prewarm controller and does not claim that automatic upshift is faster.
- Standard three-case native acceptance remains independent; the comparison is not part of the default GHA media runtime. Raw observations and exact-head validation evidence belong to the delivery PR, not a new default playback policy.

## Next Steps
- Evaluate the paired decoded-frame results in the [recovery-latency workstream](2026-11-10-native-abr-latency-b46399.md) before specifying a production preparation/retention API.
- If preparation alone is insufficient, discuss a separate delivery-policy experiment. Buffer caps, forced item replacement, changed published media and dual players remain outside this decision.

## Evidence
- [Single-player prewarm comparison PR #98](https://github.com/Joey-Project/tvOS-net-player/pull/98).
- [Bounded preparation prerequisite PR #99](https://github.com/Joey-Project/tvOS-net-player/pull/99), merged as `f6f2ec757c57c9d7de1f7d240554e1ce988d061f`. The earlier `b0211ec0e58832613130f8a083b25806ae5feb8a` treatment failed its preparation deadline and is not an effective OFF/ON comparison.
- [Owned assessment PR #97](https://github.com/Joey-Project/tvOS-net-player/pull/97).
- [Native recovery-latency workstream](2026-11-10-native-abr-latency-b46399.md).
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
