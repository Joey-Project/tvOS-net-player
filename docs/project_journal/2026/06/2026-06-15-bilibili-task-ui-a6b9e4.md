---
id: 20260615-a6b9e4
title: Bilibili Task UI
status: completed
created: 2026-06-15
updated: 2026-06-15
branch: wip/bilibili-task-ui
pr: https://github.com/Joey-Project/tvOS-net-player/pull/16
supersedes: []
superseded_by:
---

# Bilibili Task UI

## Summary

- Add shared AppCore state for submitting Bilibili playback tasks to the LAN cache server.
- Keep gRPC as the control plane and keep playable media URLs on the LAN cache server HTTP/HLS plane.
- Wire the same task workflow into tvOS and macOS without expanding the deferred Bilibili page/episode/all result schema.

## Current State

- `BilibiliTaskViewModel` lives in `TVOSNetPlayerCore` and owns Bilibili source text, optional quality/codec hints, task progress, error state, cancellation, retry, and playable URL handoff.
- `TVOSNetPlayerCacheClient` now exposes `cancelTask(id:)` alongside the existing create/get/watch task APIs.
- tvOS and macOS app shells both expose Bilibili submit, progress, play, cancel, retry, and clear actions.
- Bilibili playback still routes through `PlayerViewModel.loadTransient`, so manual playback URLs are not overwritten by LAN task playback.
- Tests cover create options, task watch updates, playable URL exposure, cancellation, retry source preservation, and app-shell construction.

## Out Of Scope

- Explicit page/episode/all Bilibili task options and multi-item task result schema.
- LAN-side transcoding controls.
- Bonjour discovery, cache eviction UI, and weak-network/offline UX.

## Validation

- `scripts/test.sh`
