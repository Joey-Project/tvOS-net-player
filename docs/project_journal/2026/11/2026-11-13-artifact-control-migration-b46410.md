---
id: 20261113-b46410
title: Independent Artifact Control Migration
status: active
created: 2026-11-13
updated: 2026-11-13
branch: codex/artifact-control-migration
pr: https://github.com/Joey-Project/tvOS-net-player/pull/103
supersedes: []
superseded_by:
---

# Independent Artifact Control Migration

## Decision
- Separate artifact-verification controls from the candidate checkout before
  migrating the UI process helper. Passing CI and remote code review do not
  replace strict artifact acceptance. A helper-changing candidate must not
  import its own helper code to establish its artifact verdict.
- Deliver three ordered stages: independent verification tooling, a focused UI
  helper migration, then refreshed acceptance of the freshness/withdrawal work
  in PR #102. Keep production playback, buffering and scheduling unchanged.
- Use the prior accepted tooling to gate this tooling prerequisite. Only after
  normal merge and a pinned release may its new interface gate the helper
  migration. Neither prerequisite may weaken the ordinary unchanged-helper
  guard, hide changes by narrowing BASE, or relax existing acceptance cases.

## Scope
- Explicitly separate operator-assigned source scope, independently pinned
  executable controls, immutable raw artifact input and fresh derived output.
- Bind producer-source identity and validator identity independently. Include
  transitive media-to-UI helper imports in the control dependency closure.
- Preserve native3, macOS5, tvOS5 and the automatic/manual/silent/resumption
  fixture assertions, exact artifact/run identities, deadlines and byte caps.
- Keep existing UI/media helpers, workflow definitions, signing requirements,
  system-mute verification and production Swift/Rust code unchanged.

## Validation
- Recover the accepted PR #101 adapter and its unchanged helpers. A fresh
  offline replay produced exactly the original proof SHA-256
  `5f2fee91a35bbc0dbda957eb5fe8dde43f77205a1b00b93330af2ce6ed6256e9`.
  The replay preserved the original inputs, prior proof and executable bytes;
  it establishes a reusable old-policy baseline, not acceptance of new code.
- Cover ordinary/migration route separation, exact source scope, control bundle
  substitution, path escapes, candidate import canaries, malformed evidence,
  missing/skipped cases and failure without accepted proof.
- Require complete local format/lint/tests, fixed-signed muted native acceptance,
  current-head GitHub CI/UI artifact verification under prior controls, GitHub
  Codex review and all conversations resolved before normal protected merge.
- Muted or artifact-only verification is not audible playback, visible
  black-screen, authenticated live Bilibili or physical Apple TV acceptance.

## Next Steps
- Pin the independently accepted tooling release identity and its complete
  executable dependency manifest before using it for helper migration.
- Deliver only the UI helper child-exit race fix and its tests in a new branch
  from updated master, using the released independent verifier.
- After that helper is independently accepted and merged, pin the successor
  control bundle with its accepted helper bytes before ordinary feature gates.
- Integrate updated master into PR #102 with an additive signed merge, freeze
  its new genuine base/head/tree, and repeat all invalidated acceptance gates.
- Resume reader-aware near-low/high/far-low scheduling and the separate complete
  PR #98 OFF/ON comparison only after those prerequisites. No faster-upshift
  result is established by this tooling work.

## Evidence
- [Independent artifact control tooling PR #103](https://github.com/Joey-Project/tvOS-net-player/pull/103).
- [Artifact verification boundaries](../../../architecture/artifact-verification.md).
- [Prior independently accepted reader-binding PR #101](https://github.com/Joey-Project/tvOS-net-player/pull/101).
- [Freshness and withdrawal PR #102](https://github.com/Joey-Project/tvOS-net-player/pull/102).
- [Reader-binding decisions](2026-11-12-hls-reader-binding-b46408.md).
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
