---
id: 20260619-6e4b2a
title: Bilibili Live E2E Skill
status: completed
created: 2026-06-19
updated: 2026-06-20
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
- The Bangumi media and episode cases are recorded and explicitly runnable. The Rust LAN cache server now accepts BBDown credential file and restricted-area proxy runtime config.
- This workstation now has a local private credential file with valid cookie, access key, and TV access key. The file remains outside the repository.
- Public BiliRoaming reverse proxies cannot be used with BBDown TV playurl mode. The restricted Bangumi live cases now use web-mode restricted API proxy planning.
- Public restricted API proxies can receive the configured access key. The skill now keeps copyable examples on a trusted/private proxy placeholder and treats the public registry as availability evidence, not a default credential-safe endpoint.
- `.agents/skills/bilibili-live-e2e/references/restricted-api-proxies.json` stores the public proxy registry from the BiliRoaming wiki, sorted with the latest known successful playback validation first.
- `prefer_tv_api` now reaches BBDown core's TV playurl mode instead of being rejected by the adapter.

## Next Steps

- Keep the proxy registry fresh by updating `last_checked_at` and `last_available_at` after future real playback probes.
- Decide later whether default live smoke should include all four cases; keep restricted cases opt-in for now because they depend on public proxy availability.

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
- `bbdown auth health --json` with the local private credential file: cookie, access key, and TV access key all valid.
- Public proxy probe from the BiliRoaming wiki list found `bili.lli.cx` usable for web/app playback planning and found `atri.ink` returning HTTP 502 at the time of validation.
- `BILIBILI_LIVE_E2E_RESTRICTED_API_PROXY=hk=https://bili.lli.cx BILIBILI_LIVE_E2E_CASES=bangumi-media-series,bangumi-episode just test-bilibili-live`: passed after switching the restricted Bangumi fixture to web-mode planning.
- The same restricted Bangumi run with `prefer_tv_api=true` failed, confirming public reverse proxies should not be paired with BBDown TV playurl mode.
- Added `Cache:BBDownCredentialPath`, `Cache:BBDownRestrictedArea`, `Cache:BBDownRestrictedAreaProxy`, and `Cache:BBDownRestrictedApiProxy`.
- `cargo test --manifest-path Cargo.toml --package tvos-net-player-cache-server config --locked`: passed.
- `cargo test --manifest-path Cargo.toml --package tvos-net-player-cache-server bbdown_adapter --locked`: passed.
- `cargo test --manifest-path Cargo.toml --package tvos-net-player-cache-server --test bilibili_live_e2e --locked --no-run`: passed.
- `uv run --isolated --with pyyaml python3 .../skill-creator/scripts/quick_validate.py .agents/skills/bilibili-live-e2e`: passed after narrow escalation for uv cache access.
- `python3 .../project-journal/scripts/project_journal.py validate --repo .../tvOS-net-player`: passed.
- `just lint`: passed.
- `just test-cache-server`: passed.
- `just test-bilibili-live`: passed the default live smoke entrypoint after the restricted-area config wiring; restricted-area env vars were unset, so the two Bangumi cases were not run in this pass.
- `git diff --check`: passed.
- Internal Codex review (`isolated_review`, `codex-review` lane): no findings.
