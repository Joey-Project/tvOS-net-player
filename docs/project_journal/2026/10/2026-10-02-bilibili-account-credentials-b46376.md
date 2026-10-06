---
id: 20261002-b46376
title: Bilibili Account Credential Validation
status: completed
created: 2026-10-02
updated: 2026-10-02
branch: master
pr:
supersedes: []
superseded_by:
---

# Bilibili Account Credential Validation

## Summary
- Keep the previous account and the newly authorized account in separate credential profiles.
- Obtain Web cookie, generic access key, and TV access key on the Mac operator; never send their values to clients, chat, or tracked files.
- Reuse healthy credentials and stored refresh material instead of repeatedly requesting QR login.

## Current State
- The previous profile's Web cookie and generic access key passed the typed health probes before this operation.
- The new-account profile now contains its Web cookie, generic access key, and TV access key, including private refresh material for all three kinds.
- The generic key's OAuth account ID matched the new Web cookie account ID before saving. No account IDs or credential values were emitted.
- The TV credential, expiry metadata, and refresh secret were atomically moved together using the core credential store transaction. Source Web/generic and destination Web credentials were preserved.
- Final official probes reported `valid` for Web cookie (`web_cookie`), generic key (`intl_bstar`), and TV key (`tv`) in the new profile.
- The previous profile retains its healthy Web cookie and generic key. Its default selection remains unchanged; subsequent validation must explicitly select the new-account test profile recorded in the local, ignored `AGENTS.override.md`.
- The macOS credential store remains owner-private (`0600`). QR tickets and raw OAuth results were never committed.

## Validation Boundaries
- Credential health is scoped to the exact probe API; it does not prove restricted-region playback, subscription access to a particular title, or sustained HLS decoding.
- Public reverse proxies remain Web-mode routes, not TV API routes. Public-key probing consent does not authorize forwarding a Web cookie.
- The local operator's OAuth polling is not a completed client-facing browser-handoff or automatic renewal feature.

## Next Steps
- Use the selected profile for authenticated and restricted live validation; account permissions must be checked against actual requested content.
- Keep automatic credential renewal and client-facing browser-handoff integration in the parent workstream; this operator login does not complete those product features.

## Evidence
- Parent workstream: `../09/2026-09-30-adaptive-bilibili-playback-roadmap-7e2c1a.md`.
- Task-scoped private operator artifacts: `.codex-tmp/bilibili-credentials-20261002/` (ignored; not part of repository delivery).
- The authoritative credential store is the existing macOS BBDown application-support store, not an in-repository credential fixture.
- Private migration operator: 7 offline tests passed; generic OAuth operator: 7 mocked tests and syntax validation passed. No local formal review was run.
