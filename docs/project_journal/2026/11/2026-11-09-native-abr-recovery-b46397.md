---
id: 20261109-b46397
title: Native ABR Recovery And Media Acceptance
status: active
created: 2026-11-09
updated: 2026-11-09
branch: wip/native-media-header-integration
pr:
supersedes: []
superseded_by:
---

# Native ABR Recovery And Media Acceptance

## Scope
- Finish slice 3A of the [home LAN roadmap](../10/2026-10-23-home-lan-readiness-roadmap-b46388.md): automatic native bitrate recovery first, then every remaining local media acceptance gap.
- Start from `2ad666498b4cefa1bf33488d606e6c514955a054`, the merged [header-outcome prerequisite](2026-11-09-foreground-header-outcomes-b46396.md). Preserve the merged foreground cache API and [fixed local signing contract](2026-11-09-stable-local-signing-b46395.md).
- Selectively integrate preserved bounded MP4 indexing, production-router fixtures, HLS audio identity and duration corrections, decoded-frame probe and bounded runner. Do not copy obsolete cache or signing implementations from earlier working trees.
- Bring forward focused slice 3B regression protection only where it prevents a demonstrated integration regression. A dedicated GHA media job remains separately deliverable if it is not needed for this fix.

## Acceptance Checklist
Implementation checkmarks describe development evidence, not final-head delivery or a successful ABR recovery case.

- [x] Parse supported progressive indexes and terminal metadata within bounds; preserve safe fallback, validated-prefix reuse and representation identity. Revalidate the final delivery candidate.
- [x] Exercise production HTTP routes with credential-free, paced local media and record actual upstream failure and recovery-phase traffic. This does not establish a recovered high rendition.
- [ ] Decode automatic 640x360, 320x180, then 640x360 with at least three distinct display timestamps per phase, conservatively aligned with the real fault timeline.
- [x] Decode production H.264/AAC transcoding output with audio-track evidence and every required pause, resume, seek and playback-rate control. Revalidate the final delivery candidate.
- [x] Validate probe, runner, report admission and malformed/failed-result regressions, including complete named-case execution rather than compilation alone. Revalidate affected checks after changes.
- [ ] Pass complete default tests, lint/format, GitHub CI/UI and current-head GitHub Codex review; resolve every conversation before normal exact-head merge.

## Invariants
- Keep the 160-second ABR input, 130-second probe, 175-second probe deadline, 225-second case limit, decoder-ready baseline and 45-second high-rendition fault. Do not force manual quality or weaken frame, clock, control, integrity or resource assertions.
- Use pinned signed stable executables for local native acceptance, verify system mute before playback and leave it muted. Muted tests do not establish audible audio acceptance.
- HTTP header readiness, validated cache publication, native decoding, full App UI and full offline completion are distinct evidence boundaries.
- Preserve prior and current failed recovery attempts as negative evidence. Correct upstream HTTP error delivery is not proof of native ABR recovery.
- Keep authenticated live traffic, physical Apple TV, persistence/gRPC schema changes and distribution outside this slice. A demonstrated missing BBDown-rust contract requires an explicit handoff to the operator, not an unplanned upstream modification.

## Current Evidence
- The prerequisite passed HTTP/cache regressions and full delivery gates. Its integrated native successor still has not established recovered-high frames.
- The earlier integrated native runs decoded high then low but never recovered high within their bounded observation. The per-fragment query-URI experiment also failed and was withdrawn.
- The first native run on the merged header foundation executed both cases through fixed signed executables. Transcoding passed with 805 decoded frames, H.264/AAC 640x360 output, one audio track and all nine controls. ABR failed after 2,806 decoded frames: high samples at about 8.1 seconds, low at 27.4 seconds, and no recovered-high samples by the 118-second probe exit. No unchanged retry was performed.
- This run observed two high-origin 503 responses and one LAN 502 header response. Recovery-phase high ingress and origin counts were both zero; low playback continued. Numeric player errors included URL -1005 and CoreMedia -16848. Correct pre-header rejection therefore does not, by itself, establish automatic upshift; an earlier accepted response may still fail during its body.
- All recorded native source hashes stayed unchanged throughout the run. System mute was verified before and after playback and left enabled. The fixed probe's 59 self-tests and runner's 21 helper tests passed independently afterward.
- Focused Rust integration checks passed 193 tests across parser, HLS, cache and fixture groups, plus formatting and Clippy. Additional dedicated prefix and media-route coverage below closes the gaps left by that initial integration run; general cache success alone was not treated as proof of the new API's boundary.
- Media-route regressions passed all 78 cases, including cold/prewarmed SIDX, unsupported optional tails, startup-prefix reuse and explicit temporary-error responses. An initial prefix-route failure came from fixture data not matching its declared size; using the matching fixture fixed the test without relaxing cache validation. Numeric body-outcome EOF/error/drop, framing transparency and bounded rare-error retention are covered by the five telemetry cases below.
- Dedicated prefix admission/publication tests brought the complete cache group to 135 passes. Framing and telemetry checks passed five cases, preserving empty and HEAD bodies, Content-Length behavior and the 32-event cap. Formatter and Clippy checks passed.
- The second native run still failed automatic upshift: 2,840 frames, high at about 7.5 seconds, low at 26.8 seconds and no recovered-high samples by the 117.5-second exit. Transcoding passed with 807 frames, an enabled audio track and all nine controls. Both runs retained the same acceptance bounds and left system audio muted.
- The second run confirmed a high-response header at 13.070 seconds before the 14.183-second fault transition, followed by its body error at 14.247 seconds. A later high request returned LAN 503 at 15.256 seconds. Recovery-phase high ingress and origin counts remained zero. Correct temporary-status mapping is therefore not a sufficient recovery fix.
- Body-stream drop is an observer-lifecycle event, not independent proof of client cancellation: a known-length body can be dropped after all bytes are observed without a final EOF poll. Likewise, `buffer_full=true` does not prove that the entire asset is buffered. These diagnostic boundaries must remain explicit.
- A third run added only passive final buffer observations; it still failed upshift after 2,800 frames, while transcoding passed with 809 frames. The player reported one valid, non-truncated loaded range `[0, 159.977333]` seconds at a `98.608903`-second playhead, leaving about `61.368431` seconds readily available ahead. The forward-buffer preference was `0` (automatic). This establishes buffer state, not the sole cause of absent upshift or durable whole-file cache completion.
- The passive probe passed 61 self-tests, including invalid-range filtering and the eight-observation cap, plus strict formatting and pinned-signature checks. Native source hashes remained stable, owned processes and locks were released, and system mute remained enabled after the third run.
- The complete default Python/Rust regression command passed 170 Python helpers, 1,245 Rust library tests, 60 harness tests and six integration tests: 1,481 passes, zero failures and three intentional opt-in ignores. Rust binary and doc-test targets contained no tests. This dirty-candidate source proof is not full `just ci`, exact-head delivery evidence or native ABR acceptance.
- The subsequent complete local `just ci` passed with exit zero and a complete 16,745,772-byte log: Python helpers 170, Swift package tests 568, tvOS hosted XCTest one, macOS hosted XCTest three, probe self-tests 61, Rust library 1,245, harness 60 and integration six. The three opt-in Rust cases remained ignored. These groups overlap the earlier focused/default runs and are not additional unique-test totals. All source hashes and the dirty-file list were stable through the run; system mute remained enabled.
- Local default CI was explicitly credential-free. Its simulator/app-shell tests are not the separate five-case UI jobs, its default ignores are not native media passes, and its uncommitted candidate evidence is not final-head GitHub CI or review. Native recovery and the final delivery gates remain open.

## Follow-up Design
- Explicit upstream 503 is preserved as sanitized temporary unavailability with a bounded retry hint and route regressions. Upstream bodies, credentials and arbitrary headers are not forwarded.
- This protocol correction is not a demonstrated explanation of the failed upshift. [Apple's HLS error-handling guidance](https://developer.apple.com/videos/play/wwdc2017/514/) distinguishes temporary server unavailability from invalid gateway responses; it does not promise a fixed rendition-recovery deadline.
- Prefix-seeding, SIDX route coverage and bounded passive buffer observations are implemented without replacing the merged cache/header foundations. Discontinuous loaded ranges remain explicit. Keep server-side whole-asset fill unchanged.
- Do not infer a unique cause from the measured forward buffer. A finite client buffer preference would be a deliberate policy experiment, not a hard cap or an accepted fix; no such override is currently applied.
- The operator clarified that excess buffering is not itself a problem. Preserve the automatic buffer preference and whole-asset downloading; do not treat the proposed 20-second experiment as approved. First define whether recovery concerns only future unbuffered media or should also replace already buffered low-quality media, then align the implementation and acceptance scenario with that product contract.

## Next Steps
1. Confirm recovery semantics for already buffered low-quality media versus future unbuffered media. Preserve all three failed runs; do not infer an undocumented variant-disable interval or automatically reduce buffering.
2. Select the focused recovery change and corresponding evidence scenario from that explicit contract. A material policy or acceptance revision needs a documented decision, not a test-only override. Revalidate through the pinned signed executables.
3. Complete the checklist and exact-head delivery gates. After merge, update master before starting a successor PR.
