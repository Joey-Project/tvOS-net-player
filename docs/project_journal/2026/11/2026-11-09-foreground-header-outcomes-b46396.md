---
id: 20261109-b46396
title: Foreground Range Header Outcomes
status: completed
created: 2026-11-09
updated: 2026-11-09
branch: wip/cache-header-outcomes
pr: https://github.com/Joey-Project/tvOS-net-player/pull/89
supersedes: []
superseded_by:
---

# Foreground Range Header Outcomes

## Scope
- Deliver a focused LAN cache prerequisite before native weak-network, ABR and transcoding acceptance. Build on the completed foreground Range and fixed local signing foundations.
- Separate validated upstream response-header readiness from complete, durable range publication. Do not equate either boundary with native decoding or automatic bitrate recovery.
- Keep the work inside RustCacheServer. If an upstream BBDown-rust contract is demonstrably missing, stop at that dependency and provide the operator with the required interface, reproduction and acceptance criteria instead of modifying the upstream repository.

## Design
- Before committing a LAN success response for a partially cached request, establish the outcome of its first missing bounded range, including requests whose cached prefix exceeds one delivery chunk.
- Reuse the same controlled range fetch for header readiness and eventual body validation. Do not add a duplicate connectivity probe or an unowned detached download.
- A narrow owned outcome may become ready only after applicable status, Content-Range, total-length and resource-identity checks. Required prefix or cross-origin validation still precedes readiness.
- Preserve the 512 KiB startup identity proof, 64 KiB foreground origin requests, larger background full-fill strategy, ETag/cross-sampling rules, cancellation, preemption, flight sharing, quota and publication guards.
- Keep the completion API, on-disk manifest and gRPC schemas compatible. Fully cached responses must remain independent of the upstream network.
- Header readiness never makes unvalidated body bytes readable. Only complete, length-checked, identity-validated atomic range publication permits cache reads.
- If every candidate fails before accepted headers, return the existing sanitized non-success LAN response rather than a success response followed by a body abort. Once valid headers have been accepted, later truncation or transport failure remains a body error; an already-sent status cannot be rewritten.

## Validation
- Use credential-free local HTTP fixtures with explicit body gates to distinguish header readiness, durable publication and body completion without timing guesses.
- Cover known-identity cold gaps, warm prefixes with missing tails, prefixes larger than one delivery chunk, all-cache hits, upstream rejection and invalid range or identity responses.
- Cover shared flights, foreground interest, request drop/cancellation, preemption, truncated bodies and cache/quota cleanup. Existing integrity and fallback regressions remain required.
- Run complete default tests, formatter and linter checks, current-head GitHub CI and GitHub Codex review; resolve every conversation before the authorized merge. No local formal review lane is required.
- Keep authenticated Bilibili requests, native playback acceptance and dedicated media GHA integration separate. Any later playback-capable test requires verified mute and the pinned signed executable paths.
- The complete credential-free `just test-cache-server` passed: 1,203 library tests, 60 harness tests and six integration tests. One existing opt-in live test remained ignored. The final four Rust source hashes were unchanged across this run; this pre-commit source proof is not an exact-head CI receipt.
- Repository Rust formatter and workspace all-targets Clippy checks passed. Focused runs additionally covered 24 range-cache tests, 20 HLS segment tests and the named gated LAN, warm-prefix rejection and quota consumer cases. These overlapping commands are not an additional unique-test total.
- The completed-cache regression failed before the fix and passed afterward, with the upstream unavailable and the obsolete partial checkpoint absent. A separate existing in-flight completion test now survives an unknown-to-known size publication without weakening manifest identity checks.
- Truncation timing is explicit: the fixture releases its failing origin body only after the LAN client receives and checks the success headers. Immediate transport abortion is not substituted for this post-header assertion. Truncated bytes remain unpublished.
- The quota case directly drives the stream consumer with a real observed ensure and a local upstream. It proves error dispatch, correct fallback bytes and no durable publication, not the exact timing of a late quota failure over HTTP.

## Implementation Boundary
- The observed preparation handle owns its original completion future rather than spawning a detached task. Its narrow result describes the prepared first gap, not completion of the entire HTTP request.
- Shared flights carry an owned readiness snapshot. Late joiners must inspect the current value instead of requiring a second header event.
- The HTTP stream retains the preparation handle while delivering its verified cached prefix, then completes the pending bounded fetch before reading its newly durable bytes.
- Existing full-range ensure callers retain their completion semantics. No upstream library or protocol change is required by this interface.
- Validated completion metadata, including ETag, travels with the preparation result. Completed resources do not need a deleted partial checkpoint, and metadata reads remain within the range activity guard.
- Pending completion errors use the existing stream error dispatch, preserving quota tail fallback and bounded expired-URL recovery. Successful prepared gaps are recorded as network work only after validated content is readable; mixed requests are not silently counted as pure cache hits.
- In-flight preparation retains its original resource descriptor when a compatible runtime publication adds a previously unknown size. Strict persisted resource bindings and representation checks remain intact.

## Next Steps
- Resume native media integration and unchanged automatic high-to-low-to-high decoded-frame acceptance after this foundation lands. Its outcome remains unproved by HTTP fixture success alone.

## Evidence
- Base: `2c9c0d4013caae09c077074b6196583789080785`, the merged fixed-signing prerequisite.
- Roadmap: [Home LAN Reliability And Preview Readiness](../10/2026-10-23-home-lan-readiness-roadmap-b46388.md).
