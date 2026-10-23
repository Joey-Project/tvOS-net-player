---
id: 20261023-b46388
title: Home LAN Reliability And Preview Readiness
status: active
created: 2026-10-23
updated: 2026-10-23
branch:
pr:
supersedes: []
superseded_by:
---

# Home LAN Reliability And Preview Readiness

## Approved Goal
- Deliver the agreed eight slices sequentially after [PR #81](https://github.com/Joey-Project/tvOS-net-player/pull/81), merged as `c825285af0fe3c6b89ad7bf5b09bb49949384f16`.
- Keep tvOS primary and macOS functionally aligned, using macOS for live validation and simple daily use.
- Build on the existing server-owned resolution/CDN, full-fill/cache, ABR/transcoding, credential lifecycle and task-watch recovery implementations rather than rebuilding them.
- Planning numbers below are delivery order, not GitHub PR numbers. Each successor starts only after the preceding PR is merged and local master is updated.

## Delivery Sequence
| Slice | State | Scope and acceptance |
| --- | --- | --- |
| [0. CI stability and diagnostics](2026-10-23-ci-stability-diagnostics-b46389.md) | completed | Add toolchain/source-bound compiled caches, bounded stage logs/JSON, all-helper regressions and redacted test-waiter diagnostics. No warm-cache speedup or timeout root cause is inferred. Preserve all test, lint and build gates. |
| [1. Silent task-state reconciliation](2026-10-23-silent-task-reconciliation-b46390.md) | completed | Use bounded existing `GetTask` reconciliation when observation stays open but becomes silent. Healthy idle work is not a failure. Preserve task identity, cancellation/generation guards and media playback; never resubmit as a recovery side effect. |
| 2. Client task and playback resumption | pending | Restore the accepted task and playback position on the same client, bound to the original server and result identity. Offer an explicit Continue action rather than autoplay. Expired tasks, deleted media and a mismatched server produce an honest unavailable state, never an implicit new resolution/download. |
| 3. Media weak-network, ABR and transcoding acceptance | pending | Use controlled local upstream faults to validate latency, failed/truncated reads, CDN fallback, actual quality downgrade/recovery, representation isolation and decoding of existing transcoded outputs. Manifest presence alone is not acceptance. |
| 4. Native cache lifecycle and offline acceptance | pending | Exercise full fill during playback, upstream unavailability, same-root server restart, existing high/low watermark eviction and deletion protection through native workflows. Only genuinely complete selected representations qualify as fully offline. |
| 5. Native credential and settings UI regression | pending | Use credential-free fixtures for login readiness, expiry, cancellation, unavailable methods, endpoint replacement and existing settings warnings/conflicts. Keep real scanning and authenticated requests separate and opt-in. |
| 6. Real macOS long-play acceptance | pending | Reuse the four canonical URLs and existing bounded authenticated cases, recording restricted proxy success, actual CDN paths, complete cache/offline behavior, native seeking/replay and supported window usability. Fix only demonstrated scoped failures. |
| 7. Local and trusted-LAN preview delivery | pending | Provide explicit versioned app/server artifacts, startup/health checks, upgrade/rollback and backup instructions, and refresh outdated documentation. This is not public distribution, App Store submission or notarization acceptance. |

## Resumption Contract
- Share implementation between macOS and tvOS, but initially retain each device's own record only. Cross-device history is deferred.
- Persist only necessary server/task/result identities and playback state; never persist credentials or temporary upstream/playback URLs as a resumption contract.
- Restore observation of the original accepted task. Server-side downloading already survives client absence; reopening the app must not create another task.
- Wait for a validated playable result before offering playback continuation. Define completed-content behavior rather than automatically seeking into the final seconds.
- Cover app/server restart, endpoint change, task retention expiry, cache eviction, completed content and position restoration. Later media/offline slices extend these cases under faults.

## Validation And Delivery Rules
- The lead owns architecture, integration, journal updates and Git/GitHub mutations. Bounded implementation and test execution use GPT-6 Luna workers with disjoint write scopes.
- Require complete relevant tests, lint/format, current-head GitHub CI and GitHub `@codex review`, plus resolution of every conversation. Do not run local formal review lanes.
- Merge only with applicable authorization and genuine required human approval. Never impersonate an approver, bypass protection, change authentication or rerun Actions implicitly.
- Freeze signed commits before review. After each merge, update master before creating the next branch. Keep transient review/check state in PR metadata and bounded private receipts.
- Manual/exploratory UI acceptance uses Computer Use; stable workflows become coded XCUITest cases. Verify system mute before playback and leave it muted afterward. Muted playback is not audible-audio acceptance.
- Preserve the existing CI gates while improving their runtime and diagnostics. Cold-run success does not prove a warm-cache benefit, and a green job alone does not prove named UI-case execution.

## Live And Scope Boundaries
- Reuse the locally selected saved credential profile and preserve other profiles/global defaults. Notify the operator before authenticated live work or login; request new credentials only if the needed saved material is missing or rejected.
- Keep Web cookies and all credential storage on the server. Public restricted proxies use Web-mode APIs and may receive only the already-authorized shared generic access key, never Web cookies.
- Target ordinary/episode foreground windows of about 600 seconds and separate 300-second cache-only windows when content permits; report actual duration and full natural completion for shorter content. HTTP readability, native decoding, full app UI and network-loss evidence remain distinct.
- Physical Apple TV validation, TLS/public-facing authentication, cross-device history, 4K/HDR expansion and additional upstream-page schema remain deferred.
- Use existing V2 APIs first. A demonstrated missing contract needs a focused design before any protocol or upstream dependency expansion.

## Current Baseline
- PR #81's reviewed head and squash merge have the same tree `1f955a60a80dbcbd86aa080ab1f39e0cbbadef44`; both platforms executed all three native UI cases with zero failures/skips and validated task metrics.
- Ordinary GitHub CI took 39m44s under a 45-minute job limit. Its raw log exceeded a 16 MiB diagnostic capture ceiling; complete run/job/step success metadata remained available, but remote non-UI totals were not independently extracted.
- One local Rust run timed out in the unchanged two-second asynchronous test waiter; its focused case and one complete normal-parallel rerun passed. This is evidence of an unreproduced timeout, not a proved production defect or resource-contention root cause.
- Prior capabilities and source-versioned acceptance remain in [project state](../../../PROJECT_STATE.md) and the completed workstream journals.
