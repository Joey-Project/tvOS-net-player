---
id: 20261004-b46378
title: CDN ETag and Byte Content Validation
status: completed
created: 2026-10-04
updated: 2026-10-04
branch: wip/bangumi-live-validation
pr:
supersedes: []
superseded_by:
---

# CDN ETag and Byte Content Validation

## Scope
- Investigate the ordinary-video `chunk-origin-etag-change` diagnostics before changing playback or cache acceptance policies.
- Reuse the local test credential profile without changing account defaults, logging credentials, or requesting an unnecessary login.
- Use the canonical ordinary playlist's first candidate, Web-mode 360p H.264, with separate video and audio comparisons.
- Match the backend's actual media request headers. A different encoding-negotiation experiment must be labeled separately.
- Keep production ETag, byte-range, and cache-publication guards unchanged.

## Validation Plan
- Compare identical byte intervals across repeated requests and a bounded set of CDN origins, including the prefix, a later chunk, the middle, and the tail.
- Record exact byte/SHA-256 equality independently of strong, weak, or absent ETag observations, HTTP status, Content-Range, and total-length consistency.
- Keep signed media URLs, raw ETags, credentials, and media payloads out of logs and tracked records. Capture the fresh playback plan only in bounded memory.
- Bound the direct probe to 128 MiB of media, a 30-second request deadline, a 900-second overall deadline, and a 1 MiB diagnostic-output ceiling. Full-track comparisons are optional only when they fit the remaining budgets.
- If practical, repeat one 300-second ordinary-only LAN sustained-read case under a separate bounded supervisor. Reproduction of the warning and byte comparisons are separate outcomes.
- Use credential-free offline tests for the temporary probe, and harvest every running process before reporting final results.

## Interpretation Boundaries
- The original run logged two ETag-change warnings. Its first mismatch probe reported matching origin, total length, prefix SHA-256, and prefix ETag; this does not prove the remainder of the resource was unchanged.
- Samples cannot establish whole-file equivalence. A new run without an ETag warning cannot retroactively prove that the earlier event was harmless.
- A locally computed completed-file checksum proves retained-byte stability, not correct origin-version assembly without an independent baseline.
- Do not introduce a permissive playback policy solely from an inconclusive probe.

## Evidence
- Prior validation: `2026-10-02-bilibili-live-playback-b46377.md`.
- Canonical inputs: `.agents/skills/bilibili-live-e2e/references/live-cases.json`.
- Temporary diagnostic scope: `.codex-tmp/bilibili-etag-validation-20261004/` (ignored).

## Confirmed LAN Reproduction
- The unchanged release-mode Rust LAN suite passed the ordinary-only 300-second sustained-read case: 1,148 read rounds, 304.364 seconds including setup and teardown, producer exit 0.
- One `chunk-origin-etag-change` warning was observed. The first mismatch diagnostic reported matching origin, total length, prefix SHA-256, and baseline prefix ETag; both observed ETags had valid syntax.
- This reproduces the warning without establishing equality of the later bytes or complete offline-cache readiness. Production guards remain unchanged.
- The supervisor confirmed child reaping and successful teardown. The compact local receipt is `.codex-tmp/bilibili-etag-lan-20261004/summary.json`; it is explicitly a derived summary, not a reconstructed original log.

## Direct Probe Checkpoint
- Nine credential-free temporary helper tests and Python compilation passed before the initial live attempts.
- The sandbox attempt stopped during plan acquisition; the narrow escalated attempt acquired a playback plan but stopped at `q16_h264_variant_missing`. Both read zero media bytes and reaped their children.
- A bounded in-memory structural diagnostic prompted removal of the helper's unsupported fixed 640x360 constraint. The corrected helper selects the same canonical first-page q16 H.264 representation without imposing that aspect ratio; it does not substitute a different quality. The sanitized first-ten-variant diagnostic did not include q16 dimensions, so no actual q16 width or height is claimed.
- Twelve credential-free temporary helper tests and Python compilation passed after correction, including representation selection and valid-observation eligibility cases.
- The single corrected live probe resolved two fresh plans with two CDN origins and separate video/audio resources. It obtained a strong-ETag one-byte video length-discovery response, then stopped at `body_length_mismatch` after reading 441,522 aggregate payload bytes. It completed no eligible interval comparisons and no full-file comparison.
- The retained diagnostic lacks the failed response's expected and actual length breakdown. It cannot distinguish a truncated transfer from a helper/read-path issue, and it is not evidence that two equal byte intervals differed. The Python HTTP/1.1 probe also does not establish exact parity with the Rust backend's transport.
- The failure occurred after HTTP 206 and exact Content-Range validation. If Content-Length was present, it matched the range-derived expected length; otherwise the helper would have emitted `content_length_mismatch`. This narrows the failure phase without proving its cause.
- The direct result is inconclusive. No credential rejection was established, and no login or credential mutation occurred. Signed URLs, raw ETags, credentials, and media bodies were not retained.
- The corrected probe exited 2; its runner confirmed `child_reaped=true`, and both runner and probe reached a terminal state. The schema diagnostic exited 0. Neither diagnostic path remains running.

## Initial Decision
- The initial Python validation ended inconclusively; ETag/content equivalence was not established. Keep production playback and complete-cache acceptance rules unchanged.
- The warning reproduction and continuing LAN readability justify a focused follow-up, not an assertion that the warning is harmless.
- Before implementing provisional playback acceptance, collect identical-interval evidence through the Rust media transport with explicit expected/actual length diagnostics and independent CDN comparisons. Keep playback eligibility separate from complete offline-cache publication if a more permissive policy is later adopted.
- Retain the ignored temporary helper, credential-free tests, sanitized schema receipt, and compact LAN receipt for that follow-up. Generated Python bytecode was removed with `codex-clean-tmp`. Do not treat samples as whole-file or origin-version proof.
- Project-journal validation and `git diff --check` passed. This validation-only task did not run the complete deterministic suite, GitHub CI, formal reviews, or native AVPlayer decoding again.

## Rust Comparison Follow-Up
- Joey requested actual comparisons after the inconclusive initial probe. Use a temporary example in the existing Rust package to reuse the exact workspace dependency lock and reqwest features without changing library behavior, Cargo manifests, or complete-cache acceptance.
- Match the backend's connect/read/pool timeouts, default HTTP negotiation, media-header forwarding, and streamed-body reading. The diagnostic does not invoke cache publication or bypass guards in the production cache.
- Compare eligible identical ranges across repeated same-origin requests, separate CDN origins, and two fresh resolution plans. Keep video and audio separate; record different-range ETag changes as a distinct observation.
- Capture HTTP status/version, range/total validity, expected and actual body length, and a safe failure class before excluding an incomplete response. Continue to other bounded candidates rather than interpreting one failed transfer as content inequality.
- Keep live media below 128 MiB, each request below 30 seconds, the live probe below 600 seconds, and its diagnostic log below 1 MiB. Keep signed URLs, credentials, raw ETags, and payloads in memory only.
- Require a nonzero eligible comparison count and independent-origin evidence before reporting cross-CDN equivalence. Whole-track equality is a separate claim and is optional only within the same budgets.
- Lead owns this journal and integration decisions. A GPT-6 Luna worker owns the temporary Rust example, focused tests, and bounded execution artifacts under `.codex-tmp/bilibili-etag-rust-20261004/`.

## Confirmed Rust Sampling
- The corrected final sampling run selected q16 H.264 at 480x360 and AAC stream 30280, matching the backend's variant-selection ordering. It resolved twice and observed two independent effective CDN origins, using HTTP/1.1 and HTTP/2 respectively.
- The run exited 0 after 25.8 seconds. Of 64 observations, 54 were eligible: 46 media-range responses and eight one-byte length-discovery responses. Ten failed video responses were excluded.
- The diagnostic counted 111 identical-interval comparisons, including the one-byte discovery comparisons, with zero byte inequalities. Forty-eight comparisons had different valid ETags but equal bytes; these occurred on the audio track across origins. No different-interval ETag change was observed among eligible current samples.
- The primary video origin repeatedly declared a 524,288-byte or 1,048,576-byte range but the body stream failed after 441,521 bytes. Other video requests on that origin failed during send. The same short-read boundary seen by the initial Python probe is therefore independently reproduced with reqwest; its underlying network/CDN cause remains undetermined.
- The alternative origin served complete matching video ranges. Audio requests succeeded on both origins. Incomplete observations are neither content-inequality evidence nor acceptable cache spans.
- The initial Rust sampling attempt also collected valid comparisons, but the corrected final run is the authoritative sampling receipt: `.codex-tmp/bilibili-etag-rust-20261004/probe-final.log`. The final log is 37,347 bytes; the producer and supervisor reached terminal states.
- Four focused offline tests, Clippy with `-D warnings`, and the example build passed for this sampling version. No production acceptance rule changed.

## Whole-Track Supplement
- Discovered lengths are 16,698,182 video bytes and 15,151,519 audio bytes. Add one bounded whole-track comparison rather than extrapolating samples to full content.
- Issue exactly four full-track range requests from one plan, one per track and effective origin, plus at most one previously viable video middle-range check against an independently fetched full-track baseline. Do not retry failed origins or repeat the entire sampling sweep.
- Allow 90 seconds for a full-body request and 360 seconds for this supplementary run while retaining the backend's 20-second read timeout. Cap this run at 70 MiB; the two sampling runs consumed 59,299,932 bytes, leaving the combined experiment below 128 MiB.
- A complete body from only one origin is not cross-CDN whole-track equality. Publish separate outcomes for complete-track equality, sampled-span equality, and incomplete transfers.

## Whole-Track Results
- Exactly four full-track body requests were issued. Both audio origins returned all 15,151,519 expected bytes with valid strong ETags; the complete bodies and their SHA-256 digests matched, while the ETags differed.
- The video HTTP/2 route returned all 16,698,182 expected bytes. The HTTP/1.1 route advertised that same full length but failed after 441,521 bytes. Video whole-track equivalence across origins remains unknown; the incomplete body was excluded.
- The optional video middle-range check was not run. No failed full-track request was retried, and no additional live run followed this supplement.
- The supplementary probe finished in 16.1 seconds and exited 2 with `full_track_incomplete`, correctly reporting that both tracks did not complete on both origins. The supervisor also exited 2 and was confirmed terminal; this is not an all-pass full-video result.
- The supplement received 47,442,749 bytes including discovery; all Rust runs together received 106,742,681 bytes, below 128 MiB. The full log is 5,311 bytes. The safe derived receipt is `.codex-tmp/bilibili-etag-rust-20261004/full-summary.json`.
- Anonymous `cdn-N` labels are scoped to each run. Primary/backup ordering changed between plans; identical labels across logs are not evidence of the same origin. HTTP versions here describe the observed routes, not a controlled experiment proving that the protocol caused the failure.
- Four focused tests, formatting, Clippy with `-D warnings`, and a fresh example build passed before this supplementary run. The exact compiled source was archived under the ignored artifact directory, verified byte-for-byte, then the task-only example was removed from the package.

## Final Interpretation
- Different cross-CDN ETags do not imply different content: the complete audio comparison is direct evidence, not a prefix-only inference. Complete-cache correctness should concern the content version, not globally identical validator strings across origins.
- Current eligible samples did not reproduce the earlier same-origin ETag change. These results do not establish that the original warning was harmless, and they do not justify automatically accepting a same-origin changed validator.
- Length and stream-integrity checks remain necessary. A complete alternative video origin is usable evidence for that origin, not permission to cache a truncated response from another one.
- This comparison task is complete with explicit video/full-track limitations. Production acceptance and publication rules were not changed; the full deterministic suite, GitHub CI, and native AVPlayer decoding were not rerun for this diagnostic-only follow-up.
