---
id: 20260621-d1a7b4
title: BBDown Native Progress And Cancellation
status: completed
created: 2026-06-21
updated: 2026-06-21
branch: wip/bbdown-native-progress-cancellation
pr:
supersedes: []
superseded_by:
---

# BBDown Native Progress And Cancellation

## Summary

- Complete-download Bilibili tasks now consume `BBDown-rust` `v0.5.0` native `DownloadProgressEvent` updates.
- Server task progress maps BBDown file-level bytes into existing `progress`, `downloaded_bytes`, `total_bytes`, and `message` fields without changing the gRPC schema.
- Running complete-download cancellation is bridged into `DownloadCancellationToken` so BBDown can roll back partial files through its own cancellation path instead of only having the cache-server adapter drop the future.
- The adapter still owns final local playback muxing with server-side `ffmpeg`; BBDown upstream mux remains disabled for now.

## Decisions

- Keep planning cancellation on the existing cache-server polling helper because BBDown planning APIs do not expose a standalone cancellation token in this release.
- Scope native cancellation to the BBDown download call, where `download_plan_with_archive_decision_with_progress_and_cancellation` provides both progress and cancellation.
- Preserve the existing coarse adapter phase boundaries: planning starts at 2%, BBDown download spans 10% to 80%, server muxing reports 80%, and library indexing reports 95%.
- Use `DownloadReport::summary().total_bytes` for completed byte totals so resumed bytes and freshly written bytes share BBDown's upstream summary semantics.
- Coalesce high-frequency BBDown file progress events by byte/progress delta before publishing them to task watchers, while still publishing file start/completion and plan/entry state transitions.
- While a BBDown entry is active, report only already completed entries' byte snapshot; if no completed entry bytes are known, report `0/0` so persisted task state clears stale totals and clients fall back to adapter phase progress until `EntryCompleted` confirms the active entry's full file universe.
- Cap tvOS/macOS offline-cache percentage labels at the task's overall phase progress so completed-entry byte snapshots cannot make a multi-entry task look nearly complete before the full plan finishes.
- Track multi-entry download progress from completed event count and current-entry byte ratio, because BBDown entry indices are source page/episode/item indices rather than guaranteed contiguous ordinals within a selected plan.
- Cap an incomplete entry's active byte contribution until `EntryCompleted`, because BBDown reports files as they start and an entry can still have unstarted DASH audio, FLV segments, subtitles, danmaku, or cover files.
- Preserve cancellation semantics when the task registry has already requested cancellation and BBDown returns a late non-cancel error before observing the token.
- Give BBDown a bounded grace period to observe download cancellation and roll back partial files, then return `Cancelled` if the core future remains non-responsive so the worker does not hold the archive lock indefinitely.
- Roll back per-file accumulated bytes on BBDown `FileFailed`, because BBDown truncates failed attempts back to the attempt start offset before retrying, cancelling, or failing the plan.
- Do not persist high-frequency progress updates to disk; task lifecycle persistence remains unchanged.

## Validation

- `cargo fmt --all`
- `cargo test --package tvos-net-player-cache-server --locked`
- `python3 /Users/joey/.codex/skills/project-journal/scripts/project_journal.py validate --repo /Users/joey/Program/Codex-workspace/tvOS-net-player`
- `cargo fmt --all -- --check`
- `git diff --check`
- `just ci`
- `just test-bilibili-live`

## Next Steps

- PR 2 should extend the download options schema for audio language, AI subtitle policy, and sidecar/danmaku controls.
- Later UX work can make the richer byte progress more visible in tvOS/macOS if the existing shared progress presentation is not enough.
