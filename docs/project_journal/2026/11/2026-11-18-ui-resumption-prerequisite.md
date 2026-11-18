---
id: 20261118-ui-resumption-prerequisite
title: Native Result Navigation And Precise Resumption
status: completed
created: 2026-11-18
updated: 2026-11-18
branch: codex/ui-resumption-fix
pr: https://github.com/Joey-Project/tvOS-net-player/pull/111
supersedes: []
superseded_by:
---

# Native Result Navigation And Precise Resumption

## Summary
- Isolated Swift/UI result navigation and precise relaunch resumption from the
  independent UI-helper migration and high-preparation feature.
- Preserved native playback controls, exact result identity and explicit Continue.

## Design
- Extract relevant native tvOS results/cache/task lists and focus navigation
  from the existing high-preparation branch without importing unrelated media,
  Rust, EOF-test or helper changes.
- The visible cache-library cover owns its deletion confirmation, including
  cancellation. The results cover displays the actual current Core playback
  state, so no-autoplay checks do not query a hidden underlying player. The
  existing read-only cache fixture verifies deletion remains disabled; positive
  confirmation/cancellation runtime coverage needs a separate opt-in fixture.
- Preserve the existing zero-tolerance resume seek and its one-second actual
  position verification. Publish a read-only, operation/identity-bound sample
  of that actual position before autoplay; a requested target is not an applied
  seek. Both clients expose the observation as structured accessibility data
  only in isolated UI-test mode, without changing production capture or
  playback. Tests verify precision plus playing inside the original 30-second
  window, then separately verify real progress.
- Isolated UI diagnostics use a native text leaf, like the working playback
  timeline, rather than a transparent one-pixel accessibility carrier. Native
  focus sections preserve access to available actions when adjacent actions
  are disabled; tests do not force focus or change playback behavior.
- Keep no-autoplay, selected-vs-decoy, failure/cancellation and stale-operation
  protections. Do not widen position tolerance, budgets or required case sets.
- Both producer helpers remain outside this candidate's diff. Independent old
  release C retains native3, five named UI cases per platform and fixture4.

## Evidence
- [PR110 UI run 44332592866](https://github.com/Joey-Project/tvOS-net-player/actions/runs/44332592866)
  completed each platform with four passes and one resumption failure. Both
  Xcode children exited 65 before time/log budgets; this is not timeout evidence.
- tvOS could not focus the decoy result Play after relaunch. macOS first paused
  checkpoint was about 8.4 seconds while the later Continue failure snapshot
  showed 38.7/92.0. This moving-timeline observation does not prove whether the
  original seek was correct. The stable sample removes that evidence gap
  without changing seek policy, playback behavior or acceptance precision.
- Focused `PlayerViewModelTests` passed all 136 cases with zero failures on
  Swift 6.3.3. Core, test and dependency-lock hashes were unchanged; system mute
  was verified before and after. This is not hosted UI or media acceptance.
- Complete ordinary `just ci` passed: 269 Python helper tests, 9 tvOS simulator
  tests, 750 Swift package tests, 3 hosted macOS tests, 67 LAN harness cases and
  1,451 Rust tests (5 ignored). Configured lint and all four build stages passed.
  The frozen code snapshot remained unchanged. Final journal-only edits are
  validated separately; current-head hosted UI/media acceptance remains a PR
  merge gate rather than a claim supplied by ordinary local CI.

## Next Steps
- Integrate this accepted UI slice into the separate helper-migration
  prerequisite and refresh its independent acceptance, then return to
  high-preparation validation. No previous local media allowance is renewed.
