---
id: 20260615-e7c2b5
title: Task Retention Cleanup
status: completed
created: 2026-06-15
updated: 2026-06-15
branch: wip/task-retention-cleanup
pr:
supersedes: []
superseded_by:
---

# Task Retention Cleanup

## Summary

- Add configurable retention limits for persisted server-side task history.
- Keep active task records and completed progressive HLS playback records outside ordinary terminal-history pruning.
- Document the cleanup boundary so future cache eviction can delete HLS cache sessions and their authorization tasks together.

## Current State

- `CacheServerOptions` exposes `Cache:TaskRetentionMaxTerminalTasks` and `Cache:TaskRetentionTerminalAgeDays`.
- Defaults retain up to 200 ordinary terminal task records and ordinary terminal task records up to 30 days old.
- Setting either value to `0` disables that individual limit.
- `AppState` passes the configured retention policy into `BilibiliTaskRegistry` at startup.
- The registry prunes eligible terminal records whenever it rewrites the durable task snapshot, including restart restore rewrites.
- `BilibiliProgressivePlayback` tasks in `COMPLETED` remain retained because they authorize completed HLS cache library items until cache eviction can remove the HLS cache session and virtual item atomically.

## Out Of Scope

- Deleting HLS cache sessions or completed Bilibili HLS virtual library items.
- User-facing cache deletion and weak-network/offline controls.
- Bilibili task options/result schema changes.

## Validation

- `cargo fmt --all`
- `python3 /Users/joey/.codex/personal-sync/overlays/private/releases/bb9b591d6375c3c11482cb4fa99394132419c816/personal_codex/skills/project-journal/scripts/project_journal.py validate --repo /Users/joey/Program/Codex-workspace/tvOS-net-player`
- `cargo test --package tvos-net-player-cache-server --locked retention -- --nocapture`
- `cargo test --package tvos-net-player-cache-server --locked parses_bilibili_worker_and_bbdown_args -- --nocapture`
- `just ci`

## Next Steps

- Continue with PR 5: discovery, cache management, and weak-network UX.
