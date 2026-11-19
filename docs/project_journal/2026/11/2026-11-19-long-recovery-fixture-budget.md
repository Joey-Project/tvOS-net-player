---
id: 20261119-long-recovery-fixture-budget
title: Long Recovery Fixture Generation Budget
status: completed
created: 2026-11-19
updated: 2026-11-19
branch: codex/long-recovery-fixture-budget
pr:
supersedes: []
superseded_by:
---

# Long Recovery Fixture Generation Budget

## Summary
- Raise the long-recovery FFmpeg fixture-generation timeout from 120 to 240 seconds.
- Keep the profile assertion pinned to the exact 240-second cap.
- Preserve the finite 840-second case deadline and all source, probe, media, cache, and normal-profile budgets.

## Current State
- The long-recovery FFmpeg child process has a 240-second deadline.
- The required and diagnostic long-recovery cases retain their 840-second outer deadline; child process execution remains deadline-bounded.
- Long-source generation remains 3600 seconds. Playback and probe durations, FFprobe and normal FFmpeg timeouts, global budgets, fixture limits, and existing identity, fault, byte, and cleanup assertions are unchanged.
- The profile unit test asserts the exact long-recovery FFmpeg timeout and existing profile budgets.

## Follow-On Plan
1. After this prerequisite is accepted and integrated into `master`, update from `master` and finish the assigned PR #112 layered UI input work.
2. Keep clipboard input unimplemented and make no host input-method changes.

## Evidence
- Scoped implementation changed only the long-recovery FFmpeg timeout and its existing profile assertion.
- Focused unit checks do not establish native media or audible-audio acceptance.
- Full CI, exact-head GitHub Codex review, and independently validated hosted media/UI artifacts remain separate delivery gates; execution evidence is retained with the PR.
