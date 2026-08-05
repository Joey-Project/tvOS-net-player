---
id: 20260805-e2c7b4
title: Authenticated And Restricted Live Validation
status: completed
created: 2026-08-05
updated: 2026-08-05
branch: wip/authenticated-restricted-live-validation
pr:
supersedes: []
superseded_by:
---

# Authenticated And Restricted Live Validation

## Summary

- PR 5 makes the opt-in Bilibili live e2e path usable with a named server-owned credential profile while keeping credential values and raw upstream errors out of terminal output.
- It validates authenticated pages, changing collection feeds, restricted Bangumi playback, and completed progressive HLS behavior through the Rust LAN cache server.
- Physical Apple TV validation remains deferred; the macOS app and local LAN cache path remain the validation target.

## Delivered

- Added `BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PROFILE` and mapped it to the server's `Cache:BBDownCredentialProfile` setting.
- Added per-case server isolation, aggregate failure reporting, credential-safe details, and failure classes for credential, empty account state, upstream schema/availability, restricted proxy, and server defects. Configuring a credential path, even one that fails to load, makes the server assign a non-sensitive class before replacing raw details with a fixed marker in background-facing task state and client-facing Bilibili task/result/RPC failures. Task RPCs sanitize recovered or legacy failed records again at the client boundary and replace running download message text while preserving numeric progress. The live harness consumes the server-provided class instead of classifying already-redacted wording. Teardown stops and awaits both listeners, enumerates every task in the isolated registry so a lost create response cannot hide work, cancels each task and its queued/current scheduler fills, proves all tasks and registered planning/finalization/transcoding/HLS-fill work are idle, then closes and awaits the per-state HLS worker before dropping that case's cache root and clients. A teardown failure retains the isolated root and persisted task state and reports its path. Current completion plus optional requeue is atomic with scheduler cancellation, covering terminal multi-result parents whose secondary cache fill remains active after the primary cache completes.
- Preserved stable Bilibili collection-item identity when recommendations or other dynamic feeds reorder between selection and task execution, including exact page selection by embedded CID. Stable `item:` ids now bind a canonical parsed collection-source token and reject reuse against another list/feed endpoint.
- Kept completed HLS playlists and persisted manifests upstream-free while allowing already-issued alternate playlist and range URLs to finish from in-memory runtime metadata during one fixed 60-second monotonic grace period. The registry enforces the monotonic deadline on lookup; its monotonic timer applies the generation-bound scrub after sleeping without consulting a rollback-prone wall clock. The generation check prevents either path from overwriting even a byte-for-byte identical newer same-id session.
- Refreshed the restricted proxy inventory with separately recorded availability timestamps; restricted playback remains web API mode only.

## Live Evidence

- Public ordinary-video, multi-part-video, and space-collection cases passed through server-owned HLS playback.
- The named credential profile passed homepage recommendations, authenticated history, authenticated watch-later, and space videos.
- `bangumi-media-series` and `bangumi-episode` passed with the available web-mode restricted proxy.
- Public/credential-backed cases used the persistent named profile only against Bilibili official endpoints. Restricted proxy validation used a separate temporary access-key-only profile, so the persistent Web cookie was not sent to the public proxy; the temporary store was deleted after both cases passed.
- The unavailable proxy fixture failed safely as `restricted_proxy`, without leaking credential or upstream response details.
- Following/dynamic cases remain blocked by a `bbdown-core v0.5.0` compatibility issue: the upstream dynamic payload now returns `module_author.pub_ts` as a numeric string while the dependency requires an integer. This is an external dependency follow-up, not a passing live case.

## Validation

- Final Rust gate: 475 unit tests, 33 deterministic live-harness tests plus 1 ignored opt-in live test, and 6 integration tests passed.
- Final official live run: ordinary video, multi-part video, space videos/collection, homepage recommendations, authenticated history, and authenticated watch-later passed in 317.66 seconds.
- Final restricted live run: Bangumi media series and episode passed in 12.90 seconds with a temporary access-key-only profile; the temporary credential root was removed after the run.
- Final Swift/Xcode gate: 261 Swift package tests and 1 macOS app integration test passed; generic tvOS, macOS, tvOS build-for-testing, and release Rust server builds succeeded.
- `cargo fmt --all`
- `just lint`
- `just pre-commit`
- `scripts/test-cache-server.sh`
- `cargo test -p tvos-net-player-cache-server completed_runtime --locked`
- `cargo test -p tvos-net-player-cache-server background_work --locked`
- `cargo test -p tvos-net-player-cache-server cancelling_task_removes_queued_jobs_and_cancels_current_without_requeue --locked`
- `cargo test -p tvos-net-player-cache-server collection_item_selection_bound --locked`
- `cargo test -p tvos-net-player-cache-server shutdown --locked`
- `cargo test -p tvos-net-player-cache-server credential_safe_log_detail_omits_raw_upstream_error --locked`
- `cargo test -p tvos-net-player-cache-server credential_configured --locked`
- `cargo test -p tvos-net-player-cache-server bbdown_file_failed_rolls_back_bytes_before_terminal_events --locked`
- `cargo test -p tvos-net-player-cache-server task_client_boundary_redacts_failed_child_of_completed_parent --locked`
- `cargo test -p tvos-net-player-cache-server worker_omits_adapter_failure_detail_when_credentials_are_configured --locked`
- `cargo test -p tvos-net-player-cache-server live_failure_message_omits_raw_detail --locked`
- `cargo test -p tvos-net-player-cache-server --test bilibili_live_e2e shutdown_cancels_untracked_registry_task_before_removing_case_root --locked`
- `cargo test -p tvos-net-player-cache-server --test bilibili_live_e2e teardown_failure_retains_case_root_and_persisted_state_for_recovery --locked`
- `cargo test -p tvos-net-player-cache-server --test bilibili_live_e2e --locked`
- Opt-in real live e2e: ordinary video, multi-part video, space collection/videos, homepage recommendations, authenticated history/watch-later, and both restricted Bangumi fixtures.
- `just test`
- `just test-macos`
- `just build-cache-server`
- `just build`
- `just build-macos`
- `just build-for-testing`
- Local tvOS simulator execution is machine-state blocked because CoreSimulator `1051.54.0` is older than the Xcode-required `1051.55.0`; generic tvOS and macOS builds pass, and GitHub CI remains the simulator execution gate.

## Next Steps

- Execute PR 7 for transcoding and ABR policy controls.
- Update `bbdown-core` after its dynamic timestamp parser accepts both integer and numeric-string payloads, then rerun following/dynamic live cases.
- Pause before deferred/non-sequential PR 6 and discuss the task options/result schema v2.

## Evidence

- Roadmap: `docs/project_journal/2026/06/2026-06-24-next-phase-productization-roadmap-a8d2c5.md`
- Live e2e skill: `.agents/skills/bilibili-live-e2e/SKILL.md`
- Cache-server architecture: `docs/architecture/cache-server.md`
