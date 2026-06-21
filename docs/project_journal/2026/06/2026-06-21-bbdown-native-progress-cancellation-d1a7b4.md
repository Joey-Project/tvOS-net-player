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
- Report a conservative total equal to downloaded bytes when BBDown has no expected file size, because task progress updates currently treat `total_bytes: None` as "leave the previous total unchanged".
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
