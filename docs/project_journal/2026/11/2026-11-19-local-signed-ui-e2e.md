---
id: 20261119-local-signed-ui-e2e
title: Local-Signed macOS UI E2E
status: completed
created: 2026-11-19
updated: 2026-11-19
branch: codex/local-signed-ui-e2e
pr:
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
- The existing UI test assertions, 35-minute budget, artifact limits, UserDefaults/reset/relaunch behavior and owned loopback fixtures remain unchanged. Build, signature and keychain commands share the work deadline. No live credentials or host security mutations are introduced.
- Local signing support loads only when needed by signed execution; cold validation consumers retain the released four-file control-bundle closure.

## Next Steps
- Continue the separate HLS high-preparation workstream after fixed-identity acceptance. Keep host permissions, lock-state coverage and audible audio acceptance separate from code signing and muted automated tests.

## Evidence
- Accepted implementation base: `9b2`.
- Repository contract: `README.md`, Native UI E2E; `Justfile`, `test-ui-e2e-macos` recipes.
- Regression coverage includes mode selection, server-builder isolation, malformed/expired work budgets, product paths, lock cleanup, receipt and signature drift, previous-worktree reuse, pre-test identity verification and standalone validation imports. Execution results remain in the PR and bounded operator evidence.
