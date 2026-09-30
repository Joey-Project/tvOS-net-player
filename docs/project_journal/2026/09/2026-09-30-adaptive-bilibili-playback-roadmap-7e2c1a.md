---
id: 20260930-7e2c1a
title: Adaptive Bilibili Resolution And Continuous Playback Fill
status: active
created: 2026-09-30
updated: 2026-09-30
branch:
pr:
supersedes: []
superseded_by:
---

# Adaptive Bilibili Resolution And Continuous Playback Fill

## Goal And Boundaries

- Keep Bilibili resolution, credentials, CDN choice, and media fetching on the Mac mini LAN server. tvOS/macOS clients receive only server-owned control-plane state and HTTP/HLS media URLs.
- Make the selected playable video/audio representation download to completion during playback, even after pause or a switch to another item, so a transient network loss does not erase the offline opportunity.
- Use the existing ABR-capable HLS metadata and client policy controls. Request additional `bbdown-core` interfaces only after the server integration identifies a concrete missing representation, URL-refresh, range, or measurement contract.
- Physical Apple TV validation remains deferred; use the macOS app and opt-in real Bilibili e2e tests for live validation.

## Decisions

### Credential Ownership And Login

- The Mac mini owns login initiation, polling/callbacks, credential persistence, refresh, and secret redaction. Clients may show a short-lived QR or login URL and redacted readiness but never receive an `access_key`, cookie, credential file path, or upstream media URL.
- Enabled restricted-area API proxies share the selected server credential profile's generic `access_key`. There is no per-proxy secret field. A profile may also carry the Web cookie required by authenticated Web page fetch.
- Web QR and generic access-key authorization are distinct protocols. The current core can poll Web QR, whereas BiliPlus/BALH access-key authorization needs a trusted browser handoff/callback. The server implementation must prove callback origin, session binding, expiry, and one-time consumption before advertising unattended access-key capture. It must not add a LAN-writable raw-secret RPC to the plaintext control plane.
- Preserve valid server-side credentials and use core refresh/readiness information to avoid repeated login prompts. A client request cannot overwrite a healthy credential without an explicitly authorized Mac-side action.
- PR 1 implements only the core's pollable Web QR flow: the Mac mini stores the QR ticket, polls upstream, and merges the resulting Web cookie and available refresh token into its configured private credential profile. The generic `access_key` remains in that server-side profile; Web QR must never replace it. Until a Mac-owned, origin- and session-bound browser handoff is proven, BiliPlus/BALH access-key capture is a separate follow-up and is not advertised as automatic login.
- A Web QR scan URL contains a short-lived login ticket even though it is not the resulting cookie. `Cache:AllowBilibiliLoginSessions` therefore defaults off and must be explicitly enabled on a trusted LAN; do not log or persist the scan URL in client state beyond the active session, and use a protected transport before exposing login outside that boundary. A missing server credential path is an operator setup error, not a reason to accept secret material from a LAN client.
- PR 1 saves Web QR refresh tokens but does not yet execute automatic Web-cookie refresh. Expired cookies and explicit account replacement need a Mac-authorized renewal path; clients cannot use the unauthenticated LAN login RPC to replace a present cookie. Add this as a credential-lifecycle follow-up before claiming unattended authentication maintenance.

### Resolver Selection

- Bundle the BBDown resolver catalog with the server as versioned, historically unverified candidates. Entries are enabled by default in settings; users can disable entries or add validated custom endpoints. Enabled does not mean every endpoint is contacted for every request: route through a small health-ranked subset with bounded concurrency and timeouts.
- Test connection readiness with bounded DNS/TLS/application-level probes before choosing a proxy. ICMP ping is not a reliable API-readiness check. Keep last-success, last-failure, latency, failure class, and cooldown; avoid probing the entire catalog on the playback critical path.
- For PGC (Bangumi, film, documentary), race official mainland resolution with the best region-appropriate Web-mode proxy when title/metadata suggests HK/TW or a series has confirmed region affinity. Title matching is a weak hint; promote affinity only after a genuine region restriction and successful same-identity proxy resolution.
- Record canonical content/season identity, failed direct-route reason, region, selected proxy, and expiry so subsequent episodes can prioritize a previously successful route. Authentication failures, generic timeouts, and transient 5xx errors must not train a region block. Retry official resolution periodically, and never accept a proxy result for a different episode or representation.
- The restricted API proxy path is Web-mode. Do not send TV API requests to these reverse proxies.

### CDN And ABR Selection

- Maintain privacy-bounded host-level recent health plus a short-lived per-content top-K host preference: reachability, first-byte latency, validated Range support, throughput, and failure/cooldown history. Persist only host identifiers and aggregated measurements, never signed URLs, request headers, or credentials.
- Use recent measurements to order fresh CDN candidates and permit concurrent requests for independent ranges/segments of the same selected representation. Include the ordinary playback path, not only explicit download tasks. Apply concurrency and bandwidth limits so probing or background fill cannot overwhelm foreground playback or the LAN.
- Validate 206 `Content-Range`, total size, returned length, and stable representation identity; quarantine a discrepant edge and retry conservatively. Upstream parallel-download tests support specific cross-CDN samples but prefix/length equality alone is not a universal content-integrity proof. When stronger validators are unavailable, record this residual risk and keep a single-origin fallback.
- Preserve the current ABR policy and per-variant cache identity. A quality switch chooses a different representation rather than mixing bytes across variants. Determine whether `bbdown-core` must expose ordered variant candidates, refreshed signed URLs, range/probe outcomes, or CDN host measurements only when implementing the server adapter; send a focused upstream request with a failing fixture and expected contract if needed.

### Continuous Full-Asset Fill

- On first playback, begin filling the entire selected video and audio representation while serving foreground reads. Playhead requests preempt background ranges; pause, seek, end-of-playback, and switching to another item do not cancel the old fill.
- A new foreground session may aggressively demote old fills. Old jobs retain durable partial progress, run at very low priority during foreground activity, and fairly complete when no foreground playback remains. Continuous foreground use may delay old jobs indefinitely; that is intentional.
- Deduplicate foreground and background range work. Checkpoint completed extents, source representation identity, and expected total size before marking bytes durable. Resume missing extents after restart or preemption rather than restarting from byte zero. Explicit user cancellation/deletion, quota, or safety failures may stop a fill and must surface a typed status.
- One selected representation is required to become complete for offline use. Other ABR variants may remain partial; serving them does not imply that the item is fully offline.

## Sequential PRs

1. **PR 0: Core baseline and long live probe.** Upgrade `bbdown-core` to the current verified revision, address numeric-string `module_author.pub_ts` compatibility upstream or with a narrowly justified bridge, and add opt-in sustained HLS/Range live validation. Freeze the existing public/restricted baseline before behavior changes.
2. **PR 1: Server-owned login.** Implement bounded QR/session state and Mac-owned credential lifecycle. Add tvOS/macOS display and redacted status. Prove the access-key browser handoff before promising automatic capture.
3. **PR 2: Resolver routing.** Bundle the BBDown catalog, settings toggle/custom entries, bounded health scoring, concurrent official/proxy PGC resolution, and expiring content/series route memory. Include decision-trace tests without leaking credentials.
4. **PR 3: CDN intelligence.** Add host/per-content measurement history, candidate ranking, opt-in probing, and existing download-path CDN optimization. Cover failures, signed URL refresh, and fallback.
5. **PR 4: Continuous progressive fill.** Implement durable full-asset range fill, foreground preemption/background fairness, cross-CDN chunk scheduling and verification, ABR variant isolation, offline completion, and weak-network status. Extend `bbdown-core` only for observed API gaps.

For each PR: complete implementation tests, Swift/Rust lint and formatting, macOS build/tests, and the applicable real-network e2e budget; push only after the local gates. Request GitHub PR `@codex review` on the final pushed head, require green CI and zero unresolved conversations, then merge. Update `master` and branch anew before the next PR. Do not run local review lanes.

## Live Validation And Inputs

- Reuse the four committed real URLs: ordinary playlist `BV1QtjA6BEB8`, multi-part `BV1uW4y1s7zN`, Bangumi series `md28338980`, and Bangumi episode `ep664928`. Also retain authenticated collection/list cases for pagination and Web-cookie coverage.
- Run the existing public smoke suite first, then an opt-in sustained media read of at least several minutes against the LAN server and a resume/offline replay probe once continuous fill exists. Capture elapsed time, bytes served, state transitions, completed extents, selected representation, and CDN host aggregates without raw URL or secret logging.
- Restricted Bangumi validation requires a working Web-mode region proxy and a server-owned access-key credential profile. Authenticated page-fetch cases additionally require a Web cookie. Treat missing/expired credentials or unavailable public proxies as an external validation blocker, not a passing test.

## Current State

- The current server already has HLS/ABR metadata, weak-network policies, a fill scheduler, a core adapter, credential-profile status, and login-session protobuf messages. Login-session RPCs are not yet implemented, and runtime playback does not yet provide durable full-asset multi-CDN fill.
- `BBDown-rust` revision `72b0c1ed5313df07ec5441cc4654afc1521d5165` contains CDN probe/parallel download work. The app's prior pin was `b5dde066561fc39c6387198f6e9a61513ee44eee`. Upstream PR [#78](https://github.com/Joey-Project/BBDown-rust/pull/78) was squash-merged as `fe79961c15d38e32b2e9c0d9e5bf9291edb440a4`, fixing both multithreaded `Send` embedding and numeric-string dynamic `pub_ts` parsing; its tested PR head and merged commit have the same Git tree.
- PR 0 pins the merged core revision, adds the opt-in sustained probe, and passes the ordinary-video 180-second LAN HLS/Range run (239 rounds) plus public multi-part and collection live cases without credentials. This is a readability baseline, not proof of AVPlayer decoding or complete offline fill. During the sustained run, a separate full-cache finalize GET reported a response-body decode error while the runtime source remained playable. The safe log does not identify a failing host or prove a cause; PR 4 must test resumable completion under this failure mode rather than treating the online probe as an offline-cache pass.
- PR 1 adds opt-in server-owned Web QR login, bounded/cancellable in-memory sessions, private per-profile cookie/refresh-token persistence, and redacted QR/status presentation in both clients. Existing Web cookies and generic access keys are protected from replacement through the LAN RPC. Local gate evidence: Rust 871 unit + 42 harness + 6 integration tests, Swift package 319 tests, macOS app target test, pre-commit lint, and release server build passed. The public ordinary-video 300-second LAN HLS/Range probe completed 1099 rounds; public multi-part and space-collection cases also passed. Real QR confirmation, restricted Bangumi, and authenticated fetch remain unverified until a user-provided login or private credential is available.

## Next Steps

- Obtain one real Web QR confirmation when the operator is available, then validate private credential persistence and authenticated page fetch without exposing the cookie to clients or tracked artifacts.
- Implement Mac-authorized cookie replacement and automatic Web-cookie refresh before claiming unattended credential maintenance. The BiliPlus/BALH access-key browser handoff still needs a separately proven origin/session-bound design.
- After PR 1 passes current-head GitHub CI/review and merges, update `master` and start PR 2 resolver routing. Identify any concrete ABR API gap only when server integration exposes one; the current adapter already receives variants and backup media URLs.
