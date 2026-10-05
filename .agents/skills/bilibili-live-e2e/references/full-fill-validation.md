# Full-Fill Live Validation

Use this mode when validation needs durable whole-asset fill and cache-only playback, not just a playable HLS master.

## Supported Cases

- `ordinary-video-playlist`: first candidate.
- `bangumi-media-series`: first candidate.
- `bangumi-episode`: default/current candidate when available, otherwise first; explicit first-candidate selection is also supported.

Unknown cases and multi-result selection are not admitted to full-fill mode. Both the expected selection and the actual playable/listed results must contain exactly one result. Bangumi remains explicitly opt-in and public restricted proxies must use Web mode, not TV API.

## Run Shape

Select one case per full-fill run. Pass an existing private credential path/profile explicitly; do not assume the machine's default profile is the intended test account. Configure the restricted Web proxy only when the selected case needs that route.

```bash
BILIBILI_LIVE_E2E_CASES=bangumi-episode \
BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PATH=/path/to/private/credentials.json \
BILIBILI_LIVE_E2E_BBDOWN_CREDENTIAL_PROFILE=test-profile \
BILIBILI_LIVE_E2E_RESTRICTED_AREA=hk \
BILIBILI_LIVE_E2E_RESTRICTED_API_PROXY='hk=https://trusted-proxy.example' \
BILIBILI_LIVE_E2E_FULL_FILL=1 \
BILIBILI_LIVE_E2E_SUSTAINED_SECONDS=300 \
BILIBILI_LIVE_E2E_OFFLINE_SECONDS=300 \
just test-bilibili-live
```

The harness bounds full-fill cache usage to 1 GiB and allows up to 1,200 seconds for fill. A 300-second foreground probe plus a 300-second offline window, setup, and teardown require an outer deadline greater than their sum; a 600-second supervisor cannot cover that run. For one case, an outer deadline of 2,400 seconds also leaves headroom for the fill deadline and orderly teardown. Use a bounded output sink. If the supervisor stops the process early, record the run as incomplete and retain its recovery root until cleanup is proven safe.

## Evidence

The mode quiesces and restarts the same server root, verifies checkpoint representation and non-regressing extents when a durable partial checkpoint was observed, waits for complete selected-variant fill, then restarts with Bilibili disabled. It walks cached media and verifies that completed cache files retain their checksums across offline reads. A fill that completed before quiescence is not proof of partial-resume behavior.

## Native Playback

To expose the cache-only source to a separate native probe, also set:

```bash
export BILIBILI_LIVE_E2E_LAN_PLAYBACK_READY_PATH="$PWD/.codex-tmp/fill-pr4-live/ready.json"
```

This exact repository-local path is deliberately fixed. It requires full-fill mode and the 300-second offline window. Publication refuses to overwrite a pre-existing file; preserve and investigate old evidence rather than deleting it indiscriminately. The ready file contains a clean LAN-only source, the case ID, byte count, and an expiry window, not an upstream media URL or credential.

Compile `scripts/probe-lan-playback.swift` before the producer reaches that window. Consume only the fresh ready document for the expected case, and finish the native probe before the producer shuts down. A 120-second probe with a 180-second deadline needs at least 190 seconds remaining in the 300-second window. The producer removes its ready file only after confirmed listener shutdown.

Report decoded-frame and control results from the native probe separately from HTTP readability, cache completion, and checksum results. A probe self-test is preparation, not live video playback evidence.
