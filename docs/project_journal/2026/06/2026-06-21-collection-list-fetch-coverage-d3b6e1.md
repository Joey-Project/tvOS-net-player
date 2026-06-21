---
id: 20260621-d3b6e1
title: Bilibili Collection/List Fetch Coverage
status: completed
created: 2026-06-21
updated: 2026-06-21
branch: wip/bilibili-collection-list-fetch-coverage
pr:
supersedes:
superseded_by:
---

# Bilibili Collection/List Fetch Coverage

## Summary

- Added repo-local Bilibili live e2e fixtures for favorite lists, uploader space videos, uploader collections, uploader series, and homepage recommendations.
- Kept these collection/list fixtures explicit opt-in because live probes showed upstream availability, account state, sample freshness, and cookie requirements vary more than the ordinary public video fixtures.
- Added live harness assertions that collection/list candidates use bounded stable `item:` selection ids and that generated HLS URLs stay on the LAN media listener.
- Expanded deterministic Rust adapter coverage for all list-like BBDown inputs: favorite, space videos, collection, series, space collection, space series, recommendation, following, space dynamic, history, and watch-later.

## Validation

- `cargo fmt --all`
- `cargo test --package tvos-net-player-cache-server --lib --locked`
- `cargo test --package tvos-net-player-cache-server --test bilibili_live_e2e --locked`
- `just test-bilibili-live`
- `BILIBILI_LIVE_E2E_CASES=space-collection just test-bilibili-live`

## Live Notes

- Default `just test-bilibili-live` now skips collection/list fixtures unless `BILIBILI_LIVE_E2E_INCLUDE_COLLECTION_LIST=1` or an explicit `BILIBILI_LIVE_E2E_CASES` filter is supplied. The collection/list include flag admits eligible unauthenticated samples into a broader local sweep, but that sweep can still hit upstream availability or rate-limit errors; authenticated list/feed cases still require `BILIBILI_LIVE_E2E_INCLUDE_AUTHENTICATED=1`, and stale sample shapes marked `requires_live_sample_override` need a URL override before joining the unfiltered sweep.
- `space-collection` passed live validation with the committed sample URL and now uses the resolved stable `item:` ids when creating the playback task.
- `space-videos` and `homepage-recommendations` currently return Bilibili `-101` without a web cookie, so they are documented as cookie-backed local validation cases.
- The committed favorite sample currently fails upstream selected-item resolution, and the committed series sample can time out while preparing playback; both remain useful schema fixtures, but stable smoke validation should provide `BILIBILI_LIVE_E2E_FAVORITE_URL` / `BILIBILI_LIVE_E2E_SERIES_URL` overrides.

## Follow-Up

- PR 6 should use these explicit collection/list fixtures to improve UX around login-required feeds, empty lists, truncated candidate windows, and retryable upstream failures.
