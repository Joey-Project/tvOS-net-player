---
id: 20261019-b46384
title: macOS Computer Use End-To-End Validation
status: completed
created: 2026-10-19
updated: 2026-10-22
branch: wip/macos-computer-use-e2e
pr:
supersedes: []
superseded_by:
---

# macOS Computer Use End-To-End Validation

## Approved Scope
- Continue the macOS UI/player workstream associated with PR #76 from `d8b95a484cfa7fc5c77e5b765b4e68ff23f8841c`.
- Use Computer Use for exploratory/manual app acceptance. The operator also authorizes repeatable UI flows to become coded XCUITest e2e tests. Keep credentials server-side; preserve saved profiles and global defaults.
- Use isolated app state/cache and loopback control/media only. No deploy, existing-user-service restart, host-network change, physical Apple TV, or expanded TLS scope.
- System output mute was set and verified before playback and remains enabled. Muted playback is not audible-audio validation.

## Current Repair And Source Gates
- Candidate uses native AppKit `AVPlayerView` and an `NSScrollView` hosting the unchanged SwiftUI controls. Scrolling retains/clamps its vertical position and constrains the hosted view to the clip width; player hosting preserves identity across updates and clears on removal.
- Current source gates passed: strict Swift formatting, macOS arm64 build, 3 macOS integration tests (0 failures/skips), and 370 Swift tests. Hosted coverage includes finite/overflowing geometry, 360-to-480-point width changes, updated content, and retained scroll position. These are local gates only; no GitHub CI or review is claimed.
- CUA exposed the full accessibility tree and screenshot; native scrolling moved down and back up while the player stayed visible. Evidence: `.codex-tmp/macos-cua-e2e-20261022/isolation/native-control-scroll/BUILD.md`.

## CUA Observations
- Ordinary cached video played with visible decoded frames. Pause/resume, ten-second skips in both directions, and speed selection were exercised. System/player mute remained set; no audible output was assessed.
- Multipart resolution exposed 12 candidates. Page 2 was selected and submitted; actual LAN HLS playback reached 1:15. Fill eventually reached 47,128,253/47,128,253 bytes.
- The task update stream failed and the UI did not automatically reconnect. The bridged `RPCError error 1` does not establish a wire status or error origin. Eventual fill is not attributed to the intervening Cancel click.
- Dynamic candidate paging automatically grew 50 -> 100 -> 113 while scrolling. The limited-list notice remained; All selection was absent and pagination ended. This does not establish hidden snapshot-token identity.

## Upstream-Blocked Full Replay
- The owned normal server was stopped and reaped; the same private cache/state was restarted under the existing loopback-only process policy. A same-policy loopback HTTP probe returned 404. A direct external TCP probe returned `connectx ... Operation not permitted`; its original untruncated command record was recovered after a result-projection failure without repeating the probe.
- These controls establish the tested local-positive/external-negative paths, not physical host network disconnection or exhaustive network isolation. The policy was applied only to the test server, not global host networking.
- A fresh client listed two completed HLS items and played cached Page 2 from the LAN listener. With speed set to 2x and no seeks, actual frames and timeline progression were observed through 0, 5, 10, 85, 169, 361, 436, 534, 679 and 783 seconds, then natural completion.
- At the end, UI reported `Playback finished`, progress `14:19 of 14:19`, native play/pause off, and a visible final video frame. This is full-content muted native-client playback, not only readiness or HTTP readability.
- The parent stopped the player and normally quit the client through CUA; a separate inventory confirmed no running test players or Activity Monitor. The runtime owner stopped and reaped the isolated server; the PID was absent, both test ports were released and the bounded capture reported `cleanup_uncertain=false`. Final system output mute was verified true.

## Recovery History (Brief)
- Early phases were blocked by a locked Mac, then by player-window access failures. Baseline and no-outer-scroll A/B observations narrowed the triggering change group to the added outer SwiftUI scroll container while native AppKit scrolling worked; the helper's deeper assertion cause remains unproved.
- One explicitly authorized normal `SIGTERM` to the exact helper was performed and is consumed. A replacement helper appeared, but player-window access did not recover. Do not repeat termination or broaden recovery without new direction.
- Earlier playback crashes, inaccessible repaired-app windows, and preparation-only server/cache evidence are historical; they do not supersede current UI observations or prove a deeper framework, helper, credential, or network cause.

## Follow-Up Boundaries
- The UI acceptance run and owned-runtime teardown are complete. Isolated cache, builds and bounded receipts are retained for reproducibility; no production service or credential profile was changed.
- Implement watcher reconnect after stream failure, safe termination/cancellation diagnostics, and a deterministic recovery regression as a separate focused operational slice. Do not infer gRPC wire status from the bridged error string.
- Real narrow-window drag, audible audio, physical Apple TV and restricted Bangumi were not covered by this UI repair acceptance. Restricted proxy configuration was absent; no new login was attempted.
- The completed scope is the macOS repair, hosted-view regressions and actual manual UI acceptance. New UI-test infrastructure, task-watcher recovery, deployment and physical-device validation remain separate work.

## Approved Automated E2E Follow-Up
- Deliver the current macOS repair first, with its existing hosted-view regressions. The operator subsequently confirmed that new automated UI e2e work follows this merge, rather than expanding this repair PR. Shared task-watcher recovery remains another focused follow-up. Merge only after relevant complete tests, current-head GitHub CI/Codex review and resolved conversations; update master before branching for the successor.
- Read-only local preflight found Xcode 26.6 (17F113), with iOS 26.5 and tvOS 26.5 runtimes marked available and no availability error. Runtime inventory is not a simulator boot or executed-test pass; recheck actual execution rather than inheriting the historical CoreSimulator blocker.
- Existing macOS/tvOS XCTest targets exercise models and hosted views, not XCUI application interaction. Add explicit UI-test infrastructure and stable accessibility identifiers rather than relabeling those tests as UI e2e. This repo has no iOS app target; the iOS runtime does not add iOS client coverage.
- The later UI-automation slice should cover app launch, usable controls/candidate layout and local-fixture playback controls. Watcher-recovery coverage should exercise control-stream interruption/recovery, fresh task-state reconciliation, cancellation/terminal boundaries and absence of duplicate task submission. Use shared deterministic tests for state-machine edge cases and UI tests for real visible workflows, including tvOS remote focus/selection.
- Keep repeatable UI tests credential-free using an isolated local control/media fixture with known content and explicit failure injection; no production services or hidden view-model calls may stand in for the UI path. Run stable cases through the repository task runner and CI, retaining bounded failure diagnostics/results.
- Verify actual execution on a GitHub Actions runner in the automation slice, including simulator availability, GUI/permission prerequisites, bounded runtime and diagnostic artifacts. Existing GHA unit-test execution or successful UI-test compilation is not proof that the new UI e2e ran. Do not put saved credentials or public-proxy availability into normal CI gates.
- Real Bilibili login/proxy/CDN and long playback acceptance remain explicit opt-in local live tests, separate from deterministic CI. Continue using Computer Use for exploratory/live acceptance and keep system output muted for every playback run.
