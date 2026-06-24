---
id: 20260621-e4c8d9
title: Bilibili Download Options Schema
status: completed
created: 2026-06-21
updated: 2026-06-24
branch: wip/bilibili-download-options-schema
pr:
supersedes: []
superseded_by:
---

# Bilibili Download Options Schema

## Summary
- PR 2 extends the Bilibili control-plane schema for BBDown `v0.5.0` download options while keeping existing defaults unchanged.
- Physical Apple TV validation remains out of scope; validation continues through local macOS app/build paths and the repo-owned live e2e suite.

## Current State
- `BilibiliDownloadOptions` carries audio language, subtitle AI policy, cover sidecar, and explicit danmaku format controls in addition to the existing quality, codec, TV API, subtitle, and danmaku fields.
- The Rust LAN cache server maps those options into BBDown core `StreamSelection`, `SubtitleAiPolicy`, sidecar toggles, and danmaku formats, with validation for combinations that would otherwise be ignored.
- Task state persistence and active-task dedupe keys include the new option fields with defaults for older snapshots.
- Shared AppCore, macOS, and tvOS controls carry `audioLanguagePreference` for progressive playback and expose a distinct complete-download mode for sidecar/download controls. Progressive playback still submits only stream-selection preferences; complete downloads submit `BilibiliDownloadTaskOptions` with subtitle, danmaku, cover, subtitle AI policy, danmaku format, and audio-language fields.

## Next Steps
- PR 3 should add server-owned credential health/control-plane foundation without exposing secrets to the apps.
- Later progressive HLS PRs should use the carried audio-language field when ABR/audio variant selection becomes variant-aware.

## Evidence
- Roadmap parent: `docs/project_journal/2026/06/2026-06-21-bbdown-050-hls-roadmap-c9f0a2.md`
- Architecture note: `docs/architecture/cache-server.md`
