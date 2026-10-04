---
id: 20261004-v2gate
title: Codex Review Gate v2 Consumer Installation
status: active
created: 2026-10-04
updated: 2026-10-04
branch:
pr:
supersedes: []
superseded_by:
---

# Codex Review Gate v2 Consumer Installation

## Summary
- The target-branch installation replaces the dedicated v1 review producer with the canonical v2 verifier and controller, and protects the workflow control plane through CODEOWNERS.

## Current State
- `.github/workflows/codex-review-gate.yml` uses the floating `@v2` action, read-only verifier permissions including `actions: read`, `request_author_permission: any`, and `request_review: false`.
- `.github/workflows/codex-review-gate-controller.yml` is installed; `.github/CODEOWNERS` assigns workflow and CODEOWNERS ownership to `@JoeyTeng`.
- The production ruleset is unchanged by this consumer installation. This note does not claim that a v2 production requirement or the wider repository migration is complete.

## Next Steps
- Complete the separately authorized production-ruleset transition while preserving existing rules and without adding a new `@codex` requirement; continue the remaining consumer rollout before recording the shared cutover as complete.

## Evidence
- Consumer implementation commit: `b2740a5cf2a6c1a86a2850fbcc622743ba5f685d`.
- Canonical bootstrap preview/apply and exact verifier/controller template comparisons passed; `actionlint` 1.7.12 and `git diff --check` passed.
