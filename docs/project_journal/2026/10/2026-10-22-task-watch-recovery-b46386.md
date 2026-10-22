---
id: 20261022-b46386
title: Shared Task Watch Recovery
status: completed
created: 2026-10-22
updated: 2026-10-22
branch: codex/task-watch-recovery
pr:
supersedes: []
superseded_by:
---

# Shared Task Watch Recovery

## Scope
- Follow the merged native UI automation slice, [PR #79](https://github.com/Joey-Project/tvOS-net-player/pull/79), from master `eef6554b511068b07b75e1d4776a332ffb78b7ad`.
- Address the task-stream interruption observed during [macOS Computer Use validation](2026-10-19-macos-computer-use-e2e-b46384.md). The bridged error string did not establish a gRPC wire status or root cause.
- Keep recovery in shared AppCore, with the same behavior on macOS and tvOS. Do not change Bilibili resolution, credentials, background fill policy, or media URLs.
- This entry describes the squash-merge target-branch result. Current-head GitHub CI, remote Codex review and all-conversation resolution remain merge requirements recorded in the delivery PR, not local-test claims.

## Design
- Reconnect observation of the accepted task ID and original endpoint; never create, resolve, resubmit, or cancel a task as a recovery side effect.
- Treat premature clean stream termination as a lost observation channel when the task still requires updates. Use six cancellation-aware delays of 1, 2, 4, 8, 16 and 16 seconds per outage, then expose an explicit observation-only retry action. Manual retry also reconciles with `GetTask` before opening a stream.
- Before resubscribing, fetch the current task under the existing operation timeout and reconcile its output revision and paginated results. The Rust registry atomically registers a subscriber and snapshots committed tasks, so resubscription also covers updates after the unary read.
- Accept updates only for the current task and observation generation. Clearing, replacing, or finishing a task cancels the old stream, pending read, and retry delay. Preserve playable media and active playback while observation is unavailable.
- Duplicate initial snapshots do not replenish the outage retry budget. A genuine accepted state/progress change may establish recovery; successful subscription alone does not prove a healthy stream.
- Classify actual typed gRPC statuses at the transport boundary. Retry transient transport failures within the budget; stop on explicit permission, authentication, unsupported-operation, invalid-request, or missing-task failures. Never derive wire codes from localized strings.
- Show only safe task-control diagnostics. A failed cancel request means cancellation is unconfirmed, not that the server stopped work. A later authoritative terminal update takes precedence over a stale response or error.

## Coverage And Final Local Evidence
- Eleven added model regressions cover retry/backoff, meaningful progress versus repeated heartbeats, premature EOF, explicit retry ordering and completion, cancellation races, deallocation during a suspended read, and ignoring a replaced task's late response. Five typed-error tests cover actual RPC status mapping, retry classification, cancellation and redaction.
- Four credential-free protocol integration cases use an official HTTP/2 Posix server on an ephemeral loopback port, generated method descriptors and typed Protobuf codecs, and the production NIOTS client. They verify recovery and output consistency, no retry on permission denial, explicit NotFound reconciliation without recreation, and clearing during a recovery delay. Test-only dependencies come from existing pinned packages; no production transport injection or server-code change is needed.
- The final unfiltered Swift suite passed **444 tests**, including all four protocol integration cases, with zero failures. macOS hosted XCTest passed **3 tests**, with zero failures or skips. macOS/tvOS builds and tvOS build-for-testing passed; the latter is compilation rather than simulator execution.
- Local tvOS native UI E2E actually passed its named case: one passed, zero failed or skipped, with the expected target count. Local macOS UI E2E failed before runner connection, with no playback assertion execution proved. No retry, unlocking or helper restart was performed; actual GitHub macOS UI execution remains an independent delivery gate.
- Full pre-commit passed shell syntax/ShellCheck, strict Swift formatting, Rust formatting and workspace/all-target Clippy. Before/after fingerprints bind the receipts to unchanged production and final test sources. Every owned validation process exited and was reaped; the private simulator was removed, and verified system output mute remained enabled.
- Early protocol trials first exposed a missing fixture capability, then an abort at XCTest's asynchronous error-observation boundary. Minimal waiter/server/GetServerInfo cases passed, while explicit caller-sleep injection did not remove the abort. The final fixture gates repeated completion notifications once, distinguishes transient reconnect warnings from stopped observation, and asserts the outage snapshot when it is observed. The corrected four cases and the full suite passed without changing production code; no unique compiler or runtime root cause is claimed. Temporary diagnostic methods and phase logging were removed.

## Baseline Evidence
- The unchanged Rust baseline first failed two of 1,173 library tests at the shared two-second asynchronous waiter. Both tests passed once in isolation; one authorized complete serialized run then passed 1,239 tests with one existing opt-in live case ignored. Release build, formatting and strict Clippy passed; 48 tracked Rust/Cargo/script input hashes were unchanged.
- The missing task-state file appeared after timeout unwinding removed a test fixture. An HLS identity rejection was consistent with the same teardown race, but its exact causal timing was not proved. These observations support scheduling sensitivity, not a proven sole cause. The original parallel failure is retained, and default-concurrency GitHub CI remains required.
- Existing Python helper regressions passed 47 tests; the freshly compiled native probe passed 30 self-tests, and both helper scripts passed syntax parsing. No actual playback or internet request was used for these checks. Receipts remain in ignored `.codex-tmp/task-watch-recovery-20261022/`.

## Boundaries
- Protocol recovery coverage is not native UI interaction with the reconnect button, media decoding, live Bilibili/proxy validation, audible-audio validation, or physical Apple TV acceptance. The native UI fixture separately exercises real Rust cache media playback; local macOS runner bootstrap remains unverified.
- Credentials, saved profiles, household services, dependency versions and branch protection were not changed. New logins, automatic task resubmission, silent-stall heartbeats, deployment and physical-device validation are outside this slice.
