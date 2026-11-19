---
id: 20261119-local-signed-ui-e2e
title: Local-Signed macOS UI E2E
status: completed
created: 2026-11-19
updated: 2026-11-19
branch: codex/local-signed-ui-e2e
pr: https://github.com/Joey-Project/tvOS-net-player/pull/112
supersedes: []
superseded_by:
---

# Local-Signed macOS UI E2E

## Summary
- Use a fixed-identity signing mode for local macOS UI E2E while keeping CI and explicit isolated runs credential-free.

## Current State
- Default macOS UI E2E uses the pinned local Apple Development identity outside CI and explicit isolated mode. Missing signing configuration must fail without ad-hoc fallback.
- The App, nested UI-test bundle and runner have fixed dedicated test identifiers under `dev.joey.tvos-net-player.uie2e.local`, separate from production. Their fixed product root is `<durable_root>/UIE2E/DerivedData`; a whole-session lock serializes local test state and builds.
- Local mode builds for testing, verifies each product's configured Team, leaf certificate, identifier, path and designated requirement, then tests without rebuilding. An independent local receipt detects unexpected identity or code-hash changes before later runs. Previous source-worktree paths are audit data, never filesystem authority.
- Local mode only reuses the pinned server's existing verified build receipt; prepare a new server explicitly through the ordinary local-build command when required. Tests do not rewrite the operator receipt or export private keys.
- CI and explicit `LOCAL_BUILD_MODE=isolated` use the existing isolated mode. tvOS remains isolated and rejects local-signed mode.
- The existing five UI cases retain their functional assertions, 35-minute local budget, artifact limits, preference isolation/reset/relaunch persistence semantics and owned loopback fixtures. Build, signature and keychain commands share the work deadline. No live credentials or host security mutations are introduced.
- Local signing support loads only when needed by signed execution; cold validation consumers retain the released four-file control-bundle closure.
- The catalog/playback case owns simulated keyboard address entry and exact text readback before Connect. The four downstream task-watch/resumption cases accept only initial server/source form strings through fresh DEBUG `--ui-testing` process inputs; Connect, Submit, task observation and playback remain real.
- Normal, non-DEBUG and preserve-preferences launches ignore both initial-form inputs. Tests remove the seed environment after initial launch and before relaunch; no task, result, checkpoint, successful state or missing preferences are injected or repaired. Clipboard and host input-method changes are not used.

## Next Steps
- Continue the separate HLS high-preparation workstream after fixed-identity acceptance. Keep host permissions, lock-state coverage and audible audio acceptance separate from code signing and muted automated tests.

## Evidence
- Accepted implementation base: `fb4c269`, after the bounded fixture-budget prerequisite in PR #113.
- Repository contract: `README.md`, Native UI E2E; `Justfile`, `test-ui-e2e-macos` recipes.
- Regression coverage includes mode selection, server-builder isolation, malformed/expired work budgets, product paths, lock cleanup, receipt and signature drift, previous-worktree reuse, pre-test identity verification, standalone validation imports, initial-form guards and absence of constructor-side seed persistence. Execution results remain in the PR and bounded operator evidence; downstream seeding does not establish that host-specific simulated keyboard input is fixed.
