---
id: 20261116-b46414
title: Atomic Artifact Scan Migration
status: completed
created: 2026-11-16
updated: 2026-11-16
branch: codex/artifact-scan-atomic-publication
pr:
supersedes: []
superseded_by:
---

# Atomic Artifact Scan Migration

## Decision
- Isolate the native artifact scanner fix from the HLS preparation work in
  PR #108. Start from updated master without carrying feature or client changes.
- A report writer publishes through a temporary file followed by atomic rename.
  An enumerated temporary child may legitimately disappear before the scanner
  reads its metadata. Do not classify that absence as a non-regular file.
- Protect bounded, no-follow file-type and size observations rather than a
  frozen directory entry set. Ordinary atomic publication and child-entry churn
  are not content corruption; directory timestamp changes are not a verdict.
- Keep unreadable metadata/traversal distinct from an enumerated missing child.
  Reject actual symlinks and special files, retain the 512-entry and aggregate
  byte limits, and preserve the native report contract and playback assertions.

## Implementation And Validation
- Collect no-follow metadata once for each observed entry. Skip a missing
  enumerated child, reject file/directory kind changes and surface traversal
  errors rather than silently treating unreadable directories as empty.
- Deterministic regressions exercise publication between enumeration and
  metadata collection without sleeps or timing-dependent race loops. Pair
  benign disappearance/publication with rejected symlink/special-file
  replacement and unreadable access, plus the existing size and entry bounds.
- The complete helper unittest module passed all 44 cases with no failures or
  skips on Python 3.14.5. Helper/test bytes were unchanged after execution.
- Formal artifact validation uses the independently accepted control release
  `1f6465c95570cff3c652401ad13f77e7ed9b4f65`.
  Declare exactly `scripts/media-network-e2e.py` as the migrating producer;
  bind its old/new hashes and the genuine base/head/tree separately from the
  unchanged old control manifest. Never import candidate helper code to
  decide its own artifact acceptance.
- Preserve `native3-ui5-fixture4`, all test identities, deadlines, report
  assertions, fixed operator signing, system mute and artifact safety limits.
  Unit execution of the candidate helper is test-subject coverage only.

Full delivery gate results belong to the associated PR; focused unit tests
do not establish native playback acceptance or faster HLS upshift.

## Next Steps
- After independent acceptance and normal merge, pin a successor bundle without
  modifying the prior trusted release in place. Integrate updated master into
  PR #108 and continue its tvOS relaunch diagnostics and remaining media gates.

## Evidence
- [PR #108](https://github.com/Joey-Project/tvOS-net-player/pull/108).
- [Native failure run](https://github.com/Joey-Project/tvOS-net-player/actions/runs/43906270976).
- The failed run reported `abr.json.tmp` as non-regular; a deterministic
  enumeration-to-rename experiment reproduces that error, but does not by
  itself prove the historical event ordering or pass the native suite.
- [Artifact verification boundaries](../../../architecture/artifact-verification.md).
- Source thread: `019e7fd0-b463-76f2-a5ab-37d7d77620f4`.
