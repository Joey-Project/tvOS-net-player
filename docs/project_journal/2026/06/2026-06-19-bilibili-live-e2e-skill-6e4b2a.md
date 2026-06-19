---
id: 20260619-6e4b2a
title: Bilibili Live E2E Skill
status: active
created: 2026-06-19
updated: 2026-06-19
branch: wip/bilibili-live-e2e-skill
pr:
supersedes: []
superseded_by:
---

# Bilibili Live E2E Skill

## Summary

- Added a repo-local `.agents/skills/bilibili-live-e2e/` workflow for opt-in real Bilibili playback smoke validation.
- Embedded four canonical live URLs in the skill reference: ordinary video, multi-part video, Bangumi media series, and Bangumi episode.
- Added `just test-bilibili-live` as the stable local entrypoint.

## Current State

- Default live smoke validates the two non-region-restricted video cases through the Rust LAN cache server, progressive playback task, and generated HLS master playlist.
- The Bangumi media and episode cases are recorded and explicitly runnable, but currently fail without BBDown restricted-area runtime configuration.
- `prefer_tv_api` now reaches BBDown core's TV playurl mode instead of being rejected by the adapter.

## Next Steps

- Add server-side BBDown restricted-area configuration and credential/proxy handling.
- Re-run `BILIBILI_LIVE_E2E_CASES=bangumi-media-series,bangumi-episode just test-bilibili-live`.
- After the restricted-area route is available, decide whether default live smoke should include all four cases.

## Evidence

- `uv run --isolated --with pyyaml python3 .../skill-creator/scripts/quick_validate.py .agents/skills/bilibili-live-e2e`: passed.
- `python3 .../project-journal/scripts/project_journal.py validate --repo .../tvOS-net-player`: passed.
- `bash -n scripts/test-bilibili-live.sh`: passed.
- `shellcheck scripts/test-bilibili-live.sh`: passed.
- `cargo fmt --all`: passed.
- `just lint`: passed.
- `just test-cache-server`: passed.
- `scripts/test.sh`: passed, including 138 Swift XCTest cases.
- `scripts/test-macos.sh`: passed, including the `MacOSNetPlayerTests` app-shell integration test.
- `just test-bilibili-live`: passed the default opt-in live smoke entrypoint.
- `scripts/test-bilibili-live.sh`: default run passed `ordinary-video-playlist` and `multi-part-video`, skipped the two restricted-area cases.
- Explicit `bangumi-media-series` run failed with Bilibili area restriction after playback planning.
- Explicit `bangumi-episode` run failed with Bilibili area restriction after playback planning.
