---
name: bilibili-live-e2e
description: Run this repository's opt-in real Bilibili live e2e smoke tests for the macOS/tvOS LAN cache playback path, including restricted-area Bangumi cases. Use when Joey asks to validate real Bilibili URLs, run live e2e, test bbdown-rust integration, verify macOS client playback readiness, or investigate live progressive HLS playback failures in tvOS-net-player.
---

# Bilibili Live E2E

## Overview

Validate the real Bilibili path through the repo-owned Rust LAN cache server and progressive HLS control plane. This skill is intentionally opt-in because it depends on public Bilibili availability, local network access, and BBDown core behavior that should not gate normal CI.

## Workflow

1. Read `references/live-cases.json` before running or modifying the live suite. It contains the canonical four real URLs and their expected intent.
2. Run the deterministic local gate first when changing code:

```bash
just test-cache-server
```

3. Run the live smoke suite explicitly:

```bash
just test-bilibili-live
```

4. If only one case is needed, pass a comma-separated case filter:

```bash
BILIBILI_LIVE_E2E_CASES=ordinary-video-playlist just test-bilibili-live
BILIBILI_LIVE_E2E_CASES=bangumi-media-series just test-bilibili-live
```

5. Default runs skip `requires_restricted_area_path` cases. Run those cases explicitly when validating BBDown restricted-area support; without a configured restricted-area route they are expected to fail with Bilibili area restriction errors. Pass local restricted-area runtime settings through these environment variables:

```bash
BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PATH=/path/to/credentials.json \
BILIBILI_LIVE_E2E_RESTRICTED_AREA=hk \
BILIBILI_LIVE_E2E_RESTRICTED_AREA_PROXY='hk=https://proxy.example/playurl' \
BILIBILI_LIVE_E2E_RESTRICTED_API_PROXY='hk=https://proxy.example/api' \
BILIBILI_LIVE_E2E_CASES=bangumi-media-series,bangumi-episode \
just test-bilibili-live
```

The credential file uses the `bbdown-core` JSON shape with optional `cookie`, `access_key`, and `tv_access_key` fields. Do not commit real credentials or real proxy tokens.
6. Treat failures as product evidence, not flaky CI noise. Capture the case id, failing phase, task state/message, and whether the failure is local code, BBDown core, credentials, region restriction, or upstream availability.

## Scope

- The live suite starts a local Rust cache server, resolves each Bilibili input, creates a progressive playback task, waits for a playable HLS source, and fetches the generated master playlist.
- The suite does not run in default `just ci` or GitHub Actions.
- The suite is for macOS/local development first; physical Apple TV validation remains a separate deploy path.
- The media plane must remain HTTP/HLS through the LAN cache server. Do not make the Swift app fetch Bilibili media URLs directly to satisfy this test.

## Resources

- `references/live-cases.json`: canonical real Bilibili e2e inputs.
- `scripts/test-bilibili-live.sh`: repo command used by the skill.
- `CacheServer/RustCacheServer/tests/bilibili_live_e2e.rs`: ignored Rust integration test run by the script.
