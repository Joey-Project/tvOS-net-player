---
id: 20260827-d8c4f1
title: Task Output And Bilibili Schema V2 Roadmap
status: active
created: 2026-08-27
updated: 2026-08-28
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
- PR 6B implements ordered generic output state in the Rust task registry, derives compact summaries for legacy and v2 reads, and migrates disk snapshots from schema v1 to a fail-closed v2 format that persists results, problems, artifacts, public resource metadata, revisions, snapshot IDs, and primary-result identity.
- `ListTaskResults` now serves bounded immutable snapshots through random, task-bound continuation tokens. Existing snapshots remain internally consistent across later output revisions, while malformed, cross-task, expired, and evicted tokens fail explicitly.
- Available artifacts can be served with GET, HEAD, and single-range requests from the fixed `.tvos-net-player/resources/<opaque-id>/body` namespace. Resource paths stay server-only, secure no-follow opening prevents symlink traversal, declared sizes are checked, and client URIs are projected through the configured LAN or public media base.
- The Rust server advertises `SERVER_CAPABILITY_TASK_OUTPUT_V2` only when durable task state and secure HTTP Range serving are available. Legacy task result fields remain populated for older clients.
- PR 6B review hardening makes pagination state process-wide across dual-stack listeners, dynamically drops v2 after post-start persistence failures, keeps legacy progress from creating rollback-prone revisions, and synchronizes cache deletion with authoritative result summaries and artifacts.
- Resource IDs are canonicalized before filesystem use and cannot be rebound to different live representations. Immutable page snapshots retain their resources, conditional HTTP requests implement validator and `If-Range` semantics, durable retirement removes old bodies, and startup no-follow scanning closes the metadata-commit/resource-delete crash window.
- PR 6B final hardening serializes authoritative output publication with durable snapshot replacement, fsyncs the task-state parent directory, and defers startup orphan cleanup until the sanitized state rewrite is durable. Failed writes remain gated and an identical retry can recover the installed output safely.
- Pagination now owns unique resource lease IDs, so duplicate first-page requests and snapshot-ID reuse cannot release a newer lease. Existing continuation tokens remain valid after terminal task metadata is pruned, while expired or evicted page snapshots release only their own resource lease.
- Task-state persistence health is independent from generic resource-storage health: HLS recovery can continue when the v2 resource namespace is unavailable, while the `TASK_OUTPUT_V2` capability still requires both. Resource `HEAD` ignores `Range`, repeated `If-None-Match` fields support quoted commas, and configured public media bases are canonicalized before URI projection.
- The final independent-review follow-up stages durable task events until their complete registry snapshot is saved, then publishes each task's latest pending event exactly once. A failed generic-output write can therefore recover through any later successful mutation without exposing a rollback-prone revision or requiring an identical retry.
- Reused first-page snapshots atomically renew their expiry and transfer ownership to the new resource lease. First-page insertion and continuation-token resolution now include page extraction in the same page-store critical section, so concurrent capacity eviction cannot invalidate an already accepted request.
- First-time task-state directory creation records the missing directory chain and fsyncs it from the state directory through the nearest pre-existing ancestor after the atomic state-file rename.
- Durability-required output replacement and cache deletion now use rollback checkpoints. A failed snapshot save restores the previously durable task, output, queue, cancellation, cleanup, and pending-publication view before releasing the registry lock, while persistence generations remain monotonic for recovery. `GetTask`, new subscriptions, resource authorization, and restart therefore cannot observe a rejected authoritative mutation.
- Whole-task cache-deletion tombstones advance the previous output revision and use a fresh snapshot identity, so clients cannot discard a deletion event as stale.
- Task-result pagination uses one reaper per shared page store instead of one sleeper per first-page request. The reaper expires page snapshots and continuation tokens during idle periods and releases exactly their retained resource leases.
- Resource ownership, immutable-representation validation, and cleanup authorization build hash maps or sets once per operation, keeping maximum-sized output updates and cleanup scans linear rather than quadratic.
- Retired task resources remove both the `body` file and their now-empty opaque-ID directory through fd-relative no-follow operations. Startup scans are bounded, unexpected non-empty directories remain reserved for retry, and repeated HTTP `Range` fields are rejected before a partial response is selected.
- PR 6B's final review fixes treat atomic rename as the task-state commit point. A parent-directory sync failure keeps working state, the committed client view, and the installed file aligned while disabling v2 until a fully durable retry; a failure before rename still rejects and rolls back durability-required mutations.
- Persisted mutations now snapshot under the registry mutex, perform bounded JSON serialization/write/fsync outside it, and then install a committed visible view. `GetTask` and new subscriptions continue to expose the prior committed state while a legacy save is pending or rejected, and existing pending watch events publish only after recovery.
- Generic output is bounded by result, artifact, resource, aggregate-string, per-message encoded, and total encoded-byte budgets. The process-wide immutable page store also evicts by encoded bytes, and task-state JSON is bounded to 128 MiB on read and write.
- Completed HLS deletion and quota eviction persist task deletion metadata before removing cache directories. Physical cleanup is idempotent, retries can remove an already-unowned cache item, and startup removes sessions that no longer have an authorized persisted task.
- Cancelling a playable progressive task now commits the terminal task/output state before its RPC removes HLS bytes. A rejected state snapshot rolls the cancellation back, returns `Unavailable`, and preserves the playable task and cache session for a safe retry.
- HLS playback authorization and cache-item lookup use the same last-committed task view as `GetTask` and reconnecting watchers. Cancellation first retries a previously rejected snapshot, so hidden terminal work cannot revoke or delete media before it becomes committed.
- New task creation now uses the same commit boundary: with persistence configured, rejected download and playback creation rolls back queue/deduplication/planning state, returns `Unavailable`, and starts no worker or planner. Repairing persistence allows the identical request to create a fresh durable task.
- Result and output limits now reserve the worst-case 2 KiB server media base plus protobuf nesting and credential-redaction growth for every client-visible resource reference. Each result is rechecked after URI projection, and the page store reserves protobuf field framing and response metadata inside its 4 MiB projected-byte budget even when the requested item count is larger.
- Legacy-managed result projection is validated before snapshot construction, and the one trailing JSON newline is included inside the 128 MiB writer bound. Oversized staged legacy state therefore cannot install a snapshot that the loader would reject.
- Resource v2 remains gated until startup's bounded orphan scan succeeds. If the sanitized startup rewrite fails, a later durable mutation reruns orphan cleanup synchronously before re-enabling v2 and opaque resource-ID reuse.
- Grouped HLS deletion keeps failed child session IDs as a process-local cleanup intent after the task tombstone commits. Repeating the same library-item deletion completes those remaining sessions; restart cleanup covers a crash between metadata and byte deletion.
- Final PR 6B review hardening retains the complete parent-directory fsync chain across retries, marks projection-validation failures unhealthy, and restores resource-serving capability after a successful cleanup retry.
- Committed task state now controls HLS quota protection while a newer snapshot is pending or rejected. Quota eviction honors a refused task mutation, and explicit deletion serializes with finalization so a newly completed manifest cannot be removed before task completion and runtime registration commit together.
- Committed task outputs are shared immutable views with precomputed resource authorization. Repeated first-page requests copy only the requested page, while resource GET authorization and the no-follow file open are one cleanup-serialized operation whose descriptor survives later unlink.
- Persisted collection limits are enforced while JSON arrays are decoded, public media bases are checked after canonical percent encoding, and task-output v2 drops immediately when an in-memory snapshot cannot satisfy its durable contract.
- Rejected authoritative output commits reserve and remove newly staged resource bodies after rolling metadata back. Startup distinguishes an absent internal namespace from a missing cache root, rejects noncanonical resource-directory aliases, and keeps resource authorization plus no-follow open work off Tokio's async workers.
- The second PR 6B review follow-up keeps resource IDs reserved when a configured root or intermediate namespace is temporarily unavailable, reconstructs directory-fsync debt after restart, rechecks quota cancellation under the deletion lock, retries fills whose `PLAYABLE` publication is pending, captures repaired HLS session IDs during cancellation, and prevents proxy caching of resource `404` responses.
- The third PR 6B independent-review follow-up makes cache-deletion output mutation atomic and revalidated, preserves an existing primary result when a secondary cache item is removed, requires durable acknowledgement for cancellation and terminal completion, and keeps both download workers and HLS finalizers pending until persistence recovers. Invalid producer output now cleans staged resource bodies, live and persisted resource accounting share a 50,000-record global ceiling with a 100,000-directory startup scan window, quota cleanup retries retained physical-deletion failures before watermark short-circuiting, and task-resource opens are capped at 32 blocking jobs with fail-fast `503` backpressure.
- The fourth PR 6B independent-review follow-up requires parent-directory durability before task metadata can authorize physical HLS deletion, rolls destructive mutations back to their pre-change visible state while that durability is pending, keeps undeletable cleanup sessions protected without blocking independent quota eviction, and applies the snapshot-wide 50,000-resource budget during JSON decoding rather than after full materialization.
- The fifth PR 6B independent-review follow-up retains an async retry owner for failed and cancelled playback planning, deletes planned HLS sessions only after terminal task state is durable, requeues cache-fill failure publication until persistence recovers, and preserves restored-session media until its failure state commits durably.
- The sixth PR 6B independent-review follow-up keeps malformed configured snapshots fail-closed for HLS deletion, adopts every unowned staged resource on rejected output paths, enforces the aggregate artifact budget during typed JSON decoding, bounds retained and copied pagination artifacts, retries installed-but-not-durable cache-fill failure markers without repeating media work, and lets bounded read-only traffic recover v2 after resource storage becomes healthy.
- The seventh PR 6B independent-review follow-up keeps download workers alive until installed terminal snapshots are directory-durable, recognizes rejected cache-fill failure markers in working state before repeating upstream work, and makes resource-owned expiry override immutable page leases. Expired artifacts advance to a durable unavailable output revision, lose their projected URI immediately, and release their body plus global resource-budget slot only after the metadata transition commits.
- Existing server/client Bilibili flows continue using the legacy RPCs until the later direct-v2 client slice lands.

## Next Steps

- Implement PR 6C paginated Bilibili resolution sessions and stable candidate selection across pages.
- Keep accepted candidate identities independent from resolution-session expiry and live-list drift.

## Evidence

- Parent roadmap: `docs/project_journal/2026/06/2026-06-24-next-phase-productization-roadmap-a8d2c5.md`
- Existing multi-result schema history: `docs/project_journal/2026/06/2026-06-19-bilibili-task-schema-roadmap-b7e3f1.md`
- Current control-plane schema: `Sources/TVOSNetPlayerCacheClient/Protos/tvos_net_player/v1/cache_control.proto`
- PR 6A compatibility coverage: `Tests/TVOSNetPlayerCacheClientTests/CacheLibraryPaginationTests.swift` and `CacheServer/RustCacheServer/src/grpc_services.rs`
- PR 6B server coverage: `CacheServer/RustCacheServer/src/task_output.rs`, `task_store.rs`, `task_registry.rs`, `grpc_services.rs`, `library.rs`, and `media.rs`; the post-review Rust suite passes 627 unit tests, 34 default live-e2e helper tests with 1 opt-in real-network case ignored, and 6 integration tests.
- PR 6B client compatibility coverage: Swift package tests pass 272 tests, the macOS app test passes its shared-AppCore integration test, and generic tvOS simulator build-for-testing succeeds. Local tvOS simulator test execution remains blocked by the host CoreSimulator `1051.54.0` being older than Xcode's required `1051.55.0`; GitHub CI remains the executable simulator gate.
