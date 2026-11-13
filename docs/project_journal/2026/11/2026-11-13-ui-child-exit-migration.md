---
id: 20261113-ui-child-exit-migration
title: UI Child-Exit Migration
status: completed
created: 2026-11-13
updated: 2026-11-13
branch: codex/ui-child-exit-migration
pr: https://github.com/Joey-Project/tvOS-net-player/pull/104
supersedes: []
superseded_by:
---

# UI Child-Exit Migration

## Summary
- Preserve the owned direct child's unreaped PID as the reservation for its
  original process group while cleanup signals are dispatched.
- The child-exit race fix is isolated to `scripts/ui-e2e.py` with focused
  regression coverage in `scripts/test_ui_e2e.py`; report contracts, assertions,
  deadlines, and playback code are unchanged.

## Current State
- This prerequisite is based on updated master at `fee2d0cc5ba1cf6ae048303209bf63a32bc86f0a`, merged as artifact-control PR #103.
- A fresh `waitid(P_PID, ..., WEXITED | WNOHANG | WNOWAIT)` result for the same
  child preserves its reservation after exit. Identity mismatch requires fresh
  exit proof; external reap, failed revalidation, or a mismatched live group
  refuses signaling. No process-group signal is sent after the final reap.
- The helper SHA-256 is
  `728f7c508264f21837103f62106cac0277abccaa09af56d50d13ca57dd710af8`.

## Next Steps
- Freeze successor controls from this accepted helper release for ordinary
  artifact acceptance, retaining the prior release and migration proof.
- Integrate updated master into PR #102 separately, then refresh its complete
  acceptance against the genuine updated base and head.

## Evidence
- [Artifact verification boundaries](../../../architecture/artifact-verification.md).
- Reference implementation: `160462a26fa6758200c236e0556c3afbe42dd174`.
- `ProcessSupervisorTests`: 12 tests passed on Darwin; journal validation
  passed. Complete delivery evidence belongs to the associated PR.
