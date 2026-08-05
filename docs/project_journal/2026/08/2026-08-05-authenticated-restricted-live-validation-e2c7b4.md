---
id: 20260805-e2c7b4
title: Authenticated And Restricted Live Validation
status: completed
created: 2026-08-05
updated: 2026-08-05
branch: wip/authenticated-restricted-live-validation
pr:
supersedes: []
superseded_by:
---

# Authenticated And Restricted Live Validation

## Summary

- PR 5 makes the opt-in Bilibili live e2e path usable with a named server-owned credential profile while keeping credential values and raw upstream errors out of terminal output.
- It validates authenticated pages, changing collection feeds, restricted Bangumi playback, and completed progressive HLS behavior through the Rust LAN cache server.
- Physical Apple TV validation remains deferred; the macOS app and local LAN cache path remain the validation target.

## Delivered

- Added `BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PROFILE` and mapped it to the server's `Cache:BBDownCredentialProfile` setting.
- Added per-case server isolation, aggregate failure reporting, credential-safe details, and failure classes for credential, empty account state, upstream schema/availability, restricted proxy, and server defects.
- Preserved stable Bilibili collection-item identity when recommendations or other dynamic feeds reorder between selection and task execution, including exact page selection by embedded CID.
- Kept completed HLS playlists upstream-free while allowing already-issued alternate playlist and range URLs to finish safely from in-memory runtime metadata.
- Refreshed the restricted proxy inventory with separately recorded availability timestamps; restricted playback remains web API mode only.

## Live Evidence

- Public ordinary-video, multi-part-video, and space-collection cases passed through server-owned HLS playback.
- The named credential profile passed homepage recommendations, authenticated history, authenticated watch-later, and space videos.
- `bangumi-media-series` and `bangumi-episode` passed with the available web-mode restricted proxy.
- The unavailable proxy fixture failed safely as `restricted_proxy`, without leaking credential or upstream response details.
- Following/dynamic cases remain blocked by a `bbdown-core v0.5.0` compatibility issue: the upstream dynamic payload now returns `module_author.pub_ts` as a numeric string while the dependency requires an integer. This is an external dependency follow-up, not a passing live case.

## Validation

- `cargo fmt --all`
- `just lint`
- `just pre-commit`
- `scripts/test-cache-server.sh`
- `just test`
- `just test-macos`
- `just build-cache-server`
- `just build`
- `just build-macos`
- `just build-for-testing`
- Local tvOS simulator execution is machine-state blocked because CoreSimulator `1051.54.0` is older than the Xcode-required `1051.55.0`; generic tvOS and macOS builds pass, and GitHub CI remains the simulator execution gate.

## Next Steps

- Execute PR 7 for transcoding and ABR policy controls.
- Update `bbdown-core` after its dynamic timestamp parser accepts both integer and numeric-string payloads, then rerun following/dynamic live cases.
- Pause before deferred/non-sequential PR 6 and discuss the task options/result schema v2.

## Evidence

- Roadmap: `docs/project_journal/2026/06/2026-06-24-next-phase-productization-roadmap-a8d2c5.md`
- Live e2e skill: `.agents/skills/bilibili-live-e2e/SKILL.md`
- Cache-server architecture: `docs/architecture/cache-server.md`
