---
id: 20261005-b46380
title: Playback Reliability Follow-Ups
status: active
created: 2026-10-05
updated: 2026-10-05
branch: wip/bangumi-live-validation
pr:
supersedes: []
superseded_by:
---

# Playback Reliability Follow-Ups

## Approved Scope
- Deliver the existing bounded cross-CDN sampling and live-validation work before implementing new runtime capabilities.
- Then add same-identity signed-media URL refresh, credential lifecycle maintenance, and the remaining authenticated page-fetch validation in that order.
- Keep clients on V2 control-plane APIs and server-owned LAN HTTP/HLS sources. Do not expose upstream media URLs or credential values to clients.
- Physical Apple TV validation and TLS remain outside this workstream.

## Delivery Sequence
1. **Sampling and playback regression:** retain the approved small-sample policy, validate all four canonical URLs, and exercise complete fill, same-root restart, cache-only playback, and macOS native decoding/control behavior on the updated implementation. Report HTTP reads and native-player evidence separately.
2. **Signed-media URL refresh:** refresh the accepted content and representation without reselecting a mutable candidate list. Preserve verified extents and use explicit identity checks before resuming background fill. Extend BBDown core only if the exact pinned APIs cannot express the required contract.
3. **Credential lifecycle:** add Mac-authorized Web-cookie renewal/replacement and a trusted origin/session-bound generic access-key login handoff. Reuse healthy saved credentials; do not let an unauthenticated LAN caller overwrite a working profile.
4. **Authenticated page-fetch regression:** rerun following/dynamic with valid saved Web credentials, classify upstream or account-state failures, and fix only demonstrated compatibility gaps.

## Delivery Gates
- GPT-6.1 Sol Max owns architecture, integration, journal updates, and PR/merge decisions; bounded implementation and test tasks use GPT-6 Luna subagents with disjoint write scopes.
- No local formal code-review lanes. Each PR requires passing relevant complete local builds/tests/format/lint, current-head GitHub CI and `@codex review`, and resolution of every actionable PR conversation.
- Keep each runtime slice separate. Merge the preceding PR, update `master`, and create the next branch before implementing the successor slice.
- Follow the repository-supported squash-merge delivery convention and retain exact-head validation/review receipts outside tracked source. Never commit private profiles, QR tickets, signed media URLs, or raw upstream errors.

## Validation Policy
- Reuse the locally selected test profile recorded in ignored `AGENTS.override.md`; preserve the previous account and global default.
- Restricted Bangumi official-route failure is expected. Acceptance uses a working Web-mode reverse proxy; public proxies must not receive Web cookies.
- Prefer foreground windows of at least 600 seconds for the current ordinary/episode full-fill regressions, plus 300 seconds cache-only and a separate native decoding/control probe.
- Every long command has a finite deadline, enforced retained-output ceiling, pollable process handle, and explicit teardown result. Interrupted or unverified runs are incomplete, not passing evidence.
- macOS native AVPlayer probe evidence does not imply that the full macOS app UI or a physical Apple TV was exercised.

## Current Evidence
- The first slice's corrected deterministic and extended live validation passed: 1,088 Rust tests, 343 Swift tests, four canonical URL smokes, and separate ordinary/episode 600-second foreground plus 300-second cache-only and native-control probes. The sampling journal records the earlier reproduced race and the corrected frozen source; successor runtime capabilities remain unimplemented.
- Existing sampling implementation and deterministic regression results: [Bounded CDN Cross-Sampling](2026-10-05-cdn-cross-sampling-b46379.md).
- Previously validated restricted playback, complete fill, recovery, and native controls: [Bilibili Live Playback Validation](2026-10-02-bilibili-live-playback-b46377.md).
- Private account/profile validation: [Bilibili Account Credential Validation](2026-10-02-bilibili-account-credentials-b46376.md).
- Existing resolver/CDN/full-fill foundations and deferred contracts: [Adaptive Playback Roadmap](../09/2026-09-30-adaptive-bilibili-playback-roadmap-7e2c1a.md).
