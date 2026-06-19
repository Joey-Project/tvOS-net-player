---
id: 20260619-b7e3f1
title: Bilibili Task Schema Roadmap
status: active
created: 2026-06-19
updated: 2026-06-19
branch: wip/bilibili-task-schema-foundation
pr:
supersedes:
  - 20260615-c8f4d2
superseded_by:
---

# Bilibili Task Schema Roadmap

## Summary

- Carry the deferred Bilibili task options/result schema work forward as four sequential PRs.
- Keep the LAN cache server as the only client-facing Bilibili integration point; tvOS/macOS clients continue to receive playback sources from the Rust server instead of internet media URLs.
- Preserve the current single-selection resolve/create flow while adding schema and client/server affordances for explicit page, episode, range, all-item, and multi-result task outcomes.
- Continue treating gRPC as the control plane and HTTP/HLS/Range URLs as the media plane.

## Decisions

- The first slice should be a backward-compatible schema foundation: add protocol fields and model shape without changing default execution semantics.
- Server execution should be the second slice, so durable task state and BBDown planning semantics can be reviewed independently from client UI behavior.
- Shared AppCore should consume the new result shape before platform-specific UI work, keeping tvOS and macOS behavior aligned.
- The final slice should expose the UX and extend live e2e coverage once the shared logic is stable.
- Physical Apple TV validation remains deferred until signing, pairing, and local device availability are ready.

## PR Plan

### PR 4A: Schema Foundation

- Add backward-compatible gRPC/proto schema for explicit Bilibili task selection intent.
- Add repeated task result item shape while keeping existing single `library_item_id` behavior intact.
- Regenerate or compile generated Swift/Rust bindings through the existing build paths.
- Add focused compatibility tests and document the schema contract.

### PR 4B: Rust Server Execution

- Persist and recover the new selection/result fields in server-side task state.
- Map explicit selection modes to BBDown Rust planning without exposing internet media URLs to clients.
- Support multi-result task outcomes while preserving the existing primary playback item path.
- Add Rust unit/integration coverage for recovery, stale selections, invalid ranges, and multi-result status transitions.

### PR 4C: Shared AppCore Integration

- Update the shared Swift client and view models to consume the new task result schema.
- Preserve old-server compatibility and direct single-selection fallback behavior.
- Add shared state for multi-result progress, partial success, and library handoff.
- Cover tvOS/macOS shared behavior with Swift tests before adding platform-specific UI.

### PR 4D: tvOS/macOS UX And Live E2E

- Add platform UI for explicit page, episode, range, all-item, and batch result flows.
- Surface multi-result task status, failures, and cached playback handoff consistently on tvOS and macOS.
- Extend repo-local Bilibili live e2e smoke tests for the new schema path.
- Keep restricted-area Bangumi cases opt-in until working proxy/credential validation is available locally.

## Validation Contract

- Each PR must pass the full local gate, including `just ci`.
- Each PR must pass GitHub CI and the required review gate.
- Each PR must complete the three review lanes before merge:
  - GitHub `codex/review-gate` when present or required.
  - `independent-codex-pr-review`.
  - `offline-frozen-diff-review`.
- All actionable PR comments and unresolved conversations must be addressed before merge.
- After each merge, update local `master` from `origin/master` before branching the next PR.

## Deferred

- Physical Apple TV deployment validation.
- LAN-side transcoding policy and UI.
- True segmented HLS output and segment-level prefetch.
- Restricted-area Bangumi live e2e validation until local proxy/credential setup is confirmed.
