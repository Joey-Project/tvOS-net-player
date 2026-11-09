---
id: 20261109-b46395
title: Stable Local Signing And Executable Paths
status: completed
created: 2026-11-09
updated: 2026-11-09
branch: wip/stable-local-signing
pr:
supersedes: []
superseded_by:
---

# Stable Local Signing And Executable Paths

## Scope
- Deliver the operator's fixed-signing prerequisite independently of native media/ABR changes, preserving that separate workstream.
- Pin an existing Apple Development certificate fingerprint, its subject-OU Team and per-product identifiers in a server-local, untracked profile. Never create/export a private key or modify Keychain authorization.
- Publish the macOS App, Rust server, playback probe and generic Rust library-test executable at stable paths under `~/Library/Application Support/TVOSNetPlayer/LocalBuild`.
- Keep credential-free CI/simulator and isolated XCUITest identities separate. Preserve ordinary Cargo builds on non-macOS hosts.

## Implementation
- The local-build helper verifies the signing identity, strict signature, leaf certificate/Team, identifier, designated requirement and CDHash; publication records each successful target before continuing to the next one.
- Refuse missing/expired configuration, unexpected existing products, drifted signatures and conflicting build sessions instead of falling back to ad-hoc signing or deleting another session's lock.
- Intermediate products remain checkout-specific. Operator execution uses the published stable path; the helper's `run` command holds the build lock through execution.
- A receipt binds each product to its checkout, HEAD and dirty flag. The dirty flag is diagnostic, not a full content attestation of uncommitted files.
- Local build wrappers and UI server preparation use this path by default. Builds do not activate, restart or replace running daily processes.

## Validation
- All 16 focused helper regressions, changed-shell bash syntax/ShellCheck, Justfile parsing and diff whitespace validation passed.
- Complete normal `just ci` passed: 133 Python helper tests, 568 Swift package tests, one tvOS XCTest, three macOS XCTests, 30 probe self-tests and 1,261 Rust tests (1,195 library, 60 harness-unit and six integration tests). The existing opt-in live test remained ignored; no authenticated live request was made. Shell, Swift format and Rust lint, all default builds and the test-bundle build passed.
- The initial 16 MiB capture stopped at its log ceiling and remains incomplete evidence. After quiescence, one identical full run completed with exit zero under a 64 MiB cap. This was an output-budget correction, not a code/test failure recovery or a change to acceptance thresholds.
- Actual local builds published and strictly verified all four pinned products. The probe's locked rebuild/self-test preserved path, Team, leaf certificate, identifier and designated requirement. The signed generic Rust test binary listed 1,195 library tests; enumeration alone is not test execution or native ABR acceptance.
- Those pre-commit signing receipts explicitly recorded a dirty checkout and are not clean exact-head attestations. Delivery additionally verifies the frozen clean source and existing native UI suites; head-bound receipts belong in the associated PR.
- Keep system audio muted for playback-capable tests and afterward. Muted runs do not establish audible audio acceptance.
- Current-head GitHub CI and Codex review remain delivery gates; transient gate state belongs in PR metadata, not this journal.

## Next Steps
- Continue the separate native media/cache API work without weakening its unchanged decoded-frame, automatic downgrade/recovery or transcoding acceptance criteria.
- Migrate the authenticated live integration-test executor to the fixed signed path before later real-network long-play acceptance. No authenticated live validation is part of this slice.
