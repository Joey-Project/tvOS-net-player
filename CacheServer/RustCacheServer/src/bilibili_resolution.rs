use std::{
    collections::{HashMap, HashSet, VecDeque},
    mem,
    time::{Duration, Instant, SystemTime},
};

use prost::Message;
use tonic::Status;
use uuid::Uuid;

use crate::{
    bilibili_playback::{
        BilibiliContentIdentity, BilibiliInputResolution, BilibiliResolvedCandidate,
        MAX_BILIBILI_RESOLUTION_SNAPSHOT_BYTES, MAX_BILIBILI_RESOLUTION_STRING_BYTES,
        MAX_BILIBILI_RESOLVE_CANDIDATE_LIMIT,
    },
    generated::tvos_net_player::v1::{
        BilibiliPlaybackOptions, BilibiliResolutionSelection, BilibiliResolutionSelectionMode,
    },
};

pub(crate) const BILIBILI_RESOLUTION_SESSION_TTL: Duration = Duration::from_secs(15 * 60);
pub(crate) const BILIBILI_RESOLUTION_REAPER_INTERVAL: Duration = Duration::from_secs(60);
pub(crate) const DEFAULT_BILIBILI_RESOLUTION_PAGE_SIZE: usize = 50;
pub(crate) const MAX_BILIBILI_RESOLUTION_PAGE_SIZE: usize = 200;
pub(crate) const MAX_BILIBILI_RESOLUTION_CANDIDATES: usize = MAX_BILIBILI_RESOLVE_CANDIDATE_LIMIT;
pub(crate) const MAX_BILIBILI_RESOLUTION_TASK_CANDIDATES: usize = 100;
pub(crate) const MAX_BILIBILI_RESOLUTION_BLOCKING_OPERATIONS: usize = 4;
const MAX_BILIBILI_RESOLUTION_SESSIONS: usize = 32;
const MAX_BILIBILI_RESOLUTION_TOTAL_CANDIDATES: usize = 50_000;
const MAX_BILIBILI_RESOLUTION_TOKEN_BYTES: usize = 256;
const OPAQUE_UUID_HEX_BYTES: usize = 32;
const BILIBILI_PAGE_TOKEN_ESTIMATED_BYTES: usize = "bilibili-page-".len() + OPAQUE_UUID_HEX_BYTES;

#[derive(Clone, Debug)]
pub(crate) struct TokenizedBilibiliCandidate {
    pub(crate) token: String,
    pub(crate) candidate: BilibiliResolvedCandidate,
}

#[derive(Clone, Debug)]
pub(crate) struct BilibiliResolutionSessionView {
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) title: String,
    pub(crate) source_kind: String,
    pub(crate) created_at: SystemTime,
    pub(crate) expires_at: SystemTime,
    pub(crate) default_candidate_token: String,
}

#[derive(Clone, Debug)]
pub(crate) struct BilibiliResolutionPageView {
    pub(crate) session: BilibiliResolutionSessionView,
    pub(crate) candidates: Vec<TokenizedBilibiliCandidate>,
    pub(crate) total_size: usize,
    pub(crate) next_page_token: String,
    pub(crate) snapshot_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AcceptedBilibiliResolution {
    pub(crate) source: String,
    pub(crate) title: String,
    pub(crate) options: Option<BilibiliPlaybackOptions>,
    pub(crate) candidates: Vec<BilibiliResolvedCandidate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BilibiliTaskCandidateRecord {
    pub(crate) selection_id: String,
    pub(crate) title: String,
    pub(crate) subtitle: String,
    pub(crate) source_kind: String,
    pub(crate) content_id: String,
    pub(crate) identity: BilibiliContentIdentity,
    pub(crate) index: u32,
    pub(crate) duration_seconds: Option<u32>,
}

impl From<&BilibiliResolvedCandidate> for BilibiliTaskCandidateRecord {
    fn from(candidate: &BilibiliResolvedCandidate) -> Self {
        Self {
            selection_id: candidate.selection_id.clone(),
            title: candidate.title.clone(),
            subtitle: candidate.subtitle.clone(),
            source_kind: candidate.source_kind.clone(),
            content_id: candidate.content_id.clone(),
            identity: candidate.identity.clone(),
            index: candidate.index,
            duration_seconds: candidate.duration_seconds,
        }
    }
}

#[derive(Default)]
pub(crate) struct BilibiliResolutionStore {
    sessions_by_id: HashMap<String, BilibiliResolutionSessionSnapshot>,
    session_order: VecDeque<String>,
    page_cursors_by_token: HashMap<String, BilibiliResolutionPageCursor>,
    total_candidates: usize,
    total_bytes: usize,
    reaper_started: bool,
}

struct BilibiliResolutionSessionSnapshot {
    view: BilibiliResolutionSessionView,
    snapshot_id: String,
    options: Option<BilibiliPlaybackOptions>,
    candidates: Vec<TokenizedBilibiliCandidate>,
    candidate_offsets_by_token: HashMap<String, usize>,
    page_tokens_by_offset: HashMap<usize, String>,
    expires_at: Instant,
    estimated_bytes: usize,
}

#[derive(Clone)]
struct BilibiliResolutionPageCursor {
    session_id: String,
    snapshot_id: String,
    offset: usize,
}

impl BilibiliResolutionStore {
    pub(crate) fn mark_reaper_started(&mut self) -> bool {
        if self.reaper_started {
            return false;
        }
        self.reaper_started = true;
        true
    }

    #[cfg(test)]
    pub(crate) fn reaper_started(&self) -> bool {
        self.reaper_started
    }

    pub(crate) fn create_session(
        &mut self,
        resolution: BilibiliInputResolution,
        options: Option<BilibiliPlaybackOptions>,
        now: Instant,
        wall_clock_now: SystemTime,
        page_size: usize,
    ) -> Result<BilibiliResolutionPageView, Status> {
        self.prune(now);
        validate_page_size(page_size)?;
        validate_resolution(&resolution, options.as_ref())?;

        let session_id = format!("bilibili-resolution-{}", Uuid::new_v4().simple());
        let snapshot_id = format!("bilibili-resolution-snapshot-{}", Uuid::new_v4().simple());
        let expires_at = now + BILIBILI_RESOLUTION_SESSION_TTL;
        let wall_clock_expires_at = wall_clock_now
            .checked_add(BILIBILI_RESOLUTION_SESSION_TTL)
            .ok_or_else(|| {
                Status::internal("Bilibili resolution expiry could not be represented.")
            })?;

        let mut candidate_offsets_by_token = HashMap::with_capacity(resolution.candidates.len());
        let candidates = resolution
            .candidates
            .into_iter()
            .enumerate()
            .map(|(offset, mut candidate)| {
                let token = format!("bilibili-candidate-{}", Uuid::new_v4().simple());
                candidate_offsets_by_token.insert(token.clone(), offset);
                // Cover URLs are provider transport data and are not part of the v2 snapshot API
                // or the accepted task plan.
                candidate.cover_uri = String::new();
                TokenizedBilibiliCandidate { token, candidate }
            })
            .collect::<Vec<_>>();
        let default_candidate_token = resolution
            .default_selection_id
            .is_empty()
            .then(String::new)
            .unwrap_or_else(|| {
                candidates
                    .iter()
                    .find(|candidate| {
                        candidate.candidate.selection_id == resolution.default_selection_id
                    })
                    .map(|candidate| candidate.token.clone())
                    .unwrap_or_default()
            });
        let view = BilibiliResolutionSessionView {
            id: session_id.clone(),
            source: resolution.source,
            title: resolution.title,
            source_kind: resolution.source_kind,
            created_at: wall_clock_now,
            expires_at: wall_clock_expires_at,
            default_candidate_token,
        };
        let estimated_bytes =
            estimate_session_bytes(&view, &snapshot_id, options.as_ref(), &candidates)?;

        if estimated_bytes > MAX_BILIBILI_RESOLUTION_SNAPSHOT_BYTES {
            return Err(Status::resource_exhausted(
                "Bilibili resolution snapshot exceeds the server byte limit.",
            ));
        }
        while self.capacity_exceeded(candidates.len(), estimated_bytes) {
            let Some(oldest_id) = self.session_order.front().cloned() else {
                return Err(Status::resource_exhausted(
                    "Bilibili resolution snapshot capacity is unavailable.",
                ));
            };
            self.remove_session(&oldest_id);
        }

        self.total_candidates = self.total_candidates.saturating_add(candidates.len());
        self.total_bytes = self.total_bytes.saturating_add(estimated_bytes);
        self.session_order.push_back(session_id.clone());
        self.sessions_by_id.insert(
            session_id.clone(),
            BilibiliResolutionSessionSnapshot {
                view,
                snapshot_id,
                options,
                candidates,
                candidate_offsets_by_token,
                page_tokens_by_offset: HashMap::new(),
                expires_at,
                estimated_bytes,
            },
        );

        self.page(&session_id, None, now, page_size)
    }

    pub(crate) fn page(
        &mut self,
        session_id: &str,
        page_token: Option<&str>,
        now: Instant,
        page_size: usize,
    ) -> Result<BilibiliResolutionPageView, Status> {
        validate_page_size(page_size)?;
        let expired = self.prune(now);
        let session_id = normalized_session_id(session_id)?;
        if expired.contains(&session_id) {
            return Err(resolution_expired());
        }

        let offset = match page_token.filter(|token| !token.is_empty()) {
            Some(token) => {
                validate_token("Bilibili resolution page token", token)?;
                let cursor = self.page_cursors_by_token.get(token).ok_or_else(|| {
                    Status::invalid_argument(
                        "Bilibili resolution page token is invalid or expired.",
                    )
                })?;
                if cursor.session_id != session_id {
                    return Err(Status::invalid_argument(
                        "Bilibili resolution page token does not belong to this session.",
                    ));
                }
                let snapshot = self
                    .sessions_by_id
                    .get(&session_id)
                    .ok_or_else(resolution_not_found)?;
                if cursor.snapshot_id != snapshot.snapshot_id {
                    return Err(Status::invalid_argument(
                        "Bilibili resolution page token does not belong to this snapshot.",
                    ));
                }
                cursor.offset
            }
            None => 0,
        };
        self.page_at_offset(&session_id, offset, page_size)
    }

    pub(crate) fn accept_selection(
        &mut self,
        session_id: &str,
        selection: &BilibiliResolutionSelection,
        now: Instant,
    ) -> Result<AcceptedBilibiliResolution, Status> {
        let expired = self.prune(now);
        let session_id = normalized_session_id(session_id)?;
        if expired.contains(&session_id) {
            return Err(resolution_expired());
        }
        let snapshot = self
            .sessions_by_id
            .get(&session_id)
            .ok_or_else(resolution_not_found)?;
        let offsets = selected_offsets(snapshot, selection)?;
        let candidates = offsets
            .into_iter()
            .map(|offset| snapshot.candidates[offset].candidate.clone())
            .collect();
        Ok(AcceptedBilibiliResolution {
            source: snapshot.view.source.clone(),
            title: snapshot.view.title.clone(),
            options: snapshot.options.clone(),
            candidates,
        })
    }

    pub(crate) fn prune(&mut self, now: Instant) -> HashSet<String> {
        let expired = self
            .sessions_by_id
            .iter()
            .filter(|(_, snapshot)| snapshot.expires_at <= now)
            .map(|(session_id, _)| session_id.clone())
            .collect::<HashSet<_>>();
        for session_id in &expired {
            self.remove_session(session_id);
        }
        expired
    }

    fn page_at_offset(
        &mut self,
        session_id: &str,
        offset: usize,
        page_size: usize,
    ) -> Result<BilibiliResolutionPageView, Status> {
        let snapshot = self
            .sessions_by_id
            .get_mut(session_id)
            .ok_or_else(resolution_not_found)?;
        if offset > snapshot.candidates.len() {
            return Err(Status::invalid_argument(
                "Bilibili resolution page token offset is invalid.",
            ));
        }
        let end = offset
            .saturating_add(page_size)
            .min(snapshot.candidates.len());
        let candidates = snapshot.candidates[offset..end].to_vec();
        let next_page_token = if end < snapshot.candidates.len() {
            if let Some(token) = snapshot.page_tokens_by_offset.get(&end) {
                token.clone()
            } else {
                let token = format!("bilibili-page-{}", Uuid::new_v4().simple());
                snapshot.page_tokens_by_offset.insert(end, token.clone());
                self.page_cursors_by_token.insert(
                    token.clone(),
                    BilibiliResolutionPageCursor {
                        session_id: session_id.to_owned(),
                        snapshot_id: snapshot.snapshot_id.clone(),
                        offset: end,
                    },
                );
                token
            }
        } else {
            String::new()
        };
        Ok(BilibiliResolutionPageView {
            session: snapshot.view.clone(),
            candidates,
            total_size: snapshot.candidates.len(),
            next_page_token,
            snapshot_id: snapshot.snapshot_id.clone(),
        })
    }

    fn capacity_exceeded(&self, incoming_candidates: usize, incoming_bytes: usize) -> bool {
        self.sessions_by_id.len() >= MAX_BILIBILI_RESOLUTION_SESSIONS
            || self.total_candidates.saturating_add(incoming_candidates)
                > MAX_BILIBILI_RESOLUTION_TOTAL_CANDIDATES
            || self.total_bytes.saturating_add(incoming_bytes)
                > MAX_BILIBILI_RESOLUTION_SNAPSHOT_BYTES
    }

    fn remove_session(&mut self, session_id: &str) {
        let Some(snapshot) = self.sessions_by_id.remove(session_id) else {
            return;
        };
        self.total_candidates = self
            .total_candidates
            .saturating_sub(snapshot.candidates.len());
        self.total_bytes = self.total_bytes.saturating_sub(snapshot.estimated_bytes);
        self.session_order
            .retain(|candidate| candidate != session_id);
        self.page_cursors_by_token
            .retain(|_, cursor| cursor.session_id != session_id);
    }
}

fn validate_resolution(
    resolution: &BilibiliInputResolution,
    options: Option<&BilibiliPlaybackOptions>,
) -> Result<(), Status> {
    if resolution.candidates_truncated
        || resolution.candidates.len() > MAX_BILIBILI_RESOLUTION_CANDIDATES
    {
        return Err(Status::resource_exhausted(format!(
            "Bilibili input contains more than {MAX_BILIBILI_RESOLUTION_CANDIDATES} candidates."
        )));
    }
    if resolution.candidates.is_empty() {
        return Err(Status::failed_precondition(
            "Bilibili input did not resolve any selectable candidates.",
        ));
    }
    validate_required_string("Bilibili resolution source", &resolution.source)?;
    validate_required_string("Bilibili resolution title", &resolution.title)?;
    validate_required_string("Bilibili resolution source kind", &resolution.source_kind)?;
    validate_string(
        "Bilibili default candidate identity",
        &resolution.default_selection_id,
    )?;
    if options.is_some_and(|options| options.encoded_len() > MAX_BILIBILI_RESOLUTION_STRING_BYTES) {
        return Err(Status::resource_exhausted(
            "Bilibili playback options exceed the resolution-session limit.",
        ));
    }
    let mut selection_ids = HashSet::with_capacity(resolution.candidates.len());
    for candidate in &resolution.candidates {
        for (label, value) in [
            (
                "Bilibili candidate selection id",
                candidate.selection_id.as_str(),
            ),
            ("Bilibili candidate title", candidate.title.as_str()),
            ("Bilibili candidate subtitle", candidate.subtitle.as_str()),
            (
                "Bilibili candidate source kind",
                candidate.source_kind.as_str(),
            ),
            (
                "Bilibili candidate content id",
                candidate.content_id.as_str(),
            ),
            ("Bilibili candidate cover URI", candidate.cover_uri.as_str()),
        ] {
            validate_string(label, value)?;
        }
        if candidate.selection_id.trim().is_empty()
            || candidate.title.trim().is_empty()
            || candidate.source_kind.trim().is_empty()
            || candidate.content_id.trim().is_empty()
            || candidate.index == 0
            || !candidate.identity.is_complete()
            || !candidate.identity.matches_content_id(&candidate.content_id)
        {
            return Err(Status::failed_precondition(
                "Bilibili input resolved an incomplete stable candidate identity.",
            ));
        }
        if let Some(bvid) = candidate.identity.bvid.as_deref() {
            validate_string("Bilibili candidate BVID", bvid)?;
        }
        if !selection_ids.insert(candidate.selection_id.as_str()) {
            return Err(Status::failed_precondition(
                "Bilibili input resolved duplicate stable candidate identities.",
            ));
        }
    }
    if !resolution.default_selection_id.is_empty()
        && !selection_ids.contains(resolution.default_selection_id.as_str())
    {
        return Err(Status::failed_precondition(
            "Bilibili input resolved a default candidate outside the snapshot.",
        ));
    }
    Ok(())
}

fn validate_required_string(label: &str, value: &str) -> Result<(), Status> {
    validate_string(label, value)?;
    if value.trim().is_empty() {
        return Err(Status::failed_precondition(format!(
            "{label} is missing from the resolved input."
        )));
    }
    Ok(())
}

fn validate_string(label: &str, value: &str) -> Result<(), Status> {
    if value.len() > MAX_BILIBILI_RESOLUTION_STRING_BYTES {
        return Err(Status::resource_exhausted(format!(
            "{label} exceeds the resolution-session limit."
        )));
    }
    Ok(())
}

fn estimate_session_bytes(
    view: &BilibiliResolutionSessionView,
    snapshot_id: &str,
    options: Option<&BilibiliPlaybackOptions>,
    candidates: &[TokenizedBilibiliCandidate],
) -> Result<usize, Status> {
    let mut total = mem::size_of::<BilibiliResolutionSessionSnapshot>();
    for bytes in [
        view.id.len(),
        view.source.len(),
        view.title.len(),
        view.source_kind.len(),
        view.default_candidate_token.len(),
        snapshot_id.len(),
        options.map_or(0, Message::encoded_len),
        // The store owns an additional session-id key and queue entry.
        view.id.len(),
        view.id.len(),
    ] {
        total = total
            .checked_add(bytes)
            .ok_or_else(resolution_size_overflow)?;
    }
    for candidate in candidates {
        total = total
            .checked_add(mem::size_of::<TokenizedBilibiliCandidate>())
            .and_then(|value| value.checked_add(candidate.token.len()))
            // Candidate tokens are also owned as lookup-map keys.
            .and_then(|value| value.checked_add(candidate.token.len()))
            .and_then(|value| value.checked_add(mem::size_of::<(String, usize)>()))
            .and_then(|value| value.checked_add(candidate.candidate.selection_id.len()))
            .and_then(|value| value.checked_add(candidate.candidate.title.len()))
            .and_then(|value| value.checked_add(candidate.candidate.subtitle.len()))
            .and_then(|value| value.checked_add(candidate.candidate.source_kind.len()))
            .and_then(|value| value.checked_add(candidate.candidate.content_id.len()))
            .and_then(|value| value.checked_add(candidate.candidate.cover_uri.len()))
            .and_then(|value| {
                value.checked_add(
                    candidate
                        .candidate
                        .identity
                        .bvid
                        .as_ref()
                        .map_or(0, String::len),
                )
            })
            // Reserve hash-table control and allocation overhead conservatively.
            .and_then(|value| value.checked_add(32))
            .ok_or_else(resolution_size_overflow)?;
    }

    // A client can request page sizes that eventually materialize a cursor at every interior
    // snapshot offset. Reserve that worst case up front so lazy cursor creation cannot bypass
    // the aggregate snapshot byte limit.
    let cursor_count = candidates.len().saturating_sub(1);
    let per_cursor_bytes = mem::size_of::<(usize, String)>()
        .checked_add(mem::size_of::<(String, BilibiliResolutionPageCursor)>())
        .and_then(|value| value.checked_add(BILIBILI_PAGE_TOKEN_ESTIMATED_BYTES * 2))
        .and_then(|value| value.checked_add(view.id.len()))
        .and_then(|value| value.checked_add(snapshot_id.len()))
        .and_then(|value| value.checked_add(64))
        .ok_or_else(resolution_size_overflow)?;
    total = total
        .checked_add(
            per_cursor_bytes
                .checked_mul(cursor_count)
                .ok_or_else(resolution_size_overflow)?,
        )
        .ok_or_else(resolution_size_overflow)?;
    Ok(total)
}

fn selected_offsets(
    snapshot: &BilibiliResolutionSessionSnapshot,
    selection: &BilibiliResolutionSelection,
) -> Result<Vec<usize>, Status> {
    let mode = validate_bilibili_resolution_selection(selection)?;
    let candidate_tokens = &selection.candidate_tokens;
    let range_start = selection.range_start_candidate_token.as_str();
    let range_end = selection.range_end_candidate_token.as_str();

    match mode {
        BilibiliResolutionSelectionMode::Single => {
            Ok(vec![candidate_offset(snapshot, &candidate_tokens[0])?])
        }
        BilibiliResolutionSelectionMode::Multiple => {
            validate_task_candidate_count(candidate_tokens.len())?;
            let mut seen = HashSet::with_capacity(candidate_tokens.len());
            candidate_tokens
                .iter()
                .map(|token| {
                    let offset = candidate_offset(snapshot, token)?;
                    if !seen.insert(offset) {
                        return Err(Status::invalid_argument(
                            "Bilibili resolution selection contains a duplicate candidate token.",
                        ));
                    }
                    Ok(offset)
                })
                .collect()
        }
        BilibiliResolutionSelectionMode::Range => {
            let start = candidate_offset(snapshot, range_start)?;
            let end = candidate_offset(snapshot, range_end)?;
            if start > end {
                return Err(Status::invalid_argument(
                    "Bilibili resolution range start cannot follow its end in snapshot order.",
                ));
            }
            validate_task_candidate_count(end - start + 1)?;
            Ok((start..=end).collect())
        }
        BilibiliResolutionSelectionMode::All => {
            validate_task_candidate_count(snapshot.candidates.len())?;
            Ok((0..snapshot.candidates.len()).collect())
        }
        BilibiliResolutionSelectionMode::Unspecified => unreachable!("validated selection mode"),
    }
}

pub(crate) fn validate_bilibili_resolution_selection(
    selection: &BilibiliResolutionSelection,
) -> Result<BilibiliResolutionSelectionMode, Status> {
    if selection.candidate_tokens.len() > MAX_BILIBILI_RESOLUTION_CANDIDATES {
        return Err(Status::resource_exhausted(format!(
            "Bilibili resolution selection cannot exceed {MAX_BILIBILI_RESOLUTION_CANDIDATES} candidate tokens."
        )));
    }
    for token in &selection.candidate_tokens {
        if token.is_empty() {
            return Err(Status::invalid_argument(
                "Bilibili candidate token cannot be empty.",
            ));
        }
        validate_token("Bilibili candidate token", token)?;
    }
    let range_start = selection.range_start_candidate_token.as_str();
    let range_end = selection.range_end_candidate_token.as_str();
    if !range_start.is_empty() {
        validate_token("Bilibili range start candidate token", range_start)?;
    }
    if !range_end.is_empty() {
        validate_token("Bilibili range end candidate token", range_end)?;
    }

    let mode = BilibiliResolutionSelectionMode::try_from(selection.mode)
        .map_err(|_| Status::invalid_argument("Unknown Bilibili resolution selection mode."))?;
    match mode {
        BilibiliResolutionSelectionMode::Single => {
            require_empty_range(range_start, range_end)?;
            if selection.candidate_tokens.len() != 1 {
                return Err(Status::invalid_argument(
                    "Single Bilibili resolution selection requires exactly one candidate token.",
                ));
            }
        }
        BilibiliResolutionSelectionMode::Multiple => {
            require_empty_range(range_start, range_end)?;
            if selection.candidate_tokens.is_empty() {
                return Err(Status::invalid_argument(
                    "Multiple Bilibili resolution selection requires candidate tokens.",
                ));
            }
            validate_task_candidate_count(selection.candidate_tokens.len())?;
        }
        BilibiliResolutionSelectionMode::Range => {
            if !selection.candidate_tokens.is_empty()
                || range_start.is_empty()
                || range_end.is_empty()
            {
                return Err(Status::invalid_argument(
                    "Range Bilibili resolution selection requires only start and end candidate tokens.",
                ));
            }
        }
        BilibiliResolutionSelectionMode::All => {
            if !selection.candidate_tokens.is_empty()
                || !range_start.is_empty()
                || !range_end.is_empty()
            {
                return Err(Status::invalid_argument(
                    "All Bilibili resolution selection cannot include candidate tokens.",
                ));
            }
        }
        BilibiliResolutionSelectionMode::Unspecified => {
            return Err(Status::invalid_argument(
                "Bilibili resolution selection mode is required.",
            ));
        }
    }
    Ok(mode)
}

fn validate_task_candidate_count(candidate_count: usize) -> Result<(), Status> {
    if candidate_count > MAX_BILIBILI_RESOLUTION_TASK_CANDIDATES {
        return Err(Status::resource_exhausted(format!(
            "A Bilibili playback task cannot exceed {MAX_BILIBILI_RESOLUTION_TASK_CANDIDATES} candidates."
        )));
    }
    Ok(())
}

fn candidate_offset(
    snapshot: &BilibiliResolutionSessionSnapshot,
    token: &str,
) -> Result<usize, Status> {
    snapshot
        .candidate_offsets_by_token
        .get(token)
        .copied()
        .ok_or_else(|| {
            Status::invalid_argument(
                "Bilibili candidate token is invalid, expired, or belongs to another session.",
            )
        })
}

fn require_empty_range(start: &str, end: &str) -> Result<(), Status> {
    if !start.is_empty() || !end.is_empty() {
        return Err(Status::invalid_argument(
            "Explicit Bilibili candidate selection cannot include range tokens.",
        ));
    }
    Ok(())
}

fn validate_page_size(page_size: usize) -> Result<(), Status> {
    if page_size == 0 || page_size > MAX_BILIBILI_RESOLUTION_PAGE_SIZE {
        return Err(Status::invalid_argument(format!(
            "Bilibili resolution page size must be between 1 and {MAX_BILIBILI_RESOLUTION_PAGE_SIZE}."
        )));
    }
    Ok(())
}

fn normalized_session_id(session_id: &str) -> Result<String, Status> {
    if session_id.trim().is_empty() {
        return Err(Status::invalid_argument(
            "Bilibili resolution session id is required.",
        ));
    }
    if session_id.len() > MAX_BILIBILI_RESOLUTION_TOKEN_BYTES {
        return Err(Status::invalid_argument(
            "Bilibili resolution session id is invalid.",
        ));
    }
    Ok(session_id.to_owned())
}

fn validate_token(label: &str, token: &str) -> Result<(), Status> {
    if token.len() > MAX_BILIBILI_RESOLUTION_TOKEN_BYTES {
        return Err(Status::invalid_argument(format!("{label} is invalid.")));
    }
    Ok(())
}

fn resolution_not_found() -> Status {
    Status::not_found("Bilibili resolution session was not found; resolve the input again.")
}

fn resolution_expired() -> Status {
    Status::failed_precondition("Bilibili resolution session expired; resolve the input again.")
}

fn resolution_size_overflow() -> Status {
    Status::resource_exhausted("Bilibili resolution snapshot size overflowed the server limit.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bilibili_playback::{BilibiliContentIdentity, BilibiliContentKind};

    #[test]
    fn pages_one_immutable_snapshot_with_opaque_bound_tokens() {
        let now = Instant::now();
        let wall_clock_now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let mut store = BilibiliResolutionStore::default();
        let first = store
            .create_session(
                sample_resolution("BV1snapshot", 3),
                None,
                now,
                wall_clock_now,
                2,
            )
            .expect("first page should be created");

        assert_eq!(3, first.total_size);
        assert_eq!(vec![1, 2], candidate_indexes(&first));
        assert!(!first.next_page_token.is_empty());
        let second = store
            .page(&first.session.id, Some(&first.next_page_token), now, 2)
            .expect("continuation page should resolve");
        assert_eq!(vec![3], candidate_indexes(&second));
        assert_eq!(first.snapshot_id, second.snapshot_id);
        assert!(second.next_page_token.is_empty());

        let other = store
            .create_session(
                sample_resolution("BV1other", 1),
                None,
                now,
                wall_clock_now,
                1,
            )
            .expect("second session should be created");
        let error = store
            .page(&other.session.id, Some(&first.next_page_token), now, 1)
            .expect_err("page token must remain bound to its session");
        assert_eq!(tonic::Code::InvalidArgument, error.code());
    }

    #[test]
    fn expands_multiple_range_and_all_against_snapshot_order() {
        let now = Instant::now();
        let mut store = BilibiliResolutionStore::default();
        let page = store
            .create_session(
                sample_resolution("BV1select", 4),
                None,
                now,
                SystemTime::UNIX_EPOCH,
                4,
            )
            .expect("resolution should be created");
        let tokens = page
            .candidates
            .iter()
            .map(|candidate| candidate.token.clone())
            .collect::<Vec<_>>();

        let multiple = store
            .accept_selection(
                &page.session.id,
                &BilibiliResolutionSelection {
                    mode: BilibiliResolutionSelectionMode::Multiple.into(),
                    candidate_tokens: vec![tokens[2].clone(), tokens[0].clone()],
                    ..Default::default()
                },
                now,
            )
            .expect("multiple selection should resolve");
        assert_eq!(vec![3, 1], accepted_indexes(&multiple));

        let range = store
            .accept_selection(
                &page.session.id,
                &BilibiliResolutionSelection {
                    mode: BilibiliResolutionSelectionMode::Range.into(),
                    range_start_candidate_token: tokens[1].clone(),
                    range_end_candidate_token: tokens[3].clone(),
                    ..Default::default()
                },
                now,
            )
            .expect("range selection should resolve");
        assert_eq!(vec![2, 3, 4], accepted_indexes(&range));

        let all = store
            .accept_selection(
                &page.session.id,
                &BilibiliResolutionSelection {
                    mode: BilibiliResolutionSelectionMode::All.into(),
                    ..Default::default()
                },
                now,
            )
            .expect("all selection should resolve");
        assert_eq!(vec![1, 2, 3, 4], accepted_indexes(&all));
    }

    #[test]
    fn accepted_candidates_survive_session_expiry_as_independent_values() {
        let now = Instant::now();
        let mut store = BilibiliResolutionStore::default();
        let page = store
            .create_session(
                sample_resolution("BV1expiry", 2),
                None,
                now,
                SystemTime::UNIX_EPOCH,
                2,
            )
            .expect("resolution should be created");
        let accepted = store
            .accept_selection(
                &page.session.id,
                &BilibiliResolutionSelection {
                    mode: BilibiliResolutionSelectionMode::All.into(),
                    ..Default::default()
                },
                now,
            )
            .expect("selection should be accepted");

        store.prune(now + BILIBILI_RESOLUTION_SESSION_TTL);
        let error = store
            .accept_selection(
                &page.session.id,
                &BilibiliResolutionSelection {
                    mode: BilibiliResolutionSelectionMode::All.into(),
                    ..Default::default()
                },
                now + BILIBILI_RESOLUTION_SESSION_TTL,
            )
            .expect_err("expired session must reject new task acceptance");
        assert!(matches!(
            error.code(),
            tonic::Code::FailedPrecondition | tonic::Code::NotFound
        ));
        assert_eq!(vec![1, 2], accepted_indexes(&accepted));
    }

    #[test]
    fn rejects_truncated_resolution_instead_of_publishing_ambiguous_all_selection() {
        let mut resolution = sample_resolution("BV1truncated", 2);
        resolution.candidates_truncated = true;
        let error = BilibiliResolutionStore::default()
            .create_session(resolution, None, Instant::now(), SystemTime::UNIX_EPOCH, 2)
            .expect_err("truncated input must fail closed");
        assert_eq!(tonic::Code::ResourceExhausted, error.code());
    }

    #[test]
    fn candidate_tokens_cannot_cross_resolution_sessions() {
        let now = Instant::now();
        let mut store = BilibiliResolutionStore::default();
        let first = store
            .create_session(
                sample_resolution("BV1first", 1),
                None,
                now,
                SystemTime::UNIX_EPOCH,
                1,
            )
            .expect("first resolution should be created");
        let second = store
            .create_session(
                sample_resolution("BV1second", 1),
                None,
                now,
                SystemTime::UNIX_EPOCH,
                1,
            )
            .expect("second resolution should be created");

        let error = store
            .accept_selection(
                &second.session.id,
                &BilibiliResolutionSelection {
                    mode: BilibiliResolutionSelectionMode::Single.into(),
                    candidate_tokens: vec![first.candidates[0].token.clone()],
                    ..Default::default()
                },
                now,
            )
            .expect_err("candidate tokens must remain session-bound");

        assert_eq!(tonic::Code::InvalidArgument, error.code());
    }

    #[test]
    fn rejects_duplicate_tokens_and_reversed_ranges() {
        let now = Instant::now();
        let mut store = BilibiliResolutionStore::default();
        let page = store
            .create_session(
                sample_resolution("BV1invalid-selection", 3),
                None,
                now,
                SystemTime::UNIX_EPOCH,
                3,
            )
            .expect("resolution should be created");
        let tokens = page
            .candidates
            .iter()
            .map(|candidate| candidate.token.clone())
            .collect::<Vec<_>>();

        let duplicate = store
            .accept_selection(
                &page.session.id,
                &BilibiliResolutionSelection {
                    mode: BilibiliResolutionSelectionMode::Multiple.into(),
                    candidate_tokens: vec![tokens[0].clone(), tokens[0].clone()],
                    ..Default::default()
                },
                now,
            )
            .expect_err("duplicate candidate tokens must be rejected");
        assert_eq!(tonic::Code::InvalidArgument, duplicate.code());

        let reversed = store
            .accept_selection(
                &page.session.id,
                &BilibiliResolutionSelection {
                    mode: BilibiliResolutionSelectionMode::Range.into(),
                    range_start_candidate_token: tokens[2].clone(),
                    range_end_candidate_token: tokens[0].clone(),
                    ..Default::default()
                },
                now,
            )
            .expect_err("a reversed range must be rejected");
        assert_eq!(tonic::Code::InvalidArgument, reversed.code());
    }

    #[test]
    fn rejects_empty_candidate_token_before_snapshot_lookup() {
        let error = validate_bilibili_resolution_selection(&BilibiliResolutionSelection {
            mode: BilibiliResolutionSelectionMode::Multiple.into(),
            candidate_tokens: vec![String::new()],
            ..Default::default()
        })
        .expect_err("opaque candidate tokens cannot be normalized away");

        assert_eq!(tonic::Code::InvalidArgument, error.code());
        assert!(error.message().contains("cannot be empty"));
    }

    #[test]
    fn caps_explicit_range_and_all_task_selections() {
        let now = Instant::now();
        let candidate_count = MAX_BILIBILI_RESOLUTION_TASK_CANDIDATES + 1;
        let mut store = BilibiliResolutionStore::default();
        let page = store
            .create_session(
                sample_resolution(
                    "BV1execution-bound",
                    u32::try_from(candidate_count).expect("the execution bound should fit in u32"),
                ),
                None,
                now,
                SystemTime::UNIX_EPOCH,
                candidate_count,
            )
            .expect("bounded resolution session should be created");
        let tokens = page
            .candidates
            .iter()
            .map(|candidate| candidate.token.clone())
            .collect::<Vec<_>>();

        for selection in [
            BilibiliResolutionSelection {
                mode: BilibiliResolutionSelectionMode::Multiple.into(),
                candidate_tokens: tokens.clone(),
                ..Default::default()
            },
            BilibiliResolutionSelection {
                mode: BilibiliResolutionSelectionMode::Range.into(),
                range_start_candidate_token: tokens[0].clone(),
                range_end_candidate_token: tokens[candidate_count - 1].clone(),
                ..Default::default()
            },
            BilibiliResolutionSelection {
                mode: BilibiliResolutionSelectionMode::All.into(),
                ..Default::default()
            },
        ] {
            let error = store
                .accept_selection(&page.session.id, &selection, now)
                .expect_err("oversized task selections must be rejected");
            assert_eq!(tonic::Code::ResourceExhausted, error.code());
            assert!(error.message().contains("cannot exceed 100 candidates"));
        }
    }

    #[test]
    fn session_capacity_evicts_the_oldest_snapshot_and_its_tokens() {
        let now = Instant::now();
        let mut store = BilibiliResolutionStore::default();
        let first = store
            .create_session(
                sample_resolution("BV1oldest", 1),
                None,
                now,
                SystemTime::UNIX_EPOCH,
                1,
            )
            .expect("first resolution should be created");
        for index in 1..MAX_BILIBILI_RESOLUTION_SESSIONS {
            store
                .create_session(
                    sample_resolution(&format!("BV1capacity{index}"), 1),
                    None,
                    now,
                    SystemTime::UNIX_EPOCH,
                    1,
                )
                .expect("resolution within capacity should be created");
        }
        let newest = store
            .create_session(
                sample_resolution("BV1newest", 1),
                None,
                now,
                SystemTime::UNIX_EPOCH,
                1,
            )
            .expect("new resolution should evict the oldest snapshot");

        let error = store
            .page(&first.session.id, None, now, 1)
            .expect_err("the oldest snapshot should be evicted");
        assert_eq!(tonic::Code::NotFound, error.code());
        assert_eq!(vec![1], candidate_indexes(&newest));
    }

    #[test]
    fn rejects_incomplete_candidates_and_unknown_defaults() {
        let mut incomplete = sample_resolution("BV1incomplete", 1);
        incomplete.candidates[0].identity.cid = None;
        let error = BilibiliResolutionStore::default()
            .create_session(incomplete, None, Instant::now(), SystemTime::UNIX_EPOCH, 1)
            .expect_err("incomplete stable identity must fail before publication");
        assert_eq!(tonic::Code::FailedPrecondition, error.code());

        let mut mismatched = sample_resolution("BV1mismatched", 1);
        mismatched.candidates[0].identity.cid = Some(9_999);
        let error = BilibiliResolutionStore::default()
            .create_session(mismatched, None, Instant::now(), SystemTime::UNIX_EPOCH, 1)
            .expect_err("typed identity must match the candidate content id");
        assert_eq!(tonic::Code::FailedPrecondition, error.code());

        let mut unknown_default = sample_resolution("BV1unknown-default", 1);
        unknown_default.default_selection_id = "missing-selection".to_owned();
        let error = BilibiliResolutionStore::default()
            .create_session(
                unknown_default,
                None,
                Instant::now(),
                SystemTime::UNIX_EPOCH,
                1,
            )
            .expect_err("default selection must belong to the immutable snapshot");
        assert_eq!(tonic::Code::FailedPrecondition, error.code());
    }

    #[test]
    fn accepted_task_values_do_not_retain_provider_cover_urls() {
        let now = Instant::now();
        let mut resolution = sample_resolution("BV1cover", 1);
        resolution.candidates[0].cover_uri =
            "https://provider.invalid/private-cover?credential=must-not-retain".to_owned();
        let mut store = BilibiliResolutionStore::default();
        let page = store
            .create_session(resolution, None, now, SystemTime::UNIX_EPOCH, 1)
            .expect("resolution should be created");
        assert_eq!(
            0,
            store
                .sessions_by_id
                .get(&page.session.id)
                .expect("resolution snapshot should remain available")
                .candidates[0]
                .candidate
                .cover_uri
                .capacity(),
            "discarded provider cover storage must not bypass snapshot accounting"
        );
        let accepted = store
            .accept_selection(
                &page.session.id,
                &BilibiliResolutionSelection {
                    mode: BilibiliResolutionSelectionMode::All.into(),
                    ..Default::default()
                },
                now,
            )
            .expect("selection should be accepted");

        assert!(accepted.candidates[0].cover_uri.is_empty());
    }

    fn sample_resolution(source: &str, count: u32) -> BilibiliInputResolution {
        BilibiliInputResolution {
            source: source.to_owned(),
            title: "Snapshot video".to_owned(),
            source_kind: "video".to_owned(),
            candidates: (1..=count).map(sample_candidate).collect(),
            default_selection_id: if count == 1 {
                "page:1:cid:1001:bvid:BV1snapshot:aid:2001".to_owned()
            } else {
                String::new()
            },
            candidates_truncated: false,
        }
    }

    fn sample_candidate(index: u32) -> BilibiliResolvedCandidate {
        BilibiliResolvedCandidate {
            selection_id: format!(
                "page:{index}:cid:{}:bvid:BV1snapshot:aid:{}",
                1_000 + index,
                2_000 + index
            ),
            title: format!("Part {index}"),
            subtitle: format!("Page {index}"),
            source_kind: "video_page".to_owned(),
            content_id: (1_000 + index).to_string(),
            identity: BilibiliContentIdentity {
                kind: BilibiliContentKind::VideoPage,
                aid: Some((2_000 + index).into()),
                bvid: Some("BV1snapshot".to_owned()),
                cid: Some((1_000 + index).into()),
                epid: None,
            },
            index,
            duration_seconds: Some(60),
            cover_uri: "https://provider.invalid/cover.jpg".to_owned(),
        }
    }

    fn candidate_indexes(page: &BilibiliResolutionPageView) -> Vec<u32> {
        page.candidates
            .iter()
            .map(|candidate| candidate.candidate.index)
            .collect()
    }

    fn accepted_indexes(resolution: &AcceptedBilibiliResolution) -> Vec<u32> {
        resolution
            .candidates
            .iter()
            .map(|candidate| candidate.index)
            .collect()
    }
}
