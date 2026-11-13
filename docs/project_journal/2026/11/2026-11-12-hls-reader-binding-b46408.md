---
id: 20261112-b46408
title: Stable HLS Reader Binding
status: completed
created: 2026-11-12
updated: 2026-11-12
branch: codex/hls-playback-reader-binding
pr: https://github.com/Joey-Project/tvOS-net-player/pull/101
supersedes: []
superseded_by:
---

# Stable HLS Reader Binding

## Decision
- Keep one client-generated canonical lowercase UUIDv4 for one loaded item,
  independently of five-second context tickets, pause/resume and renewal.
  Negotiate capability 22; optional Open/ticket fields bind and echo this ID.
  Empty fields preserve capability-21 legacy reporting.
- Keep the Update wire message unchanged. Its local Swift DTO carries the
  expected reader from the previously validated ticket, so the transport and
  reporter both reject mismatched successor echoes without a transport-owned
  context map.
- Bind each accepted timing context to an exact registered session/generation
  and immutable reader ID. Separate readers keep separate samples. Multiple
  live contexts claiming the same reader are ambiguous, not a merged buffer.
- Carry `playback_reader` explicitly through generated master, video/audio
  playlists, initialization and media URIs. Relative child URIs do not inherit
  a master's query. Missing, malformed, duplicated, unknown, stale or mismatched
  bindings never demote generic HTTP continuity or grant authorization.
- Reader correlation is not authentication, a cache lease, origin capacity or
  permission to switch quality. Do not infer owners from IPs or connections.
- Keep logical source/progress/resumption identity separate from decorated
  playback URLs. An explicitly invoked helper may construct a bound URL, but
  this slice does not automatically change player items or saved source URLs.
- The URL helper preserves the original URL for unsupported legacy servers.
  With capability 22, invalid ownership/path or a conflicting, malformed or
  duplicate reader query returns no bound URL; it never silently retains
  another reader's association. An exact existing own binding is idempotent.

## Scope
- Extend Rust context storage/handlers and generated HLS URI emission, Swift
  capability/DTO/transport and explicit AppCore reporting ownership.
- Preserve legacy playlist output when unbound, media bytes, cache keys,
  HEAD/Range behavior, representation validation and existing cancellation caps.
- Do not enable automatic reporting, indexed-range demotion, prewarm admission,
  rendition switching or a modified player buffer. Suspend-aware timing and
  acknowledged seek/pause invalidation are prerequisites before consumers.

## Validation
- Cover capability fallback, malformed canonical IDs, exact echo and successor
  binding, two independent readers, duplicate-owner ambiguity, generation,
  expiry, old cleanup and legacy unbound calls through generated transport.
- Cover master-to-variant/audio-to-init/media propagation, duplicate/malformed
  query fallback, independent readers, unchanged generic output, cached/partial
  resources and normal HEAD/Range behavior without credentials.
- Require full format/lint/local CI, unchanged fixed-signed native3 with system
  mute, current-head GitHub CI/UI artifacts, GitHub Codex review and every
  conversation resolved before normal protected merge. Muted automation is not
  audible, visible-black-screen, live Bilibili or physical-device acceptance.

## Next Steps
- Consume bound readers with validated MP4 indexes and near/future admission.
  Protect missing near-term low/audio; high preparation may precede far-low only
  with a safe continuous buffer and fresh adequate server-to-origin supply.
- Establish sleep-inclusive freshness for context and origin evidence, plus
  seek/pause ordering that does not assume queued remote cleanup already ran.
- Integrate the real path into PR #98's separate complete OFF/ON diagnostic,
  keeping its recovery-owned deadline, byte/attempt and readability limits.

## Evidence
- [Stable reader binding PR #101](https://github.com/Joey-Project/tvOS-net-player/pull/101).
- [Context foundation PR #100](https://github.com/Joey-Project/tvOS-net-player/pull/100).
- [Playback urgency contract](2026-11-12-playback-urgency-context-b46407.md).
- [Incomplete comparison PR #98](https://github.com/Joey-Project/tvOS-net-player/pull/98).
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
