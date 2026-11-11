---
id: 20261111-b46403
title: Read-only HLS Rendition Catalog
status: completed
created: 2026-11-11
updated: 2026-11-11
branch: codex/hls-rendition-catalog
pr:
supersedes: []
superseded_by:
---

# Read-only HLS Rendition Catalog

## Scope
- Continue the [native playback snapshot](2026-11-10-hls-playback-context-b46402.md) and [shared assessment](2026-11-10-hls-upshift-policy-b46401.md) with exact registered rendition metadata over the existing control plane.
- Add the backward-compatible `CacheService.GetHlsRenditionCatalog` RPC and capability 20, plus an explicitly invoked typed Swift client shared by macOS and tvOS.
- This is metadata acquisition only: no player preference, prewarm, seek, item replacement, polling, download or demonstrated recovery-latency improvement.

## Contract
- Return a point-in-time catalog for an already registered, serving-authorized session. Do not restore cached sources, refresh credentials, contact origins or record playback/cache use.
- Reuse the ordinary master path's resource availability and weak-network filtering/fallback. A raw registered candidate, selected task variant or submitted progress record is not proof of current advertisement or native decoding.
- Separate the literal HLS `BANDWIDTH` declaration from optional nominal rates of the exact video and separate-audio resources. The declaration may use the existing master default; never infer independent resource rates from it or report it as measured supply.
- Preserve optional format hints and declared dimensions without inventing decoder support. A separate audio resource does not prove an audible/decoded audio track. Missing or unsupported format hints remain unknown.
- Export no upstream URLs, request headers, cache keys, credentials, absolute clocks or handover-safe flag. Bound and sanitize codec/MIME hints; do not normalize opaque identifiers or collapse distinct UTF-8 identities.
- Bound the catalog to 128 renditions and 64 KiB encoded metadata. Oversized, duplicate or invalid server state fails explicitly rather than returning a truncated candidate set.
- A successful catalog is nonempty. An authorized registration without a complete normally servable representation returns `FailedPrecondition`; an unavailable or unauthorized registration returns `NotFound`. Neither case invents an empty successful catalog or restores media.
- Keep blocking cache inspection concurrency bounded and its lifetime guard alive until the worker ends, including caller cancellation. Recheck registration identity/generation and serving authorization before returning; process-local generation is not restart authority or a lease.
- Old servers return the existing extensible unsupported-operation shape; do not add cases to closed public error enums. Preserve cancellation precedence and reject malformed response identity/presence/bounds.

## Validation
- Focused Swift development validation passes 30 tests: four catalog model tests, eight production catalog transport tests, nine readiness tests and nine prior upshift transport regressions. Strict formatting passes for the six involved Swift files; this does not replace frozen-head full validation.
- Rust development validation passes the catalog filter (13 tests) and master filter (14 tests), with zero failures. These filters overlap and are not a combined unique-test count. The generated gRPC cached-positive test uses actual downloaded video/audio cache files, clears upstream URLs and verifies no new origin requests, playback lease or fill from the query. Full `just lint` also passes.
- UI scenario setup requires the complete expected fixture source before its single submission. Synchronize tvOS keyboard editing and retain bounded phase-specific input diagnostics; truncated input must fail setup rather than masquerade as a task-watch recovery failure. Preserve all existing recovery deadlines, single-submission and playback assertions.
- Cover live-style and offline-cache availability, weak-policy fallback, disabled alternates, unavailable/unauthorized/changed registrations, limits, optional hints/rates and exact UTF-8 identities.
- Compare catalog selection with real loopback HLS master output and prove no origin requests, source restoration, fill promotion or playback-use side effects are created by the query.
- Exercise production generated Swift gRPC transport, unsupported-server fallback, cancellation and response admission independently of native playback.
- Require full lint/format/local CI, the unchanged fixed-signed three-case native suite with verified mute, exact-head GHA CI/UI/native artifact evidence, current-head GitHub Codex review and zero unresolved conversations before ordinary protected squash merge.
- Keep actual audible audio, authenticated live Bilibili and physical Apple TV validation separate; none is implicit in this metadata slice.

## Next Steps
1. Bind catalog/readiness/native timing to a current owned item/session during asynchronous production preparation, revalidating all observations before any action.
2. Implement a separately agreed target-specific serving and handover path that preserves audio and timeline continuity. The production handover preference remains unconfirmed; retain the pure automatic long gate.

## Evidence
- [Native playback snapshot PR #95](https://github.com/Joey-Project/tvOS-net-player/pull/95).
- [Recovery latency workstream](2026-11-10-native-abr-latency-b46399.md).
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
