---
id: 20261023-b46391
title: Client Task And Playback Resumption
status: completed
created: 2026-10-23
updated: 2026-10-24
branch: codex/client-task-playback-resumption
pr:
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
- Abrupt termination can restore only the most recently saved checkpoint, not the last decoded frame. Preference persistence is not a synchronous crash-proof transaction or a cross-device database.

## Restore And Continue
- Reattach through existing bounded `GetServerInfo`, `GetTask` and V2 result-page operations. Never resolve the input again, submit another task or substitute the first result for the saved one.
- A changed endpoint/server label, missing task, unavailable original result or deleted selected media has an explicit unavailable/retry/forget presentation. Keep the saved identity for retry until explicitly forgotten or superseded.
- When no separate server-address preference exists, the saved original control endpoint may seed the initial address field. Never overwrite a different existing server selection to make restoration succeed.
- Locate a child result within the existing maximum selection window of 100 results, with stable page snapshot and output revision. Reject incoherent pagination rather than guessing.
- Revalidate the selected source using fresh server responses. For retained library media, request its exact library/variant source; progressive playback must still belong to the original task/result and current advertised media origins.
- A progressive checkpoint without a library ID can resume after that same task/result is finalized into a library item. Validate the original result first, then use its fresh source; an already recorded nonempty library/variant identity cannot silently switch to another output.
- Startup may restore observation but never starts playback. Offer an explicit Continue action only after the original playable output is validated; validate again on activation.
- Seek after the new player item becomes ready, fencing readiness and seek callbacks against item replacement and manual interaction. Do not overwrite a saved position with the initial zero while restoration is pending.
- Growing progressive HLS may have an indefinite duration. Use a bounded finite saved seek target without inventing a complete duration, and verify the actual seek position before autoplay; a clamped or failed seek must not be reported as successful restoration.
- A completed checkpoint restarts from zero. An unfinished checkpoint is bounded against the fresh duration; do not silently treat ordinary near-end pauses as completed content.

## Validation And Delivery
- Add deterministic storage/player and task-recovery tests covering restart, endpoint replacement, retention expiry, result identity, eviction, stale callbacks and completed content.
- Extend both native XCUITest suites with a real terminate/relaunch/explicit-continue case. Keep isolated UI preferences only on an explicit test relaunch; preserve all existing cases and strict fixture metrics proving one original task creation and no recovery resubmission.
- Require complete local tests/lint/format/builds, actual current-head GitHub ordinary/native UI CI, GitHub Codex review, every conversation resolved and genuine applicable human approval before merge.
- Use credential-free local fixtures. Set and verify system output mute before playback and leave it muted; muted playback does not validate audible audio.

## Implementation And Focused Validation
- Both apps construct one shared `ClientResumptionStore` for their task and player models. The record uses an explicit versioned schema capped at 16 KiB; value validation remains independent of the main-actor-owned preference writes.
- Shared task restoration handles task-only records, exact paginated child selection, progressive-to-library promotion, fresh media-origin checks and explicit retry/forget. Playback-state association does not depend on the selected child already being visible in the first UI page.
- Explicit continuation waits for readiness and verifies the actual seek position within one second. Known-duration unfinished playback keeps a one-second end margin; natural completion restarts at zero, while an indefinite progressive duration does not become a fabricated finite duration.
- Preserve existing nil-store cache-deletion behavior alongside saved-record invalidation. New test fixtures budget initial reads, watch reconciliation and preparation separately; their blocked-RPC waits use phase baselines, bounded conditions and cleanup rather than indefinite subscriptions.
- Focused validation passed 268 Swift cases: 8 storage, 97 player, 149 task model, 5 real transport and 9 fixture cases, with no failures or skips. All 117 Python helper cases and repository pre-commit lint/format passed. These are focused pre-commit results, not a substitute for the complete signed-head delivery gates.
- Both native suites contain the fifth relaunch case and retain their original four cases. The fixture includes a decoy result and records exact selected-source requests, one resolution and one task creation. Actual current-head native execution, complete local CI, remote CI/review and conversation resolution remain mandatory before merge; PR-specific receipts stay in PR metadata and ignored local evidence.
