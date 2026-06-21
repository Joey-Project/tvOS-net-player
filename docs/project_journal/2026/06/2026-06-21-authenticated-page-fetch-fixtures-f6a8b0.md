---
id: 20260621-f6a8b0
title: Authenticated Page Fetch Live Fixtures
status: completed
created: 2026-06-21
updated: 2026-06-21
branch: wip/authenticated-fetch-fixtures
pr:
supersedes: []
superseded_by:
---

# Authenticated Page Fetch Live Fixtures

## Summary
- PR 4 extends the repo-local Bilibili live e2e skill with authenticated history, watch-later, following feed, and space dynamic feed fixtures.
- Physical Apple TV validation remains out of scope; these cases validate through the local/macOS LAN cache server path and generated LAN HLS sources.

## Current State
- `.agents/skills/bilibili-live-e2e/references/live-cases.json` now includes `authenticated-history`, `authenticated-watch-later`, `authenticated-following-feed`, and `authenticated-space-dynamic`.
- Default `just test-bilibili-live` runs still skip authenticated and restricted-area cases. Authenticated cases run only when selected with `BILIBILI_LIVE_E2E_CASES` or when `BILIBILI_LIVE_E2E_INCLUDE_AUTHENTICATED=1` is set.
- The live harness checks the server-owned BBDown credential status before authenticated cases and requires a loaded web cookie. It does not read or log credential values.
- Authenticated live failures are labeled as `credential`, `empty_account_state`, `upstream_schema_or_availability`, `restricted_proxy`, or `server_bug`.
- `authenticated-space-dynamic` supports `BILIBILI_LIVE_E2E_SPACE_DYNAMIC_URL` so local validation can target a known active uploader dynamic page.

## Next Steps
- PR 5 should add favorites, space videos, collections, series, and recommendations list coverage with deterministic candidate-window tests.
- PR 6 should improve macOS/tvOS fetch UX for login-required inputs, empty account state, and retryable upstream failures.

## Evidence
- Roadmap parent: `docs/project_journal/2026/06/2026-06-21-bbdown-050-hls-roadmap-c9f0a2.md`
- Live e2e skill: `.agents/skills/bilibili-live-e2e/SKILL.md`
