---
id: 20260827-d8c4f1
title: Task Output And Bilibili Schema V2 Roadmap
status: active
created: 2026-08-27
updated: 2026-08-27
branch:
pr:
supersedes: []
superseded_by:
---

# Task Output And Bilibili Schema V2 Roadmap

## Summary

- Deliver the approved task-output and Bilibili schema v2 work as five sequential PRs.
- Extend the existing `tvos_net_player.v1` package additively instead of migrating unrelated services to a new package.
- Promote sidecar files to generic task artifacts so future import, transcoding, subtitle, cover, chapter, and metadata producers can reuse the same resource model.
- Keep gRPC as the control plane and serve artifact/media bytes through server-owned HTTP/HLS/Range URLs.
- Keep the Rust LAN server as the only Bilibili integration point. Clients never receive Bilibili media URLs, request headers, credential paths, or local filesystem paths.

## Decisions

- Generic task output owns reusable pagination, result summaries, per-result progress/problems, artifacts, and resource references. Bilibili-specific identity and execution options remain strongly typed in Bilibili messages.
- Candidate and task-result pagination use opaque server-issued page tokens. Tokens are bound to their source snapshot and request scope; clients must not construct or edit them.
- A Bilibili resolution session owns an immutable ordered candidate snapshot with a bounded lifetime and size. Single/multiple selections submit explicit candidate tokens; range boundaries and all-item selection are evaluated against that snapshot rather than a later live list.
- Creating a task copies the selected stable content identities into durable task state, so expiration of the resolution session cannot change an accepted task.
- New Swift AppCore, tvOS, and macOS Bilibili flows use v2 directly. They do not retry through legacy resolve/create RPCs. A server without the v2 capability produces an explicit upgrade-required state.
- Existing v1 RPCs and legacy primary-result fields remain server-side compatibility surfaces for existing clients while the v2 work lands.
- A task may optionally select a server-owned credential profile by ID. Omitting it keeps the active-profile behavior. Secrets and credential storage locations remain server-only.
- Physical Apple TV validation remains deferred; macOS is the primary interactive validation client for this workstream.

## Contract Boundaries

### Generic Task Output

- Reusable page request/page info messages with bounded page sizes and opaque continuation tokens.
- A compact task output summary suitable for `GetTask` and `WatchTasks` without replaying every result.
- Paginated task results with independent state, progress, byte counts, safe problem classification, playback/library handoff, and artifacts.
- Generic task artifacts for media, subtitles, timed comments, cover images, chapters, and metadata.
- Server-owned resource references containing an opaque ID, LAN URI, content type, size/range metadata, validators, and optional expiry. They never contain a local path or upstream URL.

### Bilibili V2

- Typed source and content identity for video pages, Bangumi episodes, and collection items, including stable `aid`, `bvid`, `cid`, and `epid` fields where available.
- Paginated resolution sessions and candidate listing for lists larger than the current 100-item preview limit.
- Strongly typed playback/download specs, API mode, credential profile selection, subtitle AI policy, danmaku formats, and playback policy.
- BBDown plans/reports mapped into generic task results and artifacts without exposing provider transport details.

## PR Plan

### PR 6A: Generic Task Output Schema

- Add additive protobuf contracts for pagination, task output summaries, task results, safe problems, artifacts, and resource references.
- Define the paginated task-result RPC and capability boundary without advertising an implementation before it is usable.
- Add generated Swift/Rust compatibility tests and document field/default semantics.

### PR 6B: Generic Task Output And Resource Service

- Implement paginated task-result reads with stable ordering and scope-bound continuation tokens.
- Persist generic result, problem, artifact, and resource state with backward-compatible snapshot migration.
- Add the server-owned HTTP resource endpoint with bounded Range support and no path disclosure.
- Populate task output summaries while keeping legacy task result fields available to old clients.

### PR 6C: Paginated Bilibili Resolution

- Add v2 resolve/session/candidate RPCs with bounded snapshot lifetime, size, and page size.
- Persist accepted stable candidate identities into task creation input.
- Support single, multiple, range, and all selection across paginated snapshots without live-list drift.

### PR 6D: Bilibili V2 Execution And Artifacts

- Add strongly typed playback/download specs and optional credential profile selection.
- Map BBDown multi-entry reports, subtitle/danmaku/cover/chapter/metadata outputs, playback sessions, and library handoff into generic results/artifacts.
- Add durable recovery, structured safe failures, cancellation, and partial-success coverage.

### PR 6E: Direct-V2 Swift Clients And Live Validation

- Move shared AppCore, tvOS, and macOS Bilibili workflows directly to v2 with no legacy RPC fallback.
- Add paginated candidate/result UX, artifact presentation/actions, and explicit server-upgrade diagnostics.
- Extend macOS/live e2e coverage for large lists, multi-result tasks, artifacts, authenticated collections, and restricted Bangumi.

## Validation Contract

- Start each PR from the updated repository default branch and use a focused `wip/<topic>` branch.
- Run focused tests plus the full local `just ci` gate for every PR.
- Require GitHub CI, the repository review gate, an independent fresh-context Codex review, and zero unresolved conversations before merge.
- Per the repository-specific override, Claude review is not required for this workstream.
- Merge one PR at a time, update local `master`, and only then branch the next slice.

## Current State

- PR 6A provides the additive generic pagination, task output summary, per-result progress/problem, artifact, resource-reference, and paginated task-result contracts plus generated Swift public models.
- The Rust server deliberately omits `SERVER_CAPABILITY_TASK_OUTPUT_V2` and returns `UNIMPLEMENTED` from `ListTaskResults` until PR 6B provides durable output storage and HTTP resource serving.
- Existing server/client Bilibili flows continue using the legacy RPCs until the later direct-v2 client slice lands.

## Next Steps

- Implement PR 6B task-result snapshot persistence, bounded pagination, generic resource serving, and capability enablement.
- Preserve legacy task result fields for old clients while making the generic v2 state authoritative for new clients.

## Evidence

- Parent roadmap: `docs/project_journal/2026/06/2026-06-24-next-phase-productization-roadmap-a8d2c5.md`
- Existing multi-result schema history: `docs/project_journal/2026/06/2026-06-19-bilibili-task-schema-roadmap-b7e3f1.md`
- Current control-plane schema: `Sources/TVOSNetPlayerCacheClient/Protos/tvos_net_player/v1/cache_control.proto`
- PR 6A compatibility coverage: `Tests/TVOSNetPlayerCacheClientTests/CacheLibraryPaginationTests.swift` and `CacheServer/RustCacheServer/src/grpc_services.rs`
