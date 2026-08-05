---
name: bilibili-live-e2e
description: Run this repository's opt-in real Bilibili live e2e smoke tests for the macOS/tvOS LAN cache playback path, including restricted-area Bangumi cases. Use when Joey asks to validate real Bilibili URLs, run live e2e, test bbdown-rust integration, verify macOS client playback readiness, or investigate live progressive HLS playback failures in tvOS-net-player.
---

# Bilibili Live E2E

## Overview

Validate the real Bilibili path through the repo-owned Rust LAN cache server and progressive HLS control plane. This skill is intentionally opt-in because it depends on public Bilibili availability, local network access, and BBDown core behavior that should not gate normal CI.

## Workflow

1. Read `references/live-cases.json` before running or modifying the live suite. It contains the canonical real URLs and their expected intent.
2. Read `references/restricted-api-proxies.json` before restricted-area validation. It records BiliRoaming public reverse proxies, sorted by latest known successful playback validation.
3. Run the deterministic local gate first when changing code:

```bash
just test-cache-server
```

4. Run the live smoke suite explicitly:

```bash
just test-bilibili-live
```

5. If only one case is needed, pass a comma-separated case filter:

```bash
BILIBILI_LIVE_E2E_CASES=ordinary-video-playlist just test-bilibili-live
BILIBILI_LIVE_E2E_CASES=bangumi-media-series just test-bilibili-live
BILIBILI_LIVE_E2E_CASES=space-collection just test-bilibili-live
```

6. Default runs skip `requires_collection_list_validation` cases. Collection/list cases cover favorite lists, uploader space videos, uploader collections, uploader series, and homepage recommendations; they are explicit because public-looking Bilibili list/feed APIs can require cookies, become empty, be rate-limited, or change availability independently of the app. Prefer `BILIBILI_LIVE_E2E_CASES=space-collection` for the stable public collection smoke. `BILIBILI_LIVE_E2E_INCLUDE_COLLECTION_LIST=1` adds eligible unauthenticated collection/list cases to a broader unfiltered local sweep, but that sweep can still fail on upstream availability/rate limits. Authenticated collection/list cases require `BILIBILI_LIVE_E2E_INCLUDE_AUTHENTICATED=1` and a web-cookie credential, and cases marked `requires_live_sample_override` need a current URL override before they join the unfiltered sweep:

```bash
BILIBILI_LIVE_E2E_INCLUDE_COLLECTION_LIST=1 just test-bilibili-live

BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PATH=/path/to/credentials.json \
BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PROFILE=family-room \
BILIBILI_LIVE_E2E_INCLUDE_AUTHENTICATED=1 \
BILIBILI_LIVE_E2E_FAVORITE_URL='https://www.bilibili.com/list/ml...' \
BILIBILI_LIVE_E2E_SPACE_VIDEOS_URL='https://space.bilibili.com/<mid>' \
BILIBILI_LIVE_E2E_COLLECTION_URL='https://www.bilibili.com/list/<mid>?sid=<sid>' \
BILIBILI_LIVE_E2E_SERIES_URL='https://www.bilibili.com/list/<mid>?sid=<sid>&type=series' \
BILIBILI_LIVE_E2E_RECOMMENDATIONS_URL='https://www.bilibili.com/' \
BILIBILI_LIVE_E2E_INCLUDE_COLLECTION_LIST=1 \
just test-bilibili-live
```

Explicit `BILIBILI_LIVE_E2E_CASES` filters bypass the default skip policy and are useful for investigating a committed sample or upstream failure, but stable smoke validation should provide URL overrides for `requires_live_sample_override` cases. These cases assert that the LAN server resolves list candidates with bounded stable `item:` selection ids containing a canonical collection-source token plus BVID/AID/CID identity, and that generated HLS URLs stay on the LAN media listener. `space-videos` and `homepage-recommendations` currently require a BBDown web cookie in local validation.

7. Default runs skip `requires_authentication` cases. Run authenticated cases explicitly with `BILIBILI_LIVE_E2E_CASES`, or include all authenticated cases in an unfiltered local run with `BILIBILI_LIVE_E2E_INCLUDE_AUTHENTICATED=1`. These cases require a BBDown credential file containing a web cookie; `access_key` alone is not enough for web-page fetch coverage. `authenticated-space-dynamic` defaults to `https://space.bilibili.com/2/dynamic`, but local validation should usually override it with an account-relevant uploader dynamic URL:

```bash
BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PATH=/path/to/credentials.json \
BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PROFILE=family-room \
BILIBILI_LIVE_E2E_CASES=authenticated-history,authenticated-watch-later,authenticated-following-feed \
just test-bilibili-live

BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PATH=/path/to/credentials.json \
BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PROFILE=family-room \
BILIBILI_LIVE_E2E_SPACE_DYNAMIC_URL='https://space.bilibili.com/<mid>/dynamic' \
BILIBILI_LIVE_E2E_CASES=authenticated-space-dynamic \
just test-bilibili-live
```

Authenticated failure messages are classified as `credential`, `empty_account_state`, `upstream_schema_or_availability`, `restricted_proxy`, or `server_bug` to keep live validation actionable. Whenever a credential path is configured, including when that file cannot be loaded, the server assigns a non-sensitive failure class before replacing the raw detail and carries that class in a fixed client-safe marker. The harness consumes that marker instead of trying to infer a category from already-redacted text. Raw upstream and task details are omitted from harness output, same-process background logs, client-facing task/result/RPC errors, and running download progress; the suite still reports a final list of every failed case. Each case shutdown stops and awaits both listeners, enumerates every task in the isolated registry even when the client did not receive a create response, cancels task and HLS-fill work, and removes the temporary root only after stable background quiescence and HLS-worker shutdown. If teardown cannot prove that state, it retains the isolated root and persisted task state and reports the recovery path instead of deleting evidence that background work may still use.

8. Default runs skip `requires_restricted_area_path` cases. Run those cases explicitly when validating BBDown restricted-area support; without a configured restricted-area route they are expected to fail with Bilibili area restriction errors. Public BiliRoaming reverse proxies are web-mode API proxies, so use `BILIBILI_LIVE_E2E_RESTRICTED_API_PROXY` and keep the fixture `prefer_tv_api` disabled for these cases. Proxy requests may include the configured `access_key`; use a self-hosted or otherwise trusted proxy for normal validation, and use public proxies only with a disposable/test credential or for explicit availability probing. Pass local restricted-area runtime settings through these environment variables:

```bash
BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PATH=/path/to/credentials.json \
BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PROFILE=family-room \
BILIBILI_LIVE_E2E_RESTRICTED_AREA=hk \
BILIBILI_LIVE_E2E_RESTRICTED_API_PROXY='hk=https://trusted-proxy.example' \
BILIBILI_LIVE_E2E_CASES=bangumi-media-series,bangumi-episode \
just test-bilibili-live
```

The credential file uses the `bbdown-core` JSON shape with optional `cookie`, `access_key`, and `tv_access_key` fields. Set `BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PROFILE` when the file is a multi-profile store and the live credential is not the default profile. Do not commit real credentials or real proxy tokens.
9. Treat failures as product evidence, not flaky CI noise. Capture the case id, failing phase, safe task summary, and whether the failure is local code, BBDown core, credentials, region restriction, account state, or upstream availability. Never add raw credential-backed upstream errors to tracked artifacts.

## Scope

- The live suite starts an isolated local Rust cache server for each selected case, resolves the Bilibili input, creates a progressive playback task, waits for a playable HLS source, and fetches the generated master playlist. A failing case does not prevent later selected cases from running.
- The suite does not run in default `just ci` or GitHub Actions.
- The suite is for macOS/local development first. Physical Apple TV validation is intentionally outside the current plan.
- The media plane must remain HTTP/HLS through the LAN cache server. Do not make the Swift app fetch Bilibili media URLs directly to satisfy this test.
- Public reverse proxies cannot be used with BBDown TV playurl mode. TV login remains useful for direct TV API checks, but restricted-area public proxy validation should use web/app planning paths. Treat public hosts as untrusted by default because restricted API proxy requests can include an `access_key`.

## Resources

- `references/live-cases.json`: canonical real Bilibili e2e inputs.
- `references/restricted-api-proxies.json`: BiliRoaming public reverse-proxy registry with latest known local validation status.
- `scripts/test-bilibili-live.sh`: repo command used by the skill.
- `CacheServer/RustCacheServer/tests/bilibili_live_e2e.rs`: ignored Rust integration test run by the script.
