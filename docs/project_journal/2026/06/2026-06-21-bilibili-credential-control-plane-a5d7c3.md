---
id: 20260621-a5d7c3
title: Bilibili Credential Control Plane
status: completed
created: 2026-06-21
updated: 2026-06-21
branch: wip/bilibili-credential-control-plane
pr:
supersedes: []
superseded_by:
---

# Bilibili Credential Control Plane

## Summary
- PR 3 adds a server-owned BBDown credential readiness RPC for macOS/tvOS diagnostics without exposing credentials to clients.
- Physical Apple TV validation remains out of scope; validation continues through local macOS app/build paths and the repo-owned live e2e suite.

## Current State
- `ServerService.GetBilibiliCredentialStatus` reports `notConfigured`, `ready`, `degraded`, or `error` based on server runtime configuration and the current credential file state.
- The response exposes only coarse booleans, restricted-area label, proxy counts, and check time; it does not serialize the credential path, cookie/access key values, proxy URLs, or parse error details.
- `ServerInfo` advertises `SERVER_CAPABILITY_BILIBILI_CREDENTIAL_STATUS`, and the Swift cache client exposes `getBilibiliCredentialStatus()` plus `supportsBilibiliCredentialStatus`.

## Next Steps
- PR 4 should add authenticated live fixtures for history, watch-later, and following/dynamic fetch coverage.
- Later macOS UX work can surface this status in diagnostics before running authenticated/restricted Bilibili validation.

## Evidence
- Roadmap parent: `docs/project_journal/2026/06/2026-06-21-bbdown-050-hls-roadmap-c9f0a2.md`
- Architecture note: `docs/architecture/cache-server.md`
