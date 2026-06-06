---
id: 20260606-e5f1a9
title: Bilibili Task Intake
status: completed
created: 2026-06-06
updated: 2026-06-06
branch: bilibili-task-intake
pr:
supersedes: []
superseded_by:
---

# Bilibili Task Intake

## Summary

- Added the first server-side Bilibili task control-plane slice for `TaskService`.
- Introduced an in-memory task registry that accepts Bilibili URL/BV submissions, deduplicates active submissions by normalized source, exposes lookup, streams watch snapshots and updates, and supports idempotent cancellation before a downloader starts.
- Kept media bytes and playback unchanged: gRPC remains control-plane only, and the real BBDown worker is still a follow-up behind the cache server boundary.

## Current State

- `CreateBilibiliTask` returns queued `BILIBILI_DOWNLOAD` tasks instead of `UNIMPLEMENTED`.
- `GetTask`, `WatchTasks`, and `CancelTask` are implemented for in-memory task state.
- Active duplicate submissions return the same task id; once a task is cancelled, the same Bilibili source can be queued again.
- Tasks intentionally remain queued until the next slice adds a worker that calls BBDown or an equivalent Bilibili resolver/downloader.
- The tvOS app does not expose Bilibili task submission UI yet.

## Next Steps

- Add a real BBDown adapter worker on the Mac mini cache server, with bounded concurrency and output normalization into library items.
- Add tvOS UI for submitting Bilibili URLs/BV IDs and watching task progress.
- Decide whether task state should be persisted across cache server restarts before long-running downloads are enabled.

## Evidence

- `scripts/test-cache-server.sh`
- `python3 /Users/joey/.codex/personal-sync/overlays/private/releases/7ead1a9818db266b4d3768514cc817270d9aeaf7/personal_codex/skills/project-journal/scripts/project_journal.py validate --repo /Users/joey/Program/Codex-workspace/tvOS-net-player`
- `just ci`
