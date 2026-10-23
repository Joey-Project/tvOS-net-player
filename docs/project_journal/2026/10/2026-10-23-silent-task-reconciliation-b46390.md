---
id: 20261023-b46390
title: Silent Task-State Reconciliation
status: completed
created: 2026-10-23
updated: 2026-10-23
branch: codex/silent-task-reconciliation
pr:
supersedes: []
superseded_by:
---

# Silent Task-State Reconciliation

## Approved Scope
- Deliver slice 1 of the [home LAN readiness roadmap](2026-10-23-home-lan-readiness-roadmap-b46388.md) after [PR #82](https://github.com/Joey-Project/tvOS-net-player/pull/82), merged as `a1956fca4f6ff82d69605e6c0255015e7c628a2d`.
- Local master was fast-forwarded to that verified squash commit before creating this branch. Its tree matches the reviewed CI-slice tree; no predecessor work is left unmerged.
- Extend the shared AppCore task-observation lifecycle, not the protocol or server task scheduler. Keep macOS and tvOS behavior aligned.
- Same-device task/playback persistence remains the next separate slice. Do not add automatic task creation, playback resubmission, credentials, temporary-URL persistence or cross-device history here.

## Reconciliation Contract
- An open watch with no messages is not proof of failure. After a quiet observation window, read the original task through the existing bounded `GetTask` operation.
- Use a conservative default 30-second observation interval. Accepted watch activity, including identical valid snapshots, defers the idle read. Tests may inject a shorter interval or a cancellation-aware sleeper.
- Measure the quiet deadline from the latest accepted watch event using monotonic time, rather than adding a full interval when a periodic tick notices activity. An immediate subscription snapshot must not double the initial quiet window; later events wait only the remainder of their own quiet window.
- Successful unchanged reads keep the existing subscription and healthy presentation. They do not consume reconnect attempts, show an error or restart playback.
- Keep at most one idle reconciliation read active. Cancel its lifecycle before genuine stream-recovery reads and on clear, replacement, terminal completion or teardown. Pause new idle reads during an in-flight cancellation request and invalidate older responses; preserve authoritative observation when cancellation remains unconfirmed or pending.
- Bind every asynchronous result to the original task, operation and observation generation. Reuse snapshot-generation and output-revision fences so a late unary result cannot replace a newer watched snapshot or resurrect cancelled work.
- A genuine retryable read failure may hand observation back to the existing bounded reconnect path for the same accepted task. Non-retryable errors use the existing unavailable/retry presentation. Neither path resolves, submits or cancels another task as a recovery side effect.
- Preserve the existing six-attempt stream-recovery backoff and genuine-progress semantics. Keep media playback independent of control-plane reconciliation.

## Implementation And Validation
- One Luna worker owns the view-model implementation and its deterministic unit regressions. A second owns real loopback HTTP/2 transport tests. A third owns the credential-free fixture, one additional native case per platform, and strict helper admission for that case.
- Unit coverage must include healthy silence, repeated unchanged reads, accepted activity, single-flight reads, terminal/output discovery, stale results, cancellation/clear/replacement and actual read failures. Use controlled sleep/continuation events rather than long arbitrary sleeps.
- Exercise the production gRPC client against an open, silent real loopback stream. Prove original task identity, one creation, no cancellation and no healthy resubscription.
- Test server streams await the pinned gRPC library's explicit RPC cancellation handle. A closed RPC does not necessarily cancel its producer's Swift task; release per-RPC ownership on normal cancellation and error rather than leaving a manual continuation suspended.
- Add a native silent-watch playback scenario on both platforms, retaining all existing cases and assertions. Require actual named-case execution plus independent fixture metrics; do not accept a green job with missing cases.
- Native acceptance should span unchanged and changed idle reads while real fixture playback advances, using the normal interval when practical. Keep media long enough for the observed window and all fixture resources bounded and owned.
- The silent native fixture mirrors the production server's immediate initial watch snapshot and then emits no more stream events. Keep its existing terminal-wait deadline so a doubled first quiet window fails the native regression instead of being hidden by a more permissive timeout.
- Freeze all writers before complete validation. Require the complete local test/lint/format/build set, actual current-head ordinary and native UI CI, current-head GitHub Codex review and resolution of every conversation before authorized merge.
- Verify output mute before any local media test and keep it muted afterward. Deterministic tests use no real Bilibili credential or public service.

## Retained Observation
- PR #82's otherwise passing macOS native results contain a non-failing SwiftUI warning about publishing changes during view updates. Preserve its result-tree evidence for focused client diagnosis; no root cause or introduction attribution is established by that observation.
- Do not fold an unrelated UI rewrite into this control-plane slice. A demonstrated defect in the changed lifecycle should receive a narrowly justified fix and regression coverage.

## Default Async Callback Compatibility
- A local debug test aborted reproducibly at the default recovery sleep. The unchanged compiled binary reproduced the failure in the original single wire case; the crash report and matching debug symbols identify the `await sleep(...)` call.
- Module and client objects contain weak definitions of the public async default closures. The local investigation did not prove a body/context-size mismatch, so it does not establish a unique compiler root cause.
- [Swift issue #92017](https://github.com/swiftlang/swift/issues/92017) reports a matching default-argument shape. Resolve optional `nil` callback defaults inside AppCore instead of emitting async closure literals into clients. Preserve normal debug validation, delays and cancellation rather than changing optimization or injecting a test-only sleeper.

## Delivery State
- Shared AppCore now reconciles quiet task watches without resubmission or healthy-watch reconnection. Both native UI suites include a fourth named silent-watch playback case, with strict independent fixture metrics.
- Focused local validation passed 152 Swift cases, including monotonic quiet-deadline boundaries, the normal-interval initial-snapshot fixture, real HTTP/2 cancellation and subscription reuse. All 103 Python helper regressions and repository pre-commit lint/format checks passed on the same unchanged source inputs.
- Full local CI, actual current-head GitHub ordinary/native UI CI and GitHub Codex review remain mandatory delivery gates. Focused tests do not substitute for those gates or live Bilibili acceptance.
- PR-specific head, checks, review, approval and merge receipts belong in PR metadata and ignored local evidence, not this squash-merge target-branch journal.
