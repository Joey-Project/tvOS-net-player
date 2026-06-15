---
id: 20260615-d2f8a1
title: Cache Library Pagination And Search
status: completed
created: 2026-06-15
updated: 2026-06-15
branch: wip/cache-library-pagination-search
pr: 17
supersedes: []
superseded_by:
---

# Cache Library Pagination And Search

## Summary

- Replace the bounded first-page cache library preview with explicit AppCore pagination and search state.
- Keep the existing LAN cache server gRPC page-token and `filter.search_text` contract.
- Wire the same search and load-more workflow into tvOS and macOS.

## Current State

- `CacheLibraryViewModel` owns `searchText`, `activeSearchText`, pending-search detection, `hasMoreItems`, `canLoadMore`, and `loadMore()`.
- Initial refresh now requests the first 50-item library page and records the returned next-page token.
- `loadMore()` appends the next page using the active search text and disables paging while the search field has unsubmitted edits.
- tvOS and macOS both expose cache library search controls and a load-more action when more pages are available.
- The media plane remains unchanged: selected cached items still resolve to LAN HTTP/HLS URLs before handoff to `PlayerViewModel`.

## Out Of Scope

- Bonjour discovery.
- Cache eviction and item deletion UI.
- Weak-network/offline playback actions.
- Bilibili page/episode/all selection and multi-item result schema.

## Validation

- `just format`
- `python3 /Users/joey/.codex/personal-sync/overlays/private/releases/bb9b591d6375c3c11482cb4fa99394132419c816/personal_codex/skills/project-journal/scripts/project_journal.py validate --repo /Users/joey/Program/Codex-workspace/tvOS-net-player`
- `git diff --check`
- `swift test --filter CacheLibraryViewModelTests`
- `scripts/test.sh`
- `just ci`

## Next Steps

- Continue with PR 3: task retention and cleanup foundation.
- Then continue with PR 5: discovery, cache management, and weak-network UX.
