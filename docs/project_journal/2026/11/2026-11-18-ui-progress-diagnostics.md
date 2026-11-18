---
id: 20261118-ui-progress-diagnostics
title: Bounded UI Process Progress Diagnostics
status: completed
created: 2026-11-18
updated: 2026-11-18
branch: codex/ui-progress-diagnostics
pr: https://github.com/Joey-Project/tvOS-net-player/pull/110
supersedes: []
superseded_by:
---

# Bounded UI Process Progress Diagnostics

## Summary
- Isolate UI harness progress diagnostics from the high-preparation feature.
- Preserve all playback assertions, deadlines and artifact limits; diagnostic
  activity is not test completion or acceptance evidence.

## Current State
- Optional process diagnostics record fixture-build and Xcode phase timing,
  retained-output activity and confirmed child terminal state without private
  command data. Unknown child state remains unknown, not success.
- The single allowlisted ui-progress.jsonl is limited to 32 KiB and 64 records.
  Each phase reserves its own start, up to 30 budget-scaled periodic samples
  and terminal record, so a long build cannot starve the Xcode timeline.
- UI Xcode stdout is no longer quiet; its existing 8 MiB raw-log cap remains.
  Other process callers do not enable the optional observer. Original process
  supervision, ownership, cleanup and absolute deadlines remain unchanged.
- Staging includes the small diagnostic file before attempting a missing or
  partial result bundle, inside the unchanged aggregate and bundle limits.
- Keep native3, five named UI cases per platform and four task-fixture scenarios
  unchanged. Missing or partial Xcode results remain a failed/unknown outcome.
- Use the accepted independent control release c30f67d through its explicit
  single-helper migration route, not the candidate helper as its own validator.
  The unchanged report contract is native3-ui5-fixture4; only ui-e2e.py migrates.
- Preserve the immutable C manifest pin
  29bb6429c3f16286be99f77056058b4e66bcffd7031924ca81cbfef5fc8b68e0.
- Integrate the separately accepted native-list and precise-resumption
  observation baseline from [PR111](https://github.com/Joey-Project/tvOS-net-player/pull/111).
  This diagnostic slice does not change those UI behaviors or their assertions.

## Evidence
- [PR108 UI run44312746210](https://github.com/Joey-Project/tvOS-net-player/actions/runs/44312746210)
  on fdfc253 failed before a valid UI result. tvOS exceeded its 900-second
  fixture-build deadline with final observed compile progress 906/910. macOS
  completed that fixture build in 454.48 seconds but later exceeded the Xcode
  effective work deadline; its partial result lacked Info.plist.
- Existing logs do not identify a specific failing UI assertion or establish
  the underlying slow/stalled operation. The two phases must not be conflated.
- Deterministic regressions cover both full effective phase budgets, early/late
  sampling, terminal quota reservation, privacy, byte/record caps, spawn and
  cleanup failures, hardlink rejection, descriptor cleanup and result staging.
  Actual process microtests preserve nonzero, deadline and log-limit outcomes;
  none is a claim of UI playback or a repaired historical timeout.
- [The original PR110 UI run](https://github.com/Joey-Project/tvOS-net-player/actions/runs/44332592866)
  used these diagnostics to distinguish completed Xcode assertion failures
  from timeouts: both platforms ran five cases, with the resumption case
  failing. The independent PR111 repair passed all five cases per platform
  without changing helper limits or playback precision.
- [Independent artifact verification](../../../architecture/artifact-verification.md)
  defines the unchanged-helper and explicit helper-migration routes. Complete
  delivery/test/review evidence is retained by the associated prerequisite PR.

## Next Steps
- Independently validate the exact helper migration before freezing successor
  controls and integrating the released prerequisite into PR108.
- Preserve PR108's separate signed-media and effective-preparation acceptance.
