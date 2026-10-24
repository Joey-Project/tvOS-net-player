---
id: 20261023-b46391
title: Client Task And Playback Resumption
status: completed
created: 2026-10-23
updated: 2026-10-24
branch: codex/client-task-playback-resumption
pr: https://github.com/Joey-Project/tvOS-net-player/pull/84
supersedes: []
superseded_by:
---

# Client Task And Playback Resumption

## Approved Scope
- Deliver slice 2 of the [home LAN readiness roadmap](2026-10-23-home-lan-readiness-roadmap-b46388.md) after [PR #83](https://github.com/Joey-Project/tvOS-net-player/pull/83), merged as `8006e2145a11bc170c70a27cda2bc9d052083505`.
- Local master was fast-forwarded to that verified squash commit before creating this branch. Share the behavior in AppCore and keep both native clients aligned.
- Keep one device-local latest accepted task and optional playback checkpoint. Cross-device history, server protocol changes and implicit task creation are outside this slice.

## Identity And Persistence
- Bind the record to the canonical control endpoint, configured server ID, original task ID and exact optional child-result/library/variant identity. The configured server ID is a label, not an authenticated globally unique instance ID; the endpoint and original task/result checks remain necessary.
- Use a bounded, versioned, explicit-field preference record. Reject malformed, unsupported or non-finite data. Do not serialize full task objects, credentials, resolution tokens, source links or temporary playback/media URLs.
- Replacing the accepted task or selected playback identity invalidates old checkpoints. Late player/RPC callbacks cannot overwrite a newer task or result. Preserve the existing manual-stream URL preference separately.
- Save valid playback progress locally before awaiting server reporting, including periodic, pause, stop, end and scene-lifecycle checkpoints. An unavailable server must not prevent local persistence.
- Ordinary app-owned pauses save immediately, without waiting for the next heartbeat or a successful reporting RPC. Native pause observation uses the same current-controller/item and verified-position guards; preserve natural-completion state.
- Abrupt termination can restore only the most recently saved checkpoint, not the last decoded frame. Preference persistence is not a synchronous crash-proof transaction or a cross-device database.

## Restore And Continue
- Reattach through existing bounded `GetServerInfo`, `GetTask` and V2 result-page operations. Never resolve the input again, submit another task or substitute the first result for the saved one.
- Treat the wire protocol's `succeeded` state as successful completion alongside legacy `completed` names, consistently for tasks and embedded results. Real gRPC transport coverage must exercise that mapping rather than relying only on legacy-string fakes.
- A changed endpoint/server label, missing task, unavailable original result or deleted selected media has an explicit unavailable/retry/forget presentation. Compare the trimmed live server ID before reading the saved task both during restoration and playback revalidation. Keep the saved identity for retry until explicitly forgotten or superseded.
- When no separate server-address preference exists, the saved original control endpoint may seed the initial address field. Never overwrite a different existing server selection to make restoration succeed.
- Locate a child result within the existing maximum selection window of 100 results, with stable page snapshot and output revision. Reject incoherent pagination rather than guessing.
- Revalidate the selected source using fresh server responses. For retained library media, request its exact library/variant source; progressive playback must still belong to the original task/result and current advertised media origins.
- An accepted task update that changes or removes the selected playable source immediately revokes cached Continue readiness and requires fresh validation. Ordinary progress-only snapshots and device-local checkpoint updates keep the same selected-source validation without repeated polling; explicit continuation still captures the latest checkpoint.
- Keep in-flight source validation bound to immutable task/playback identity rather than mutable position, duration or completion fields. A successful validation returns the latest matching checkpoint. Canceling validation releases only that operation's restoring state; late replies and cleanup cannot affect a newer submission or restoration.
- Preserve the embedded result message when no output summary exists. A failed offline cache fill may retain a playable source under the existing marker-based policy; unrelated failed results remain unavailable.
- A progressive checkpoint without a library ID can resume after that same task/result is finalized into a library item. Validate the original result first, then use its fresh source; an already recorded nonempty library/variant identity cannot silently switch to another output.
- Startup may restore observation but never starts playback. Offer an explicit Continue action only after the original playable output is validated; validate again on activation.
- Returning from an inactive/background scene revalidates the original saved source without playback or task creation. Only arm this refresh after leaving an active scene, so initial activation does not duplicate startup restoration; coalesce in-flight validation and revoke stale readiness while checking.
- Seek after the new player item becomes ready, fencing readiness and seek callbacks against item replacement and manual interaction. Do not overwrite a saved position with the initial zero while restoration is pending.
- Growing progressive HLS may have an indefinite duration. Use a bounded finite saved seek target without inventing a complete duration, and verify the actual seek position before autoplay; a clamped or failed seek must not be reported as successful restoration.
- Failed or timed-out readiness/seek releases that resumed player without reporting false progress or overwriting the saved checkpoint. Disable its playback controls and retain a retryable error instead of leaving an untracked playable item.
- URL acceptance is not completed restoration. Await an item-owned final resumption result before marking the selected task active; failed, canceled or superseded attempts cannot leave false Playing state or clear a newer task's association.
- An explicit manual play/seek that takes over a pending restoration cancels the old saved seek and initializes normal checkpoint/reporting behavior for that same player item. Late restoration callbacks cannot override it. Editing input, replacing a source or stopping alone must not overwrite the saved position with initial zero.
- Keep native player surfaces unmounted while the saved position is still unverified. The player prepares and seeks offscreen; app-owned transport controls remain available for explicit takeover. This prevents native paused-seek controls from racing restoration without guessing the origin of the restoration's own time-jump notifications.
- A completed checkpoint restarts from zero. An unfinished checkpoint is bounded against the fresh duration; do not silently treat ordinary near-end pauses as completed content.

## Validation And Delivery
- Add deterministic storage/player and task-recovery tests covering restart, endpoint replacement, retention expiry, result identity, eviction, stale callbacks and completed content.
- Extend both native XCUITest suites with a real terminate/relaunch/explicit-continue case. Keep isolated UI preferences only on an explicit test relaunch; preserve all existing cases and strict fixture metrics proving one original task creation and no recovery resubmission.
- Keep resumption accessibility identifiers on individual controls rather than their container, so Continue, Retry and Forget remain independently discoverable. Native UI setup must explicitly establish foreground state and use the production address-submission/focus path before navigation; keep visibility, enabled-state and exact-source assertions. On tvOS, move upward from Submit to the preceding result list and verify the exact selected result is focused and hittable before playback. Use a left-pane landmark before moving right to the player's transport controls, preserving the normal remote-focus path between columns. Enter the saved-task action row before moving left to Continue, within the same total direction-key budget.
- Require complete local tests/lint/format/builds, actual current-head GitHub ordinary/native UI CI, GitHub Codex review, every conversation resolved and genuine applicable human approval before merge.
- Use credential-free local fixtures. Set and verify system output mute before playback and leave it muted; muted playback does not validate audible audio.

## Implementation And Focused Validation
- Both apps construct one shared `ClientResumptionStore` for their task and player models. The record uses an explicit versioned schema capped at 16 KiB; value validation remains independent of the main-actor-owned preference writes.
- Shared task restoration handles task-only records, exact paginated child selection, progressive-to-library promotion, fresh media-origin checks and explicit retry/forget. Playback-state association does not depend on the selected child already being visible in the first UI page.
- Explicit continuation waits for readiness and verifies the actual seek position within one second. Known-duration unfinished playback keeps a one-second end margin; natural completion restarts at zero, while an indefinite progressive duration does not become a fabricated finite duration.
- Preserve existing nil-store cache-deletion behavior alongside saved-record invalidation. New test fixtures budget initial reads, watch reconciliation and preparation separately; their blocked-RPC waits use phase baselines, bounded conditions and cleanup rather than indefinite subscriptions.
- Initial focused validation passed 268 Swift cases: 8 storage, 97 player, 149 task model, 5 real transport and 9 fixture cases, with no failures or skips. All 117 Python helper cases and repository pre-commit lint/format passed. These are focused pre-commit results, not a substitute for the complete signed-head delivery gates.
- Current resumption regressions passed four complete affected suites: 8 storage, 129 player, 181 task-model and 6 real-transport cases (324 total, zero failures/skips), plus pre-commit lint/format. They cover live server identity, failed-resume cleanup, explicit manual transport and physical seek cancellation, cached-source invalidation versus local checkpoint changes, embedded offline-fill failure markers and protocol success-state compatibility. Additional regressions cover immediate pause persistence despite reporting failure, in-flight checkpoint updates versus true identity replacement, cancellation during new submission and stale cleanup versus a newer restoration owner. Async completion and foreground activation cases verify both timeout phases, caller cancellation after successful seek, same-item manual takeover, source eviction/replacement, coalesced validation and stale continuations that must preserve newer playback associations.
- The real loopback gRPC regression uses the production client and protobuf `succeeded` states for a two-result task. It saves the second result, rebuilds the store/model, restores and explicitly prepares the exact library/variant without another resolution, task creation or cancellation. It does not decode media or replace native UI acceptance.
- Both native suites contain the fifth relaunch case and retain their original four cases. The fixture includes a decoy result and records exact selected-source requests, one resolution and one task creation. Actual current-head native execution, complete local CI, remote CI/review and conversation resolution remain mandatory before merge; PR-specific receipts stay in PR metadata and ignored local evidence.
