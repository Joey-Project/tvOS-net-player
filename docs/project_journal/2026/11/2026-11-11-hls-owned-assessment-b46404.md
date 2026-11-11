---
id: 20261111-b46404
title: Owned HLS Upshift Assessment
status: completed
created: 2026-11-11
updated: 2026-11-11
branch: codex/hls-upshift-session-binding
pr:
supersedes: []
superseded_by:
---

# Owned HLS Upshift Assessment

## Scope
- Join the [rendition catalog](2026-11-11-hls-rendition-catalog-b46403.md), [native timing snapshot](2026-11-10-hls-playback-context-b46402.md) and [shared readiness assessment](2026-11-10-hls-upshift-policy-b46401.md) through an explicitly invoked shared AppCore entry point.
- Bind each asynchronous evaluation to the actual current player/controller/item, declared server/endpoint/session identity and a local evaluation revision. Reject obsolete results, including non-cooperative cancellation and same-URL reloads.
- Keep this read-only: no polling, prewarm, fill promotion, preference change, seek or item replacement. Preserve default buffering, whole-asset fill and the pure automatic long native gate; no recovery-latency improvement is claimed.

## Contract
- Carry optional structured HLS server/session metadata in the existing playback context. Do not infer a session from URL paths, library IDs or selected quality labels. Legacy, unsupported and unknown associations stay unbound without preventing playback.
- Preserve exact UTF-8 session, server and variant identities. Use the actual loaded URI only to verify the supplied metadata association, not to derive an identity. Never publish URLs, credentials or raw transport errors in the assessment.
- New evaluations supersede old ones. Load/reload, stop/clear, manual interaction, seek/replay, rate changes and observed time jumps/end invalidate pending evidence. Cancellation saves resources; explicit ownership checks after every await are authoritative, and old cleanup cannot cancel or overwrite a newer evaluation.
- Fetch a fresh catalog per evaluation, choose only the exact advertised target, retain unknown nominal resource rates/audio/decoder support and pass its nonzero generation to readiness assessment. Never derive independent rates from aggregate advertised BANDWIDTH or decoder support from format hints.
- Age the native snapshot across catalog and readiness RPCs with conservative monotonic arithmetic and the existing policy freshness limit. Missing, stale, cancelled, clock-invalid or obsolete preparation returns no owned assessment. Valid current policy holds remain typed observations, not a command.
- Revalidate native object ownership, current readiness, playing/rate state, local revision and freshness at completion and before later consumption. An owned result is not a cache lease, delivery-capacity proof, target-serving guarantee or permission to switch.
- Endpoint/server ID and process-local generation cannot prove a server process survived a restart. Explicit server/client scope invalidation discards pending work; no cross-restart epoch, persistence or authorization is invented.

## Validation Plan
- Deterministically gate catalog and readiness responses to test reverse completion, replacement, same-URL reload, stop/clear, seek/replay, rate/state change, stale clocks, unsupported servers, identity/generation mismatch and cancellation ignored by the transport.
- Verify normal current results and explicit structured task/result context forwarding, including missing metadata and changed saved-source association. Prove no extra player transport actions or automatic requests are created by binding.
- Exercise the production generated gRPC path for catalog-to-readiness composition and exact raw identifiers where practical, without authenticated internet traffic.
- Require complete lint/format/local CI, pinned fixed-signed native3 with verified mute, exact-head GHA CI/UI/native artifacts, current-head GitHub Codex review and zero unresolved conversations before normal protected squash merge.

## Current State
- Shared AppCore exposes an explicit owned assessment and a separate current-owner/freshness check. Native object references stay weak across RPC waits, and late responses or old cancellation cannot supersede a newer evaluation.
- Exact task/result metadata carries the optional HLS association. Saved playback with a changed source remains playable but unbound; absent metadata does not trigger URL-derived identity or automatic requests.
- Unit and generated gRPC loopback coverage exercises policy holds, exact identifier bytes, generation mismatches, stale clocks, reverse completion, cancellation, source replacement and transport cleanup, without changing player transport actions.
- Composition-backed fake players use isolated notification centers; production retains the default native event source. Deterministic notification tests await actual cancellation and cover current-item time jumps/end plus obsolete-item rejection, rather than relying on a scheduling yield.
- This completes passive composition only. Default buffering, whole-asset fill, automatic native acceptance and the unresolved production handover decision remain unchanged.

## Next Steps
1. Agree on a separate production target-serving/handover mechanism before any active quality change.
2. Compare a separately implemented assisted path against the retained pure automatic baseline; continue the [latency workstream](2026-11-10-native-abr-latency-b46399.md) without relabeling metadata or ownership tests as optimization.

## Evidence
- [Read-only catalog PR #96](https://github.com/Joey-Project/tvOS-net-player/pull/96).
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
