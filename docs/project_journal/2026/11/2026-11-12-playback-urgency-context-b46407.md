---
id: 20261112-b46407
title: Playback Urgency Context Foundation
status: completed
created: 2026-11-12
updated: 2026-11-12
branch: codex/playback-urgency-context
pr: https://github.com/Joey-Project/tvOS-net-player/pull/100
supersedes: []
superseded_by:
---

# Playback Urgency Context Foundation

## Decision
- Protect missing near-term video/audio first. Once fresh continuous buffering
  establishes the protected window, higher-rendition preparation may outrank
  far-future lower-rendition prefetch. Do not mistake every AVPlayer prefetch for
  an immediate continuity requirement.
- If fresh representation-bound server-to-origin evidence is insufficient for
  higher video plus audio and safety headroom, hold new higher-rendition work
  and continue lower-rendition supply. Reevaluate when new evidence arrives.
  Fast LAN delivery and cache hits do not establish origin capacity.
- Preserve one player/item, default buffering and whole-asset download intent.
  Temporary download ordering does not replace buffered media, rewrite segments
  or cancel eventual complete caching. No non-preemptible transfer quantum is
  introduced by this decision.

## Foundation Contract
- Add capability 21 and explicit Open/Update/Close playback-context RPCs without
  changing legacy progress reporting or read-only readiness. The foundation
  stores timing observations only; it neither schedules media nor enables
  automatic reporting, prewarm, switching or URL rewriting.
- A server-issued context is bound to an existing exact session/generation.
  Each single-use challenge lasts at most 5,000 milliseconds on the server's
  monotonic clock. The client captures a fresh playing snapshot after receiving
  the challenge and conservatively ages the ticket from its RPC request start.
  No clock synchronization or client wall-clock expiry is assumed.
- An update consumes the expected challenge and sequence, then issues a new
  challenge. The accepted snapshot still expires at the consumed challenge's
  original deadline; issuing the successor does not refresh old observations.
  Late, replayed, cancelled or obsolete responses cannot supersede a new owner.
- Require finite nonnegative position, continuous buffered end at or after the
  playhead and a positive finite playing rate no greater than 16. Pause, seek,
  reload, stop and owner changes withdraw old context. Missing/expired context
  is not evidence of a safe buffer.
- Keep at most 256 process-local contexts and eight per session, pruning expired
  entries before admission. Close is idempotent and scoped to an exact context;
  generation changes and session removal invalidate observations. Multiple live
  owners are ambiguous, not one merged safety window.
- Withdraw local ownership before best-effort remote cleanup. Cleanup runs
  independently of the cancelled caller, with a one-second production RPC
  deadline and a one-second local wait. Each service permits only one outstanding
  cleanup, including a non-cooperative client that outlives the wait; late
  completion cannot revoke a newer owner. Server expiry remains the fallback.
- Session generation and per-context sequence retain unsigned 64-bit protocol
  values with checked bounds and no wraparound. Ticket validity is explicitly
  bounded despite its unsigned 64-bit wire representation. UUID context and
  challenge identifiers are process-local correlation, not authentication.
- A context does not identify the owner of an HTTP media request, reserve cache
  bytes, prove serving capacity or authorize a quality switch. Future scheduling
  must additionally bind each reader and indexed range to the correct owner.

## Validation
- Deterministic Rust coverage exercises expiry, replay, sequence/challenge
  mismatch, invalid samples, bounded admission, independent-owner cleanup,
  generation/removal and conservative multi-owner lookup. Handler integration
  must not restore sessions or fetch origins. Completed-cache registration also
  invalidates a full set of old-generation contexts before admitting a new one.
- Swift DTO and generated gRPC transport tests cover exact identities, bounds,
  unsupported servers, cancellation and malformed responses. Shared AppCore
  tests capture only after ticket receipt and exercise stale clocks, delayed
  responses, owner invalidation, serial updates and old cleanup races without
  player transport mutations. Actual generated transport exercises cleanup
  after caller cancellation, while non-cooperative cleanup remains bounded.
- Complete formatter/linter checks, local CI, the unchanged fixed-signed native
  three-case gate with verified mute, current-head GitHub CI/UI/native artifacts,
  GitHub Codex review and resolved conversations remain required. These checks
  do not establish faster upshift, audible continuity or physical Apple TV
  acceptance.

## Delivery Sequence
1. Deliver this explicit context foundation independently from the incomplete
   single-player prewarm comparison. It does not enable scheduler consumers.
2. After merge and master refresh, start a new branch for media-reader ownership,
   indexed near/future range classification and source-aware priority admission.
   Unknown, conflicting or stale ownership remains continuity-protected.
3. After that merge and master refresh, integrate the real scheduling path into
   the separate OFF/ON diagnostic. Keep its original recovery-owned 15-second
   deadline, byte/attempt limits and complete selected-range readability checks,
   plus the independent strict automatic high-low-high baseline.

## Current State
- The Rust store and generated gRPC handlers implement bounded ephemeral timing
  reports, exact-context cleanup and generation invalidation. Expiry is checked
  with a fresh monotonic observation after the store lock is acquired, rather
  than with a timestamp that may have aged while queued.
- Completed-cache runtime registration invalidates old contexts in the same
  generation-change callback as ordinary runtime registration, so obsolete
  owners cannot consume the new generation's admission slots.
- Shared Swift models and generated transport validate exact binding and ticket
  succession. The explicitly invoked AppCore service separates ticket receipt
  from request-start aging, rejects pre-ticket cached snapshots and rechecks the
  consumed ticket after a delayed update. New-owner results survive old cleanup.
- Component coverage includes actual generated transport and deterministic
  timing/ownership failures without player transport actions. This foundation
  enables no automatic reporter, reader binding or scheduler consumer. The
  remaining ordering and upstream admission work follows in separate slices.

## Evidence
- [Playback context foundation PR #100](https://github.com/Joey-Project/tvOS-net-player/pull/100).
- [Recovery-latency workstream](2026-11-10-native-abr-latency-b46399.md).
- [Owned read-only assessment](2026-11-11-hls-owned-assessment-b46404.md).
- [Incomplete single-player comparison PR #98](https://github.com/Joey-Project/tvOS-net-player/pull/98).
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
