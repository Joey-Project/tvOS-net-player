use std::{
    collections::{HashMap, HashSet},
    io,
    ops::Range,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, watch};

pub(crate) const RANGE_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub(crate) const RANGE_FOREGROUND_CHUNK_BYTES: u64 = 64 * 1024;
pub(crate) const RANGE_STARTUP_CHUNK_BYTES: u64 = 512 * 1024;
pub(crate) const RANGE_MIN_CHUNK_BYTES: u64 = 512 * 1024;
pub(crate) const RANGE_MAX_CHUNK_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const RANGE_MAX_CHUNKS: u64 = 8192;
pub(crate) const RANGE_MAX_SIZE: u64 = RANGE_STARTUP_CHUNK_BYTES
    + RANGE_MAX_CHUNK_BYTES.saturating_mul(RANGE_MAX_CHUNKS.saturating_sub(1));
pub(crate) const RANGE_MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedRangeManifest {
    pub(crate) schema_version: u32,
    pub(crate) generation: u64,
    pub(crate) resource_id: String,
    pub(crate) representation_digest: String,
    pub(crate) data_identity: Option<String>,
    pub(crate) total_length: Option<u64>,
    pub(crate) strong_etag: Option<String>,
    pub(crate) validator_origin: Option<String>,
    pub(crate) last_modified: Option<String>,
    pub(crate) prefix_length: u64,
    pub(crate) prefix_sha256: Option<String>,
    pub(crate) durable_bytes: u64,
    #[serde(default)]
    pub(crate) validated_origins: Vec<PersistedRangeOrigin>,
    pub(crate) extents: Vec<PersistedRangeExtent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedRangeOrigin {
    pub(crate) origin: String,
    pub(crate) prefix_sha256: String,
    pub(crate) strong_etag: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedRangeExtent {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HlsRangePriority {
    Foreground,
    PlaybackPreparation,
    FarFuturePlayback,
    Background,
}

pub(crate) type HlsRangePriorityResolver =
    Arc<dyn Fn(&Range<u64>, Duration) -> HlsRangePriority + Send + Sync>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionPublicationMode {
    Fresh,
    Refresh,
    OwnedCompletion,
    OwnedRestoration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsReadyRange {
    pub(crate) requested: Range<u64>,
    pub(crate) total_length: u64,
    pub(crate) strong_etag: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsRangeReadiness {
    pub(crate) accepted: Range<u64>,
    pub(crate) total_length: u64,
    pub(crate) strong_etag: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsRangePreparation {
    pub(crate) prepared_span: Option<Range<u64>>,
    pub(crate) total_length: u64,
    pub(crate) strong_etag: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum HlsRangeResourceStatus {
    Partial {
        total_length: u64,
        durable_bytes: u64,
        missing_ranges: Vec<Range<u64>>,
    },
    Complete {
        total_length: u64,
    },
}

pub(crate) enum HlsRangeError {
    Io(io::Error),
    Network(reqwest::Error),
    UpstreamStatus(StatusCode),
    RangeUnsupported,
    InvalidResponse(String),
    IdentityChanged,
    QuotaExceeded,
    Cancelled,
    Preempted,
    SessionRemoving,
}

impl std::fmt::Debug for HlsRangeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => formatter.debug_tuple("Io").field(&error.kind()).finish(),
            Self::Network(error) => formatter
                .debug_tuple("Network")
                .field(&error.to_string())
                .finish(),
            Self::UpstreamStatus(status) => formatter
                .debug_tuple("UpstreamStatus")
                .field(status)
                .finish(),
            Self::RangeUnsupported => formatter.write_str("RangeUnsupported"),
            Self::InvalidResponse(_) => formatter.write_str("InvalidResponse"),
            Self::IdentityChanged => formatter.write_str("IdentityChanged"),
            Self::QuotaExceeded => formatter.write_str("QuotaExceeded"),
            Self::Cancelled => formatter.write_str("Cancelled"),
            Self::Preempted => formatter.write_str("Preempted"),
            Self::SessionRemoving => formatter.write_str("SessionRemoving"),
        }
    }
}

impl std::fmt::Display for HlsRangeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "range cache I/O error ({})", error.kind()),
            Self::Network(error) => write!(formatter, "range cache network error: {error}"),
            Self::UpstreamStatus(status) => write!(formatter, "upstream returned {status}"),
            Self::RangeUnsupported => formatter.write_str("upstream does not support byte ranges"),
            Self::InvalidResponse(_) => formatter.write_str("invalid upstream range response"),
            Self::IdentityChanged => formatter.write_str("HLS range representation changed"),
            Self::QuotaExceeded => formatter.write_str("HLS range cache quota exceeded"),
            Self::Cancelled => formatter.write_str("HLS range fill was cancelled"),
            Self::Preempted => formatter.write_str("HLS range fill was preempted"),
            Self::SessionRemoving => formatter.write_str("HLS cache session is being removed"),
        }
    }
}

impl std::error::Error for HlsRangeError {}

impl From<io::Error> for HlsRangeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<reqwest::Error> for HlsRangeError {
    fn from(error: reqwest::Error) -> Self {
        Self::Network(error.without_url())
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RangeResourceKey {
    pub(crate) session_id: String,
    pub(crate) resource_id: String,
    pub(crate) representation: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RangeChunkKey {
    pub(crate) resource: RangeResourceKey,
    pub(crate) total_length: u64,
    pub(crate) range: Range<u64>,
}

#[derive(Default)]
struct RangeState {
    active: usize,
    active_sessions: HashMap<String, usize>,
    active_foreground: usize,
    active_foreground_permits: usize,
    waiting_foreground: usize,
    waiting_preparation: usize,
    active_preparation_batches: usize,
    active_preparation_transfers: usize,
    active_chunk_permits: usize,
    removing: HashSet<String>,
    retired: HashSet<String>,
    unpublished: HashSet<String>,
    published: HashSet<String>,
    shutting_down: bool,
    flights: HashMap<RangeChunkKey, FlightState>,
    reservations: HashMap<RangeResourceKey, HashMap<u64, RangeReservation>>,
    next_reservation: u64,
}

struct FlightState {
    completion: watch::Sender<bool>,
    readiness: watch::Sender<Option<HlsRangeReadiness>>,
    foreground_interest: usize,
    preparation_interest: usize,
    promoted_preparation_permit: bool,
}

#[derive(Clone, Copy)]
struct RangeReservation {
    target_length: u64,
    current_length: u64,
}

pub(crate) struct HlsRangeCache {
    state: Mutex<RangeState>,
    changed: watch::Sender<u64>,
    chunk_permits: Arc<Semaphore>,
    parallelism: usize,
    quota_bytes: u64,
}

impl HlsRangeCache {
    pub(crate) fn new(parallelism: usize, quota_bytes: u64) -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            state: Mutex::new(RangeState::default()),
            changed,
            chunk_permits: Arc::new(Semaphore::new(parallelism)),
            parallelism,
            quota_bytes,
        }
    }

    pub(crate) fn parallelism(&self) -> usize {
        self.parallelism
    }

    pub(crate) fn quota_bytes(&self) -> u64 {
        self.quota_bytes
    }

    pub(crate) fn begin_session_publication(
        self: &Arc<Self>,
        session_id: &str,
        mode: SessionPublicationMode,
    ) -> Result<SessionPublicationGuard, HlsRangeError> {
        let mut state = self.lock_state();
        if state.shutting_down
            || state.removing.contains(session_id)
            || state.retired.contains(session_id)
        {
            return Err(HlsRangeError::SessionRemoving);
        }
        match mode {
            SessionPublicationMode::Fresh => {
                if state.published.contains(session_id)
                    || state
                        .active_sessions
                        .get(session_id)
                        .copied()
                        .unwrap_or_default()
                        != 0
                {
                    return Err(HlsRangeError::SessionRemoving);
                }
                state.unpublished.insert(session_id.to_owned());
            }
            SessionPublicationMode::Refresh
            | SessionPublicationMode::OwnedCompletion
            | SessionPublicationMode::OwnedRestoration => {
                if state.unpublished.contains(session_id) {
                    return Err(HlsRangeError::SessionRemoving);
                }
            }
        }
        state.active += 1;
        *state
            .active_sessions
            .entry(session_id.to_owned())
            .or_default() += 1;
        Ok(SessionPublicationGuard {
            cache: Arc::clone(self),
            session_id: session_id.to_owned(),
            mode,
            committed: false,
        })
    }

    pub(crate) fn try_begin_session_removal(
        self: &Arc<Self>,
        session_id: &str,
    ) -> Result<Option<HlsSessionRemovalGuard>, HlsRangeError> {
        let mut state = self.lock_state();
        if state.shutting_down || state.removing.contains(session_id) {
            return Err(HlsRangeError::SessionRemoving);
        }
        if state
            .active_sessions
            .get(session_id)
            .copied()
            .unwrap_or_default()
            != 0
        {
            return Ok(None);
        }
        state.removing.insert(session_id.to_owned());
        drop(state);
        self.signal_change();
        Ok(Some(HlsSessionRemovalGuard {
            cache: Arc::clone(self),
            session_id: session_id.to_owned(),
            committed: false,
        }))
    }

    pub(crate) fn range_activity_counts(&self) -> (usize, usize) {
        let state = self.lock_state();
        (
            state.active,
            state.reservations.values().map(HashMap::len).sum(),
        )
    }

    pub(crate) fn active_flight_count(&self, key: &RangeResourceKey) -> usize {
        self.lock_state()
            .flights
            .keys()
            .filter(|flight| &flight.resource == key)
            .count()
    }

    #[cfg(test)]
    pub(crate) fn has_flight_overlapping_for_test(
        &self,
        session_id: &str,
        resource_id: &str,
        requested: &Range<u64>,
    ) -> bool {
        self.lock_state().flights.keys().any(|flight| {
            flight.resource.session_id == session_id
                && flight.resource.resource_id == resource_id
                && flight.range.start < requested.end
                && requested.start < flight.range.end
        })
    }

    pub(crate) fn enter(
        self: &Arc<Self>,
        session_id: &str,
        priority: HlsRangePriority,
    ) -> Result<RangeActivity, HlsRangeError> {
        let mut state = self.lock_state();
        if state.shutting_down
            || state.removing.contains(session_id)
            || state.retired.contains(session_id)
            || state.unpublished.contains(session_id)
        {
            return Err(HlsRangeError::SessionRemoving);
        }
        state.active += 1;
        if priority == HlsRangePriority::Foreground {
            state.active_foreground += 1;
        } else if priority == HlsRangePriority::PlaybackPreparation {
            state.active_preparation_batches += 1;
        }
        *state
            .active_sessions
            .entry(session_id.to_owned())
            .or_default() += 1;
        drop(state);
        Ok(RangeActivity {
            cache: Arc::clone(self),
            session_id: session_id.to_owned(),
            priority,
        })
    }

    pub(crate) fn claim_chunk(
        self: &Arc<Self>,
        key: RangeChunkKey,
        priority: HlsRangePriority,
    ) -> Result<RangeFlight, HlsRangeError> {
        let mut state = self.lock_state();
        if state.shutting_down
            || state.removing.contains(&key.resource.session_id)
            || state.retired.contains(&key.resource.session_id)
            || state.unpublished.contains(&key.resource.session_id)
        {
            return Err(HlsRangeError::SessionRemoving);
        }
        let (completion, receiver, readiness, readiness_receiver, owner) =
            match state.flights.get_mut(&key) {
                Some(flight) => {
                    if priority == HlsRangePriority::Foreground {
                        flight.foreground_interest += 1;
                    } else if priority == HlsRangePriority::PlaybackPreparation {
                        flight.preparation_interest += 1;
                    }
                    (
                        flight.completion.clone(),
                        flight.completion.subscribe(),
                        flight.readiness.clone(),
                        flight.readiness.subscribe(),
                        false,
                    )
                }
                None => {
                    let (completion, receiver) = watch::channel(false);
                    let (readiness, readiness_receiver) = watch::channel(None);
                    state.flights.insert(
                        key.clone(),
                        FlightState {
                            completion: completion.clone(),
                            readiness: readiness.clone(),
                            foreground_interest: if priority == HlsRangePriority::Foreground {
                                1
                            } else {
                                0
                            },
                            preparation_interest: 0,
                            promoted_preparation_permit: false,
                        },
                    );
                    (completion, receiver, readiness, readiness_receiver, true)
                }
            };
        let flight = RangeFlight {
            cache: Arc::clone(self),
            key,
            completion,
            receiver,
            readiness,
            readiness_receiver,
            owner,
            foreground_interest: !owner && priority == HlsRangePriority::Foreground,
            preparation_interest: !owner && priority == HlsRangePriority::PlaybackPreparation,
        };
        drop(state);
        if priority == HlsRangePriority::Foreground && !owner {
            self.signal_change();
        }
        Ok(flight)
    }

    pub(crate) async fn acquire_chunk_permit<F>(
        self: &Arc<Self>,
        key: &RangeChunkKey,
        priority: HlsRangePriority,
        control: &F,
    ) -> Result<HlsRangeChunkPermit, HlsRangeError>
    where
        F: Fn() -> crate::hls_cache::HlsCacheFillControl + Send + Sync,
    {
        let owner_completion = if priority == HlsRangePriority::PlaybackPreparation {
            self.lock_state()
                .flights
                .get(key)
                .map(|flight| flight.completion.clone())
        } else {
            None
        };
        self.check_priority_control_with_waiter(
            key,
            priority,
            control,
            false,
            owner_completion.as_ref(),
            priority == HlsRangePriority::PlaybackPreparation,
        )?;
        if priority == HlsRangePriority::PlaybackPreparation {
            let mut preparation_waiter = None;
            let mut foreground_waiter = None;
            loop {
                self.check_priority_control_with_waiter(
                    key,
                    priority,
                    control,
                    foreground_waiter.is_some(),
                    owner_completion.as_ref(),
                    true,
                )?;
                if let Some(permit) = self.try_acquire_preparation_chunk_permit_for_owner(
                    key,
                    owner_completion.as_ref(),
                    foreground_waiter.is_some(),
                ) {
                    drop(preparation_waiter.take());
                    drop(foreground_waiter.take());
                    return Ok(permit);
                }
                if self.flight_has_foreground_interest_for_owner(key, owner_completion.as_ref()) {
                    drop(preparation_waiter.take());
                    if foreground_waiter.is_none() {
                        foreground_waiter = Some(self.register_foreground_waiter());
                    }
                    let promoted_permit = {
                        let mut changed = self.changed.subscribe();
                        let acquire = self.chunk_permits.clone().acquire_owned();
                        tokio::pin!(acquire);
                        loop {
                            self.check_priority_control_with_waiter(
                                key,
                                priority,
                                control,
                                true,
                                owner_completion.as_ref(),
                                true,
                            )?;
                            if !self.flight_has_foreground_interest_for_owner(
                                key,
                                owner_completion.as_ref(),
                            ) {
                                break None;
                            }
                            tokio::select! {
                                permit = &mut acquire => {
                                    let permit = permit.map_err(|_| HlsRangeError::SessionRemoving)?;
                                    self.check_priority_control_with_waiter(
                                        key,
                                        priority,
                                        control,
                                        true,
                                        owner_completion.as_ref(),
                                        true,
                                    )?;
                                    break self.track_promoted_preparation_chunk_permit(
                                        key,
                                        owner_completion.as_ref(),
                                        permit,
                                    );
                                }
                                changed_result = changed.changed() => {
                                    if changed_result.is_err() {
                                        return Err(HlsRangeError::SessionRemoving);
                                    }
                                }
                                () = tokio::time::sleep(Duration::from_millis(50)) => {}
                            }
                        }
                    };
                    if let Some(permit) = promoted_permit {
                        drop(foreground_waiter.take());
                        return Ok(permit);
                    }
                    drop(foreground_waiter.take());
                    continue;
                } else {
                    drop(foreground_waiter.take());
                    if preparation_waiter.is_none() {
                        preparation_waiter = Some(self.register_preparation_waiter());
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        let mut foreground_waiter = None;
        if priority == HlsRangePriority::Foreground {
            if let Some(permit) = self.try_acquire_chunk_permit(priority) {
                return Ok(permit);
            }
            foreground_waiter = Some(self.register_foreground_waiter());
        }

        let acquire = self.chunk_permits.clone().acquire_owned();
        tokio::pin!(acquire);
        loop {
            self.check_priority_control(key, priority, control)?;
            tokio::select! {
                permit = &mut acquire => {
                    let permit = permit.map_err(|_| HlsRangeError::SessionRemoving)?;
                    let permit = self.track_chunk_permit(priority, permit);
                    drop(foreground_waiter.take());
                    return Ok(permit);
                }
                () = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
    }

    pub(crate) async fn acquire_chunk_permit_with_resolver<F>(
        self: &Arc<Self>,
        key: &RangeChunkKey,
        priority_for: &HlsRangePriorityResolver,
        pending_since: Instant,
        control: &F,
    ) -> Result<HlsRangeChunkPermit, HlsRangeError>
    where
        F: Fn() -> crate::hls_cache::HlsCacheFillControl + Send + Sync,
    {
        let mut foreground_waiter = None;
        let mut changed = self.changed.subscribe();
        loop {
            let priority = self.resolve_priority_for_chunk(key, priority_for, pending_since);
            self.check_priority_control(key, priority, control)?;

            match priority {
                HlsRangePriority::Foreground => {
                    if foreground_waiter.is_none() {
                        foreground_waiter = Some(self.register_foreground_waiter());
                    }
                    if let Some(permit) =
                        self.try_acquire_scheduled_chunk_permit(HlsRangePriority::Foreground, true)
                    {
                        drop(foreground_waiter.take());
                        return Ok(permit);
                    }
                }
                HlsRangePriority::FarFuturePlayback => {
                    drop(foreground_waiter.take());
                    if let Some(permit) = self.try_acquire_scheduled_chunk_permit(
                        HlsRangePriority::FarFuturePlayback,
                        false,
                    ) {
                        return Ok(permit);
                    }
                }
                HlsRangePriority::PlaybackPreparation | HlsRangePriority::Background => {
                    return self.acquire_chunk_permit(key, priority, control).await;
                }
            }

            tokio::select! {
                changed_result = changed.changed() => {
                    if changed_result.is_err() {
                        return Err(HlsRangeError::SessionRemoving);
                    }
                }
                () = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
    }

    fn try_acquire_scheduled_chunk_permit(
        self: &Arc<Self>,
        priority: HlsRangePriority,
        owns_foreground_waiter: bool,
    ) -> Option<HlsRangeChunkPermit> {
        let mut state = self.lock_state();
        let other_foreground_waiters = state
            .waiting_foreground
            .saturating_sub(usize::from(owns_foreground_waiter));
        if priority == HlsRangePriority::Foreground && other_foreground_waiters > 0 {
            return None;
        }
        if priority == HlsRangePriority::FarFuturePlayback
            && (state.waiting_foreground > 0
                || state.waiting_preparation > 0
                || state.active_foreground > 0
                || state.active_foreground_permits > 0
                || state.active_preparation_transfers > 0)
        {
            return None;
        }
        if priority == HlsRangePriority::FarFuturePlayback && self.parallelism >= 2 {
            let foreground_capacity = self
                .chunk_permits
                .available_permits()
                .saturating_add(state.active_foreground_permits);
            if foreground_capacity <= 1 {
                return None;
            }
        }
        let permit = Arc::clone(&self.chunk_permits).try_acquire_owned().ok()?;
        state.active_chunk_permits += 1;
        match priority {
            HlsRangePriority::Foreground => state.active_foreground_permits += 1,
            HlsRangePriority::PlaybackPreparation => state.active_preparation_transfers += 1,
            HlsRangePriority::FarFuturePlayback | HlsRangePriority::Background => {}
        }
        drop(state);
        Some(HlsRangeChunkPermit {
            _permit: permit,
            cache: Arc::clone(self),
            priority,
            promoted_preparation_flight: None,
        })
    }

    fn flight_has_foreground_interest(&self, key: &RangeChunkKey) -> bool {
        self.lock_state()
            .flights
            .get(key)
            .is_some_and(|flight| flight.foreground_interest > 0)
    }

    pub(crate) fn resolve_priority_for_chunk(
        &self,
        key: &RangeChunkKey,
        priority_for: &HlsRangePriorityResolver,
        pending_since: Instant,
    ) -> HlsRangePriority {
        let waited = Instant::now().saturating_duration_since(pending_since);
        let priority = priority_for(&key.range, waited);
        if priority != HlsRangePriority::Foreground && self.flight_has_foreground_interest(key) {
            HlsRangePriority::Foreground
        } else {
            priority
        }
    }

    fn try_acquire_chunk_permit(
        self: &Arc<Self>,
        priority: HlsRangePriority,
    ) -> Option<HlsRangeChunkPermit> {
        let mut state = self.lock_state();
        if priority == HlsRangePriority::Foreground && state.waiting_foreground > 0 {
            return None;
        }
        if priority == HlsRangePriority::PlaybackPreparation {
            let foreground_capacity = self
                .chunk_permits
                .available_permits()
                .saturating_add(state.active_foreground_permits);
            if state.waiting_foreground > 0
                || state.active_preparation_transfers > 0
                || (self.parallelism >= 2 && foreground_capacity <= 1)
            {
                return None;
            }
        }
        if priority == HlsRangePriority::FarFuturePlayback {
            let foreground_capacity = self
                .chunk_permits
                .available_permits()
                .saturating_add(state.active_foreground_permits);
            if state.waiting_foreground > 0
                || state.waiting_preparation > 0
                || state.active_foreground > 0
                || state.active_preparation_transfers > 0
                || (self.parallelism >= 2 && foreground_capacity <= 1)
            {
                return None;
            }
        }
        let permit = Arc::clone(&self.chunk_permits).try_acquire_owned().ok()?;
        state.active_chunk_permits += 1;
        match priority {
            HlsRangePriority::Foreground => state.active_foreground_permits += 1,
            HlsRangePriority::PlaybackPreparation => state.active_preparation_transfers += 1,
            HlsRangePriority::FarFuturePlayback | HlsRangePriority::Background => {}
        }
        drop(state);
        Some(HlsRangeChunkPermit {
            _permit: permit,
            cache: Arc::clone(self),
            priority,
            promoted_preparation_flight: None,
        })
    }

    fn try_acquire_preparation_chunk_permit_for_owner(
        self: &Arc<Self>,
        key: &RangeChunkKey,
        owner_completion: Option<&watch::Sender<bool>>,
        owns_foreground_waiter: bool,
    ) -> Option<HlsRangeChunkPermit> {
        let mut state = self.lock_state();
        let owned_foreground_waiters = if owns_foreground_waiter { 1 } else { 0 };
        let flight_matches = |flight: &FlightState| {
            owner_completion.is_some_and(|completion| flight.completion.same_channel(completion))
        };
        let matching_flight = state
            .flights
            .get(key)
            .filter(|flight| flight_matches(flight));
        if matching_flight.is_some_and(|flight| flight.promoted_preparation_permit) {
            return None;
        }
        let promoted = matching_flight.is_some_and(|flight| flight.foreground_interest > 0);
        let priority = if promoted {
            if state.waiting_foreground > owned_foreground_waiters {
                return None;
            }
            HlsRangePriority::Foreground
        } else {
            let foreground_capacity = self
                .chunk_permits
                .available_permits()
                .saturating_add(state.active_foreground_permits);
            if state.waiting_foreground > owned_foreground_waiters
                || state.active_preparation_transfers > 0
                || (self.parallelism >= 2 && foreground_capacity <= 1)
            {
                return None;
            }
            HlsRangePriority::PlaybackPreparation
        };
        let permit = Arc::clone(&self.chunk_permits).try_acquire_owned().ok()?;
        state.active_chunk_permits += 1;
        match priority {
            HlsRangePriority::Foreground => state.active_foreground_permits += 1,
            HlsRangePriority::PlaybackPreparation => state.active_preparation_transfers += 1,
            HlsRangePriority::FarFuturePlayback => {}
            HlsRangePriority::Background => unreachable!(),
        }
        let promoted_preparation_flight = if promoted {
            let flight = state
                .flights
                .get_mut(key)
                .filter(|flight| flight_matches(flight))
                .expect("promoted preparation flight should remain registered");
            flight.promoted_preparation_permit = true;
            Some((key.clone(), flight.completion.clone()))
        } else {
            None
        };
        drop(state);
        Some(HlsRangeChunkPermit {
            _permit: permit,
            cache: Arc::clone(self),
            priority,
            promoted_preparation_flight,
        })
    }

    fn track_promoted_preparation_chunk_permit(
        self: &Arc<Self>,
        key: &RangeChunkKey,
        owner_completion: Option<&watch::Sender<bool>>,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Option<HlsRangeChunkPermit> {
        let mut state = self.lock_state();
        let flight = state.flights.get_mut(key).filter(|flight| {
            owner_completion.is_some_and(|completion| flight.completion.same_channel(completion))
                && flight.foreground_interest > 0
                && !flight.promoted_preparation_permit
        })?;
        flight.promoted_preparation_permit = true;
        let promoted_preparation_flight = Some((key.clone(), flight.completion.clone()));
        state.active_chunk_permits += 1;
        state.active_foreground_permits += 1;
        drop(state);
        Some(HlsRangeChunkPermit {
            _permit: permit,
            cache: Arc::clone(self),
            priority: HlsRangePriority::Foreground,
            promoted_preparation_flight,
        })
    }

    fn flight_has_foreground_interest_for_owner(
        &self,
        key: &RangeChunkKey,
        owner_completion: Option<&watch::Sender<bool>>,
    ) -> bool {
        self.lock_state().flights.get(key).is_some_and(|flight| {
            owner_completion.is_some_and(|completion| {
                flight.completion.same_channel(completion) && flight.foreground_interest > 0
            })
        })
    }

    fn track_chunk_permit(
        self: &Arc<Self>,
        priority: HlsRangePriority,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> HlsRangeChunkPermit {
        let mut state = self.lock_state();
        state.active_chunk_permits += 1;
        match priority {
            HlsRangePriority::Foreground => state.active_foreground_permits += 1,
            HlsRangePriority::PlaybackPreparation => state.active_preparation_transfers += 1,
            HlsRangePriority::FarFuturePlayback | HlsRangePriority::Background => {}
        }
        drop(state);
        HlsRangeChunkPermit {
            _permit: permit,
            cache: Arc::clone(self),
            priority,
            promoted_preparation_flight: None,
        }
    }

    fn register_foreground_waiter(self: &Arc<Self>) -> ForegroundWaiter {
        self.lock_state().waiting_foreground += 1;
        self.signal_change();
        ForegroundWaiter {
            cache: Arc::clone(self),
        }
    }

    fn register_preparation_waiter(self: &Arc<Self>) -> PreparationWaiter {
        self.lock_state().waiting_preparation += 1;
        self.signal_change();
        PreparationWaiter {
            cache: Arc::clone(self),
        }
    }

    pub(crate) fn check_priority_control<F>(
        &self,
        key: &RangeChunkKey,
        priority: HlsRangePriority,
        control: &F,
    ) -> Result<(), HlsRangeError>
    where
        F: Fn() -> crate::hls_cache::HlsCacheFillControl + Send + Sync,
    {
        self.check_priority_control_with_waiter(key, priority, control, false, None, false)
    }

    fn check_priority_control_with_waiter<F>(
        &self,
        key: &RangeChunkKey,
        priority: HlsRangePriority,
        control: &F,
        owns_foreground_waiter: bool,
        owner_completion: Option<&watch::Sender<bool>>,
        require_owner_identity: bool,
    ) -> Result<(), HlsRangeError>
    where
        F: Fn() -> crate::hls_cache::HlsCacheFillControl + Send + Sync,
    {
        let state = self.lock_state();
        if state.shutting_down
            || state.removing.contains(&key.resource.session_id)
            || state.retired.contains(&key.resource.session_id)
            || state.unpublished.contains(&key.resource.session_id)
        {
            return Err(HlsRangeError::SessionRemoving);
        }
        let flight = state.flights.get(key).filter(|flight| {
            !require_owner_identity
                || owner_completion
                    .is_some_and(|completion| flight.completion.same_channel(completion))
        });
        let promoted = flight.is_some_and(|flight| flight.foreground_interest > 0);
        let preparation_interest = flight.is_some_and(|flight| flight.preparation_interest > 0);
        let promoted_preparation_permit =
            flight.is_some_and(|flight| flight.promoted_preparation_permit);
        if priority == HlsRangePriority::Background {
            let foreground_active =
                state.active_foreground > 0 || state.active_foreground_permits > 0;
            let foreground_waiting = state.waiting_foreground > 0;
            let preparation_active = state.active_preparation_batches > 0;
            drop(state);
            match control() {
                crate::hls_cache::HlsCacheFillControl::Cancel => {
                    return Err(HlsRangeError::Cancelled);
                }
                crate::hls_cache::HlsCacheFillControl::Preempt
                    if !promoted || (!foreground_active && !preparation_active) =>
                {
                    return Err(HlsRangeError::Preempted);
                }
                crate::hls_cache::HlsCacheFillControl::Continue
                | crate::hls_cache::HlsCacheFillControl::Preempt => {}
            }
            if !promoted
                && (foreground_active
                    || foreground_waiting
                    || (preparation_active && !preparation_interest))
            {
                return Err(HlsRangeError::Preempted);
            }
            return Ok(());
        }
        if priority == HlsRangePriority::PlaybackPreparation {
            let owned_foreground_waiters = if owns_foreground_waiter { 1 } else { 0 };
            let foreground_waiting = state.waiting_foreground > owned_foreground_waiters;
            drop(state);
            match control() {
                crate::hls_cache::HlsCacheFillControl::Cancel => {
                    return Err(HlsRangeError::Cancelled);
                }
                crate::hls_cache::HlsCacheFillControl::Preempt if !promoted => {
                    return Err(HlsRangeError::Preempted);
                }
                crate::hls_cache::HlsCacheFillControl::Continue
                | crate::hls_cache::HlsCacheFillControl::Preempt => {}
            }
            if !promoted && (foreground_waiting || promoted_preparation_permit) {
                return Err(HlsRangeError::Preempted);
            }
            return Ok(());
        }
        drop(state);
        match control() {
            crate::hls_cache::HlsCacheFillControl::Cancel => Err(HlsRangeError::Cancelled),
            crate::hls_cache::HlsCacheFillControl::Preempt => Err(HlsRangeError::Preempted),
            crate::hls_cache::HlsCacheFillControl::Continue => Ok(()),
        }
    }

    pub(crate) fn reserve(
        self: &Arc<Self>,
        key: RangeResourceKey,
        target_logical_length: u64,
        current_file_length: u64,
        managed_usage_bytes: u64,
    ) -> Result<RangeQuotaLease, HlsRangeError> {
        let mut state = self.lock_state();
        if state.shutting_down
            || state.removing.contains(&key.session_id)
            || state.retired.contains(&key.session_id)
            || state.unpublished.contains(&key.session_id)
        {
            return Err(HlsRangeError::SessionRemoving);
        }

        let existing_additional = state
            .reservations
            .iter()
            .map(|(resource, reservations)| {
                let largest_target = reservations
                    .values()
                    .map(|reservation| reservation.target_length)
                    .max()
                    .unwrap_or_default();
                let observed_length = reservations
                    .values()
                    .map(|reservation| reservation.current_length)
                    .max()
                    .unwrap_or_default();
                let current_length = if resource == &key {
                    current_file_length.max(observed_length)
                } else {
                    observed_length
                };
                largest_target.saturating_sub(current_length)
            })
            .fold(0_u64, u64::saturating_add);
        let old_target = state
            .reservations
            .get(&key)
            .and_then(|reservations| {
                reservations
                    .values()
                    .map(|reservation| reservation.target_length)
                    .max()
            })
            .unwrap_or_default();
        let target = target_logical_length.max(old_target);
        let previous_current = state
            .reservations
            .get(&key)
            .into_iter()
            .flat_map(|reservations| reservations.values())
            .map(|reservation| reservation.current_length)
            .max()
            .unwrap_or_default();
        let current_for_key = current_file_length.max(previous_current);
        let old_additional = old_target.saturating_sub(current_for_key);
        let new_additional = target.saturating_sub(current_for_key);
        let projected = managed_usage_bytes
            .saturating_add(existing_additional.saturating_sub(old_additional))
            .saturating_add(new_additional);
        if self.quota_bytes > 0 && projected > self.quota_bytes {
            return Err(HlsRangeError::QuotaExceeded);
        }

        state.next_reservation = state.next_reservation.wrapping_add(1).max(1);
        let id = state.next_reservation;
        state.reservations.entry(key.clone()).or_default().insert(
            id,
            RangeReservation {
                target_length: target,
                current_length: current_for_key,
            },
        );
        Ok(RangeQuotaLease {
            cache: Arc::clone(self),
            key,
            id,
        })
    }

    pub(crate) async fn begin_session_removal(
        self: &Arc<Self>,
        session_id: &str,
    ) -> Result<HlsSessionRemovalGuard, HlsRangeError> {
        {
            let mut state = self.lock_state();
            if state.shutting_down || state.removing.contains(session_id) {
                return Err(HlsRangeError::SessionRemoving);
            }
            state.removing.insert(session_id.to_owned());
        }
        self.signal_change();

        let mut changed = self.changed.subscribe();
        loop {
            let idle = self
                .lock_state()
                .active_sessions
                .get(session_id)
                .copied()
                .unwrap_or_default()
                == 0;
            if idle {
                return Ok(HlsSessionRemovalGuard {
                    cache: Arc::clone(self),
                    session_id: session_id.to_owned(),
                    committed: false,
                });
            }
            if changed.changed().await.is_err() {
                return Err(HlsRangeError::SessionRemoving);
            }
        }
    }

    pub(crate) async fn drain(&self) {
        self.lock_state().shutting_down = true;
        self.signal_change();
        let mut changed = self.changed.subscribe();
        loop {
            if self.lock_state().active == 0 {
                return;
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }

    pub(crate) fn note_file_length(&self, key: &RangeResourceKey, file_length: u64) {
        if let Some(reservations) = self.lock_state().reservations.get_mut(key) {
            for reservation in reservations.values_mut() {
                reservation.current_length = reservation.current_length.max(file_length);
            }
        }
    }

    pub(crate) fn release_foreground_interest(
        &self,
        key: &RangeChunkKey,
        completion: &watch::Sender<bool>,
    ) {
        let mut state = self.lock_state();
        let released = state
            .flights
            .get_mut(key)
            .filter(|flight| flight.completion.same_channel(completion))
            .is_some_and(|flight| {
                flight.foreground_interest = flight.foreground_interest.saturating_sub(1);
                true
            });
        drop(state);
        if released {
            self.signal_change();
        }
    }

    fn release_preparation_interest(&self, key: &RangeChunkKey) {
        if let Some(flight) = self.lock_state().flights.get_mut(key) {
            flight.preparation_interest = flight.preparation_interest.saturating_sub(1);
        }
    }

    fn finish_activity(&self, session_id: &str, priority: HlsRangePriority) {
        let mut state = self.lock_state();
        state.active = state.active.saturating_sub(1);
        if let Some(active) = state.active_sessions.get_mut(session_id) {
            *active = active.saturating_sub(1);
            if *active == 0 {
                state.active_sessions.remove(session_id);
            }
        }
        if priority == HlsRangePriority::Foreground {
            state.active_foreground -= 1;
        }
        if priority == HlsRangePriority::PlaybackPreparation {
            state.active_preparation_batches = state.active_preparation_batches.saturating_sub(1);
        }
        drop(state);
        self.signal_change();
    }

    fn finish_flight(&self, key: &RangeChunkKey, completion: &watch::Sender<bool>) {
        let mut state = self.lock_state();
        if state
            .flights
            .get(key)
            .is_some_and(|current| current.completion.same_channel(completion))
        {
            state.flights.remove(key);
        }
        drop(state);
        completion.send_replace(true);
        self.signal_change();
    }

    fn finish_reservation(&self, key: &RangeResourceKey, id: u64) {
        let mut state = self.lock_state();
        if let Some(reservations) = state.reservations.get_mut(key) {
            reservations.remove(&id);
            if reservations.is_empty() {
                state.reservations.remove(key);
            }
        }
        drop(state);
        self.signal_change();
    }

    fn signal_change(&self) {
        let current = *self.changed.borrow();
        self.changed.send_replace(current.wrapping_add(1));
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, RangeState> {
        self.state.lock().expect("HLS range cache state poisoned")
    }
}

pub(crate) struct RangeActivity {
    cache: Arc<HlsRangeCache>,
    session_id: String,
    priority: HlsRangePriority,
}

pub(crate) struct HlsRangeChunkPermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
    cache: Arc<HlsRangeCache>,
    priority: HlsRangePriority,
    promoted_preparation_flight: Option<(RangeChunkKey, watch::Sender<bool>)>,
}

impl Drop for HlsRangeChunkPermit {
    fn drop(&mut self) {
        let mut state = self.cache.lock_state();
        if let Some((key, completion)) = &self.promoted_preparation_flight
            && let Some(flight) = state.flights.get_mut(key)
            && flight.completion.same_channel(completion)
        {
            flight.promoted_preparation_permit = false;
        }
        state.active_chunk_permits = state.active_chunk_permits.saturating_sub(1);
        match self.priority {
            HlsRangePriority::Foreground => {
                state.active_foreground_permits = state.active_foreground_permits.saturating_sub(1);
            }
            HlsRangePriority::PlaybackPreparation => {
                state.active_preparation_transfers =
                    state.active_preparation_transfers.saturating_sub(1);
            }
            HlsRangePriority::FarFuturePlayback | HlsRangePriority::Background => {}
        }
        drop(state);
        self.cache.signal_change();
    }
}

struct ForegroundWaiter {
    cache: Arc<HlsRangeCache>,
}

struct PreparationWaiter {
    cache: Arc<HlsRangeCache>,
}

impl Drop for ForegroundWaiter {
    fn drop(&mut self) {
        let mut state = self.cache.lock_state();
        state.waiting_foreground = state.waiting_foreground.saturating_sub(1);
        drop(state);
        self.cache.signal_change();
    }
}

impl Drop for PreparationWaiter {
    fn drop(&mut self) {
        let mut state = self.cache.lock_state();
        state.waiting_preparation = state.waiting_preparation.saturating_sub(1);
        drop(state);
        self.cache.signal_change();
    }
}

pub(crate) struct SessionPublicationGuard {
    cache: Arc<HlsRangeCache>,
    session_id: String,
    mode: SessionPublicationMode,
    committed: bool,
}

impl SessionPublicationGuard {
    pub(crate) fn commit(mut self) {
        let mut state = self.cache.lock_state();
        if self.mode == SessionPublicationMode::Fresh {
            state.unpublished.remove(&self.session_id);
        }
        state.published.insert(self.session_id.clone());
        state.active = state.active.saturating_sub(1);
        if let Some(active) = state.active_sessions.get_mut(&self.session_id) {
            *active = active.saturating_sub(1);
            if *active == 0 {
                state.active_sessions.remove(&self.session_id);
            }
        }
        self.committed = true;
        drop(state);
        self.cache.signal_change();
    }
}

impl Drop for SessionPublicationGuard {
    fn drop(&mut self) {
        if !self.committed {
            self.cache
                .finish_activity(&self.session_id, HlsRangePriority::Background);
        }
    }
}

impl Drop for RangeActivity {
    fn drop(&mut self) {
        self.cache.finish_activity(&self.session_id, self.priority);
    }
}

impl HlsRangeCache {
    pub(crate) fn enter_preparation_batch(
        self: &Arc<Self>,
        session_id: &str,
    ) -> Result<RangeActivity, HlsRangeError> {
        self.enter(session_id, HlsRangePriority::PlaybackPreparation)
    }
}

pub(crate) struct RangeFlight {
    cache: Arc<HlsRangeCache>,
    key: RangeChunkKey,
    completion: watch::Sender<bool>,
    receiver: watch::Receiver<bool>,
    readiness: watch::Sender<Option<HlsRangeReadiness>>,
    readiness_receiver: watch::Receiver<Option<HlsRangeReadiness>>,
    owner: bool,
    foreground_interest: bool,
    preparation_interest: bool,
}

impl RangeFlight {
    pub(crate) fn is_owner(&self) -> bool {
        self.owner
    }

    pub(crate) async fn notified(&self) {
        let mut receiver = self.receiver.clone();
        if !*receiver.borrow() {
            let _ = receiver.changed().await;
        }
    }

    pub(crate) fn readiness(&self) -> watch::Receiver<Option<HlsRangeReadiness>> {
        self.readiness_receiver.clone()
    }

    pub(crate) fn readiness_sender(&self) -> watch::Sender<Option<HlsRangeReadiness>> {
        self.readiness.clone()
    }
}

impl Drop for RangeFlight {
    fn drop(&mut self) {
        if self.owner {
            self.cache.finish_flight(&self.key, &self.completion);
        } else if self.foreground_interest {
            self.cache
                .release_foreground_interest(&self.key, &self.completion);
        }
        if self.preparation_interest {
            self.cache.release_preparation_interest(&self.key);
        }
    }
}

pub(crate) struct RangeQuotaLease {
    cache: Arc<HlsRangeCache>,
    key: RangeResourceKey,
    id: u64,
}

impl Drop for RangeQuotaLease {
    fn drop(&mut self) {
        self.cache.finish_reservation(&self.key, self.id);
    }
}

pub(crate) struct HlsSessionRemovalGuard {
    cache: Arc<HlsRangeCache>,
    session_id: String,
    committed: bool,
}

impl HlsSessionRemovalGuard {
    pub(crate) fn commit(mut self) {
        let mut state = self.cache.lock_state();
        state.removing.remove(&self.session_id);
        state.unpublished.remove(&self.session_id);
        state.published.remove(&self.session_id);
        state.retired.insert(self.session_id.clone());
        self.committed = true;
        drop(state);
        self.cache.signal_change();
    }
}

impl Drop for HlsSessionRemovalGuard {
    fn drop(&mut self) {
        if !self.committed {
            self.cache.lock_state().removing.remove(&self.session_id);
            self.cache.signal_change();
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod publication_state_tests {
    use super::{
        HlsRangeCache, HlsRangeError, HlsRangePriority, RangeChunkKey, RangeResourceKey,
        SessionPublicationMode,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    fn cache() -> Arc<HlsRangeCache> {
        Arc::new(HlsRangeCache::new(2, 1024))
    }

    fn assert_rejected(result: Result<super::SessionPublicationGuard, HlsRangeError>) {
        assert!(matches!(result, Err(HlsRangeError::SessionRemoving)));
    }

    fn resource_key(session_id: &str) -> RangeResourceKey {
        RangeResourceKey {
            session_id: session_id.to_owned(),
            resource_id: "video".to_owned(),
            representation: "representation".to_owned(),
        }
    }

    fn chunk_key(session_id: &str) -> RangeChunkKey {
        RangeChunkKey {
            resource: resource_key(session_id),
            total_length: 1,
            range: 0..1,
        }
    }

    #[test]
    fn fresh_failure_stays_closed_and_allows_fresh_retry() {
        let cache = cache();
        drop(
            cache
                .begin_session_publication("fresh", SessionPublicationMode::Fresh)
                .expect("first publication should begin"),
        );
        assert!(
            cache
                .enter("fresh", super::HlsRangePriority::Foreground)
                .is_err()
        );

        let retry = cache
            .begin_session_publication("fresh", SessionPublicationMode::Fresh)
            .expect("failed first publication should be retryable");
        assert_rejected(cache.begin_session_publication("fresh", SessionPublicationMode::Fresh));
        retry.commit();
        assert!(
            cache
                .enter("fresh", super::HlsRangePriority::Foreground)
                .is_ok()
        );
    }

    #[test]
    fn failed_fresh_publication_blocks_range_and_quota_admission() {
        let cache = cache();
        drop(
            cache
                .begin_session_publication("unpublished", SessionPublicationMode::Fresh)
                .expect("fresh publication should begin"),
        );

        assert!(
            cache
                .enter("unpublished", HlsRangePriority::Foreground)
                .is_err()
        );
        assert!(
            cache
                .claim_chunk(chunk_key("unpublished"), HlsRangePriority::Foreground)
                .is_err()
        );
        assert!(cache.reserve(resource_key("unpublished"), 1, 0, 0).is_err());

        let control_called = AtomicBool::new(false);
        assert!(
            cache
                .check_priority_control(
                    &chunk_key("unpublished"),
                    HlsRangePriority::Foreground,
                    &|| {
                        control_called.store(true, Ordering::SeqCst);
                        crate::hls_cache::HlsCacheFillControl::Continue
                    },
                )
                .is_err()
        );
        assert!(!control_called.load(Ordering::SeqCst));
    }

    #[test]
    fn refresh_and_owned_completion_failure_preserve_open_admission() {
        let cache = cache();
        cache
            .begin_session_publication("open", SessionPublicationMode::Fresh)
            .expect("fresh publication should begin")
            .commit();

        drop(
            cache
                .begin_session_publication("open", SessionPublicationMode::Refresh)
                .expect("refresh should begin"),
        );
        assert!(
            cache
                .enter("open", super::HlsRangePriority::Foreground)
                .is_ok()
        );

        drop(
            cache
                .begin_session_publication("open", SessionPublicationMode::OwnedCompletion)
                .expect("owned completion should begin"),
        );
        assert!(
            cache
                .enter("open", super::HlsRangePriority::Foreground)
                .is_ok()
        );
    }

    #[test]
    fn publication_modes_cannot_reopen_retired_or_shutdown_sessions() {
        let retired_cache = cache();
        let removal = retired_cache
            .try_begin_session_removal("deleted")
            .expect("removal admission should succeed")
            .expect("session is idle");
        removal.commit();
        for mode in [
            SessionPublicationMode::Fresh,
            SessionPublicationMode::Refresh,
            SessionPublicationMode::OwnedCompletion,
            SessionPublicationMode::OwnedRestoration,
        ] {
            assert_rejected(retired_cache.begin_session_publication("deleted", mode));
        }

        let shutdown_cache = cache();
        shutdown_cache.lock_state().shutting_down = true;
        assert_rejected(
            shutdown_cache.begin_session_publication("shutdown", SessionPublicationMode::Fresh),
        );
    }

    #[test]
    fn publication_rejects_session_already_removing() {
        let cache = cache();
        cache.lock_state().removing.insert("removing".to_owned());
        for mode in [
            SessionPublicationMode::Fresh,
            SessionPublicationMode::Refresh,
            SessionPublicationMode::OwnedCompletion,
            SessionPublicationMode::OwnedRestoration,
        ] {
            assert_rejected(cache.begin_session_publication("removing", mode));
        }
    }
}

#[cfg(test)]
mod preparation_scheduler_tests {
    use super::{HlsRangeCache, HlsRangeError, HlsRangePriority, RangeChunkKey, RangeResourceKey};
    use crate::hls_cache::HlsCacheFillControl;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::{mpsc, oneshot};

    fn chunk_key(session_id: &str, resource_id: &str, start: u64) -> RangeChunkKey {
        RangeChunkKey {
            resource: RangeResourceKey {
                session_id: session_id.to_owned(),
                resource_id: resource_id.to_owned(),
                representation: "test-representation".to_owned(),
            },
            total_length: 4096,
            range: start..start + 512,
        }
    }

    fn continue_control() -> impl Fn() -> HlsCacheFillControl + Send + Sync + 'static {
        || HlsCacheFillControl::Continue
    }

    fn cancel_control(
        cancelled: Arc<AtomicBool>,
    ) -> impl Fn() -> HlsCacheFillControl + Send + Sync + 'static {
        move || {
            if cancelled.load(Ordering::SeqCst) {
                HlsCacheFillControl::Cancel
            } else {
                HlsCacheFillControl::Continue
            }
        }
    }

    fn spawn_preparation_waiter<F>(
        cache: Arc<HlsRangeCache>,
        key: RangeChunkKey,
        id: usize,
        control: F,
        acquired: mpsc::UnboundedSender<usize>,
    ) -> (
        oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<(), HlsRangeError>>,
    )
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync + 'static,
    {
        let (release_tx, release_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _permit = cache
                .acquire_chunk_permit(&key, HlsRangePriority::PlaybackPreparation, &control)
                .await?;
            let _ = acquired.send(id);
            let _ = release_rx.await;
            Ok(())
        });
        (release_tx, task)
    }

    async fn wait_for_state(
        cache: &Arc<HlsRangeCache>,
        predicate: impl Fn(&super::RangeState) -> bool,
    ) {
        let mut changed = cache.changed.subscribe();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if predicate(&cache.lock_state()) {
                    return;
                }
                changed
                    .changed()
                    .await
                    .expect("range cache state should remain observable");
            }
        })
        .await
        .expect("range scheduler state should reach the expected value");
    }

    #[tokio::test]
    async fn preparation_uses_spare_capacity_beside_foreground_and_preempts_when_it_waits() {
        let cache = Arc::new(HlsRangeCache::new(2, 0));
        let prep_key = chunk_key("session", "prep", 0);
        let prep_permit = cache
            .acquire_chunk_permit(
                &prep_key,
                HlsRangePriority::PlaybackPreparation,
                &continue_control(),
            )
            .await
            .expect("preparation should acquire a slot while one is reserved");
        let foreground_permit = cache
            .acquire_chunk_permit(
                &chunk_key("session", "foreground", 0),
                HlsRangePriority::Foreground,
                &continue_control(),
            )
            .await
            .expect("foreground should acquire its reserved slot");
        assert_eq!(0, cache.chunk_permits.available_permits());

        let foreground_cache = Arc::clone(&cache);
        let (acquired_tx, acquired_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let waiting_foreground = tokio::spawn(async move {
            let _permit = foreground_cache
                .acquire_chunk_permit(
                    &chunk_key("session", "foreground-waiting", 0),
                    HlsRangePriority::Foreground,
                    &continue_control(),
                )
                .await
                .expect("waiting foreground should acquire after preparation yields");
            let _ = acquired_tx.send(());
            let _ = release_rx.await;
        });
        wait_for_state(&cache, |state| state.waiting_foreground == 1).await;
        assert!(matches!(
            cache.check_priority_control(
                &prep_key,
                HlsRangePriority::PlaybackPreparation,
                &continue_control()
            ),
            Err(HlsRangeError::Preempted)
        ));

        drop(prep_permit);
        acquired_rx
            .await
            .expect("foreground should use capacity released by preparation");
        drop(foreground_permit);
        let _ = release_tx.send(());
        waiting_foreground
            .await
            .expect("foreground task should finish");
        wait_for_state(&cache, |state| {
            state.waiting_foreground == 0 && state.active_chunk_permits == 0
        })
        .await;
    }

    #[tokio::test]
    async fn only_one_preparation_transfer_runs_per_store() {
        let cache = Arc::new(HlsRangeCache::new(3, 0));
        let first = cache
            .acquire_chunk_permit(
                &chunk_key("session-a", "first", 0),
                HlsRangePriority::PlaybackPreparation,
                &continue_control(),
            )
            .await
            .expect("first preparation should acquire");

        let second_cache = Arc::clone(&cache);
        let (acquired_tx, acquired_rx) = oneshot::channel();
        let second = tokio::spawn(async move {
            let _permit = second_cache
                .acquire_chunk_permit(
                    &chunk_key("session-b", "second", 0),
                    HlsRangePriority::PlaybackPreparation,
                    &continue_control(),
                )
                .await
                .expect("second preparation should acquire after the first releases");
            let _ = acquired_tx.send(());
        });
        wait_for_state(&cache, |state| state.waiting_preparation == 1).await;
        assert_eq!(1, cache.lock_state().active_preparation_transfers);
        drop(first);
        acquired_rx
            .await
            .expect("second preparation should eventually acquire");
        second.await.expect("second preparation task should finish");
        wait_for_state(&cache, |state| {
            state.active_preparation_transfers == 0 && state.waiting_preparation == 0
        })
        .await;
    }

    #[tokio::test]
    async fn single_parallelism_preparation_yields_to_foreground_and_recovers() {
        let cache = Arc::new(HlsRangeCache::new(1, 0));
        let prep_key = chunk_key("session", "prep", 0);
        let prep_permit = cache
            .acquire_chunk_permit(
                &prep_key,
                HlsRangePriority::PlaybackPreparation,
                &continue_control(),
            )
            .await
            .expect("idle single-slot store should admit preparation");

        let foreground_cache = Arc::clone(&cache);
        let foreground = tokio::spawn(async move {
            foreground_cache
                .acquire_chunk_permit(
                    &chunk_key("session", "foreground", 0),
                    HlsRangePriority::Foreground,
                    &continue_control(),
                )
                .await
                .expect("foreground should acquire after preparation yields")
        });
        wait_for_state(&cache, |state| state.waiting_foreground == 1).await;
        assert!(matches!(
            cache.check_priority_control(
                &prep_key,
                HlsRangePriority::PlaybackPreparation,
                &continue_control()
            ),
            Err(HlsRangeError::Preempted)
        ));
        drop(prep_permit);
        let foreground_permit = foreground
            .await
            .expect("foreground task should finish acquisition");
        drop(foreground_permit);
        let recovered = cache
            .acquire_chunk_permit(
                &prep_key,
                HlsRangePriority::PlaybackPreparation,
                &continue_control(),
            )
            .await
            .expect("preparation should recover after foreground releases");
        drop(recovered);
    }

    #[tokio::test]
    async fn cancelling_foreground_wait_releases_waiter_and_all_permit_counters() {
        let cache = Arc::new(HlsRangeCache::new(1, 0));
        let prep_permit = cache
            .acquire_chunk_permit(
                &chunk_key("session", "prep", 0),
                HlsRangePriority::PlaybackPreparation,
                &continue_control(),
            )
            .await
            .expect("preparation should acquire the only slot");
        let foreground_cache = Arc::clone(&cache);
        let foreground = tokio::spawn(async move {
            foreground_cache
                .acquire_chunk_permit(
                    &chunk_key("session", "foreground", 0),
                    HlsRangePriority::Foreground,
                    &continue_control(),
                )
                .await
        });
        wait_for_state(&cache, |state| state.waiting_foreground == 1).await;
        foreground.abort();
        let _ = foreground.await;
        wait_for_state(&cache, |state| state.waiting_foreground == 0).await;
        assert_eq!(1, cache.lock_state().active_chunk_permits);
        drop(prep_permit);
        wait_for_state(&cache, |state| state.active_chunk_permits == 0).await;
        assert_eq!(1, cache.chunk_permits.available_permits());
    }

    #[tokio::test]
    async fn foreground_cancellation_is_checked_before_free_permit_fast_path() {
        let cache = Arc::new(HlsRangeCache::new(1, 0));
        let result = cache
            .acquire_chunk_permit(
                &chunk_key("session", "foreground", 0),
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Cancel,
            )
            .await;

        assert!(matches!(result, Err(HlsRangeError::Cancelled)));
        let state = cache.lock_state();
        assert_eq!(0, state.active_chunk_permits);
        assert_eq!(0, state.active_foreground_permits);
        assert_eq!(1, cache.chunk_permits.available_permits());
    }

    #[tokio::test]
    async fn background_yields_to_foreground_waiter_despite_preparation_interest() {
        let cache = Arc::new(HlsRangeCache::new(1, 0));
        let background_key = chunk_key("session", "background", 0);
        let background_flight = cache
            .claim_chunk(background_key.clone(), HlsRangePriority::Background)
            .expect("background flight should be admitted");
        let preparation_flight = cache
            .claim_chunk(
                background_key.clone(),
                HlsRangePriority::PlaybackPreparation,
            )
            .expect("preparation should join the background flight");
        let background_permit = cache
            .acquire_chunk_permit(
                &background_key,
                HlsRangePriority::Background,
                &continue_control(),
            )
            .await
            .expect("background flight should hold the only permit");

        let foreground_cache = Arc::clone(&cache);
        let (acquired_tx, acquired_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let foreground = tokio::spawn(async move {
            let _permit = foreground_cache
                .acquire_chunk_permit(
                    &chunk_key("session", "unrelated-foreground", 0),
                    HlsRangePriority::Foreground,
                    &continue_control(),
                )
                .await
                .expect("foreground should acquire after background yields");
            let _ = acquired_tx.send(());
            let _ = release_rx.await;
        });
        wait_for_state(&cache, |state| state.waiting_foreground == 1).await;

        assert!(matches!(
            cache.check_priority_control(
                &background_key,
                HlsRangePriority::Background,
                &continue_control()
            ),
            Err(HlsRangeError::Preempted)
        ));
        assert!(matches!(
            cache.check_priority_control(&background_key, HlsRangePriority::Background, &|| {
                HlsCacheFillControl::Cancel
            }),
            Err(HlsRangeError::Cancelled)
        ));

        let foreground_flight = cache
            .claim_chunk(background_key.clone(), HlsRangePriority::Foreground)
            .expect("foreground should promote the shared flight");
        assert!(
            cache
                .check_priority_control(
                    &background_key,
                    HlsRangePriority::Background,
                    &continue_control()
                )
                .is_ok()
        );

        drop(foreground_flight);
        drop(preparation_flight);
        drop(background_flight);
        drop(background_permit);
        acquired_rx
            .await
            .expect("waiting foreground should acquire the released permit");
        let _ = release_tx.send(());
        foreground.await.expect("foreground task should finish");
        wait_for_state(&cache, |state| {
            state.waiting_foreground == 0 && state.active_chunk_permits == 0
        })
        .await;
    }

    #[test]
    fn foreground_same_flight_promotes_preparation_and_background_resumes_after_batch() {
        let cache = Arc::new(HlsRangeCache::new(2, 0));
        let key = chunk_key("session", "shared", 0);
        let preparation = cache
            .claim_chunk(key.clone(), HlsRangePriority::PlaybackPreparation)
            .expect("preparation should claim its flight");
        let foreground = cache
            .claim_chunk(key.clone(), HlsRangePriority::Foreground)
            .expect("foreground should join the same flight");
        let _unrelated_waiter = cache.register_foreground_waiter();
        assert!(
            cache
                .check_priority_control(
                    &key,
                    HlsRangePriority::PlaybackPreparation,
                    &continue_control()
                )
                .is_ok()
        );
        drop(_unrelated_waiter);
        drop(foreground);
        drop(preparation);

        let background_key = chunk_key("session", "background", 0);
        let batch = cache
            .enter_preparation_batch("session")
            .expect("preparation batch should enter the session");
        assert!(matches!(
            cache.check_priority_control(
                &background_key,
                HlsRangePriority::Background,
                &continue_control()
            ),
            Err(HlsRangeError::Preempted)
        ));
        drop(batch);
        assert!(
            cache
                .check_priority_control(
                    &background_key,
                    HlsRangePriority::Background,
                    &continue_control()
                )
                .is_ok()
        );
    }

    #[tokio::test]
    async fn waiting_preparation_flight_uses_reserved_capacity_when_promoted() {
        let cache = Arc::new(HlsRangeCache::new(2, 0));
        let unrelated_key = chunk_key("session", "unrelated-preparation", 0);
        let _unrelated_flight = cache
            .claim_chunk(unrelated_key.clone(), HlsRangePriority::PlaybackPreparation)
            .expect("unrelated preparation should own its flight");
        let unrelated_permit = cache
            .acquire_chunk_permit(
                &unrelated_key,
                HlsRangePriority::PlaybackPreparation,
                &continue_control(),
            )
            .await
            .expect("unrelated preparation should occupy one slot");

        let promoted_key = chunk_key("session", "promoted-preparation", 512);
        let preparation_flight = cache
            .claim_chunk(promoted_key.clone(), HlsRangePriority::PlaybackPreparation)
            .expect("preparation should own the promoted flight");
        let promoted_cache = Arc::clone(&cache);
        let task_key = promoted_key.clone();
        let (acquired_tx, acquired_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let promoted = tokio::spawn(async move {
            let _permit = promoted_cache
                .acquire_chunk_permit(
                    &task_key,
                    HlsRangePriority::PlaybackPreparation,
                    &continue_control(),
                )
                .await
                .expect("promoted preparation should use the reserved slot");
            let _ = acquired_tx.send(());
            let _ = release_rx.await;
        });
        wait_for_state(&cache, |state| state.waiting_preparation == 1).await;

        let foreground_follower = cache
            .claim_chunk(promoted_key.clone(), HlsRangePriority::Foreground)
            .expect("foreground should join the preparation flight");
        tokio::time::timeout(Duration::from_secs(2), acquired_rx)
            .await
            .expect("foreground promotion should not wait for unrelated preparation")
            .expect("promoted preparation should report acquisition");
        {
            let state = cache.lock_state();
            assert_eq!(2, state.active_chunk_permits);
            assert_eq!(1, state.active_preparation_transfers);
            assert_eq!(1, state.active_foreground_permits);
            assert_eq!(0, state.waiting_preparation);
            assert_eq!(0, state.waiting_foreground);
        }

        drop(foreground_follower);
        assert!(matches!(
            cache.check_priority_control(
                &promoted_key,
                HlsRangePriority::PlaybackPreparation,
                &continue_control(),
            ),
            Err(HlsRangeError::Preempted)
        ));
        assert!(matches!(
            cache.check_priority_control(
                &promoted_key,
                HlsRangePriority::PlaybackPreparation,
                &|| HlsCacheFillControl::Cancel,
            ),
            Err(HlsRangeError::Cancelled)
        ));
        {
            let state = cache.lock_state();
            assert_eq!(1, state.active_foreground_permits);
            assert_eq!(1, state.active_preparation_transfers);
        }

        let _ = release_tx.send(());
        tokio::time::timeout(Duration::from_secs(2), promoted)
            .await
            .expect("promoted transfer should release its permit")
            .expect("promoted transfer task should finish");
        wait_for_state(&cache, |state| state.active_chunk_permits == 1).await;
        drop(unrelated_permit);
        wait_for_state(&cache, |state| state.active_chunk_permits == 0).await;
        drop(preparation_flight);
    }

    #[tokio::test]
    async fn retracted_waiting_promotion_restores_preparation_admission() {
        let cache = Arc::new(HlsRangeCache::new(2, 0));
        let unrelated_preparation = cache
            .acquire_chunk_permit(
                &chunk_key("session", "unrelated-preparation", 0),
                HlsRangePriority::PlaybackPreparation,
                &continue_control(),
            )
            .await
            .expect("unrelated preparation should occupy one slot");
        let foreground_permit = cache
            .acquire_chunk_permit(
                &chunk_key("session", "foreground", 0),
                HlsRangePriority::Foreground,
                &continue_control(),
            )
            .await
            .expect("foreground should occupy the reserved slot");

        let promoted_key = chunk_key("session", "retracted-preparation", 512);
        let preparation_flight = cache
            .claim_chunk(promoted_key.clone(), HlsRangePriority::PlaybackPreparation)
            .expect("preparation should own the waiting flight");
        let promoted_cache = Arc::clone(&cache);
        let task_key = promoted_key.clone();
        let (acquired_tx, acquired_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let promoted = tokio::spawn(async move {
            let _permit = promoted_cache
                .acquire_chunk_permit(
                    &task_key,
                    HlsRangePriority::PlaybackPreparation,
                    &continue_control(),
                )
                .await
                .expect("retracted preparation should resume normal admission");
            let _ = acquired_tx.send(());
            let _ = release_rx.await;
        });
        wait_for_state(&cache, |state| state.waiting_preparation == 1).await;

        let foreground_follower = cache
            .claim_chunk(promoted_key.clone(), HlsRangePriority::Foreground)
            .expect("foreground should promote the waiting flight");
        wait_for_state(&cache, |state| {
            state.waiting_foreground == 1 && state.waiting_preparation == 0
        })
        .await;
        drop(foreground_follower);
        wait_for_state(&cache, |state| {
            state.waiting_foreground == 0 && state.waiting_preparation == 1
        })
        .await;
        drop(foreground_permit);
        assert!(
            cache
                .try_acquire_preparation_chunk_permit_for_owner(
                    &promoted_key,
                    Some(&preparation_flight.completion),
                    false,
                )
                .is_none(),
            "retracted promotion must not bypass the preparation cap"
        );
        assert_eq!(1, cache.lock_state().active_chunk_permits);

        drop(unrelated_preparation);
        tokio::time::timeout(Duration::from_secs(2), acquired_rx)
            .await
            .expect("preparation should acquire after its unrelated peer releases")
            .expect("preparation should report acquisition");
        {
            let state = cache.lock_state();
            assert_eq!(1, state.active_chunk_permits);
            assert_eq!(1, state.active_preparation_transfers);
            assert_eq!(0, state.active_foreground_permits);
            assert_eq!(0, state.waiting_foreground);
            assert_eq!(0, state.waiting_preparation);
        }

        let _ = release_tx.send(());
        tokio::time::timeout(Duration::from_secs(2), promoted)
            .await
            .expect("resumed preparation should release its permit")
            .expect("resumed preparation task should finish");
        wait_for_state(&cache, |state| state.active_chunk_permits == 0).await;
        drop(preparation_flight);
    }

    #[tokio::test]
    async fn single_slot_promotion_waits_without_exceeding_global_parallelism() {
        let cache = Arc::new(HlsRangeCache::new(1, 0));
        let unrelated_permit = cache
            .acquire_chunk_permit(
                &chunk_key("session", "unrelated-preparation", 0),
                HlsRangePriority::PlaybackPreparation,
                &continue_control(),
            )
            .await
            .expect("single slot should admit preparation while idle");
        let promoted_key = chunk_key("session", "single-slot-promotion", 512);
        let preparation_flight = cache
            .claim_chunk(promoted_key.clone(), HlsRangePriority::PlaybackPreparation)
            .expect("preparation should own the waiting flight");
        let promoted_cache = Arc::clone(&cache);
        let task_key = promoted_key.clone();
        let (acquired_tx, acquired_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let promoted = tokio::spawn(async move {
            let _permit = promoted_cache
                .acquire_chunk_permit(
                    &task_key,
                    HlsRangePriority::PlaybackPreparation,
                    &continue_control(),
                )
                .await
                .expect("promoted preparation should acquire after the slot is released");
            let _ = acquired_tx.send(());
            let _ = release_rx.await;
        });
        wait_for_state(&cache, |state| state.waiting_preparation == 1).await;
        let foreground_follower = cache
            .claim_chunk(promoted_key.clone(), HlsRangePriority::Foreground)
            .expect("foreground should promote the waiting flight");
        wait_for_state(&cache, |state| {
            state.waiting_foreground == 1 && state.waiting_preparation == 0
        })
        .await;
        assert_eq!(1, cache.lock_state().active_chunk_permits);
        drop(unrelated_permit);
        tokio::time::timeout(Duration::from_secs(2), acquired_rx)
            .await
            .expect("promoted transfer should acquire the released single slot")
            .expect("promoted transfer should report acquisition");
        {
            let state = cache.lock_state();
            assert_eq!(1, state.active_chunk_permits);
            assert_eq!(1, state.active_foreground_permits);
            assert_eq!(0, state.active_preparation_transfers);
        }
        drop(foreground_follower);
        assert!(matches!(
            cache.check_priority_control(
                &promoted_key,
                HlsRangePriority::PlaybackPreparation,
                &continue_control(),
            ),
            Err(HlsRangeError::Preempted)
        ));
        let _ = release_tx.send(());
        tokio::time::timeout(Duration::from_secs(2), promoted)
            .await
            .expect("promoted transfer should release the single slot")
            .expect("promoted transfer task should finish");
        wait_for_state(&cache, |state| state.active_chunk_permits == 0).await;
        drop(preparation_flight);
    }

    #[tokio::test]
    async fn two_promoted_preparations_use_parallel_foreground_slots() {
        let cache = Arc::new(HlsRangeCache::new(2, 0));
        let holding_permit0 = cache
            .acquire_chunk_permit(
                &chunk_key("session", "holding-foreground-0", 0),
                HlsRangePriority::Foreground,
                &continue_control(),
            )
            .await
            .expect("first foreground should occupy a slot");
        let holding_permit1 = cache
            .acquire_chunk_permit(
                &chunk_key("session", "holding-foreground-1", 0),
                HlsRangePriority::Foreground,
                &continue_control(),
            )
            .await
            .expect("second foreground should occupy a slot");

        let key0 = chunk_key("session", "promoted-parallel-0", 0);
        let key1 = chunk_key("session", "promoted-parallel-1", 512);
        let preparation_flight0 = cache
            .claim_chunk(key0.clone(), HlsRangePriority::PlaybackPreparation)
            .expect("first preparation should own its flight");
        let preparation_flight1 = cache
            .claim_chunk(key1.clone(), HlsRangePriority::PlaybackPreparation)
            .expect("second preparation should own its flight");
        let (acquired_tx, mut acquired_rx) = mpsc::unbounded_channel();
        let (release_tx0, task0) = spawn_preparation_waiter(
            Arc::clone(&cache),
            key0.clone(),
            0,
            continue_control(),
            acquired_tx.clone(),
        );
        let (release_tx1, task1) = spawn_preparation_waiter(
            Arc::clone(&cache),
            key1.clone(),
            1,
            continue_control(),
            acquired_tx,
        );
        wait_for_state(&cache, |state| state.waiting_preparation == 2).await;
        let foreground_follower0 = cache
            .claim_chunk(key0, HlsRangePriority::Foreground)
            .expect("first foreground should promote its preparation flight");
        let foreground_follower1 = cache
            .claim_chunk(key1, HlsRangePriority::Foreground)
            .expect("second foreground should promote its preparation flight");
        wait_for_state(&cache, |state| {
            state.waiting_foreground == 2 && state.waiting_preparation == 0
        })
        .await;

        drop(holding_permit0);
        let first = tokio::time::timeout(Duration::from_secs(2), acquired_rx.recv())
            .await
            .expect("one promoted preparation should acquire the released slot")
            .expect("a promoted preparation should report acquisition");
        assert!(first < 2);
        {
            let state = cache.lock_state();
            assert_eq!(2, state.active_chunk_permits);
            assert_eq!(1, state.waiting_foreground);
            assert_eq!(2, state.active_foreground_permits);
        }

        drop(holding_permit1);
        let second = tokio::time::timeout(Duration::from_secs(2), acquired_rx.recv())
            .await
            .expect("the other promoted preparation should acquire the next slot")
            .expect("the other promoted preparation should report acquisition");
        assert_ne!(first, second);
        {
            let state = cache.lock_state();
            assert_eq!(2, state.active_chunk_permits);
            assert_eq!(0, state.waiting_foreground);
            assert_eq!(2, state.active_foreground_permits);
        }

        let mut release = [Some(release_tx0), Some(release_tx1)];
        let _ = release[first]
            .take()
            .expect("first release sender")
            .send(());
        let _ = release[second]
            .take()
            .expect("second release sender")
            .send(());
        for task in [task0, task1] {
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .expect("promoted preparation should finish")
                .expect("promoted preparation task should not panic")
                .expect("promoted preparation should complete");
        }
        wait_for_state(&cache, |state| state.active_chunk_permits == 0).await;
        assert_eq!(2, cache.chunk_permits.available_permits());
        drop(foreground_follower0);
        drop(foreground_follower1);
        drop(preparation_flight0);
        drop(preparation_flight1);
    }

    #[tokio::test]
    async fn two_promoted_preparations_progress_sequentially_at_parallelism_one() {
        let cache = Arc::new(HlsRangeCache::new(1, 0));
        let holding_permit = cache
            .acquire_chunk_permit(
                &chunk_key("session", "single-slot-holder", 0),
                HlsRangePriority::Foreground,
                &continue_control(),
            )
            .await
            .expect("foreground should occupy the only slot");
        let key0 = chunk_key("session", "promoted-single-0", 0);
        let key1 = chunk_key("session", "promoted-single-1", 512);
        let preparation_flight0 = cache
            .claim_chunk(key0.clone(), HlsRangePriority::PlaybackPreparation)
            .expect("first preparation should own its flight");
        let preparation_flight1 = cache
            .claim_chunk(key1.clone(), HlsRangePriority::PlaybackPreparation)
            .expect("second preparation should own its flight");
        let (acquired_tx, mut acquired_rx) = mpsc::unbounded_channel();
        let (release_tx0, task0) = spawn_preparation_waiter(
            Arc::clone(&cache),
            key0.clone(),
            0,
            continue_control(),
            acquired_tx.clone(),
        );
        let (release_tx1, task1) = spawn_preparation_waiter(
            Arc::clone(&cache),
            key1.clone(),
            1,
            continue_control(),
            acquired_tx,
        );
        wait_for_state(&cache, |state| state.waiting_preparation == 2).await;
        let foreground_follower0 = cache
            .claim_chunk(key0, HlsRangePriority::Foreground)
            .expect("first foreground should promote its preparation flight");
        let foreground_follower1 = cache
            .claim_chunk(key1, HlsRangePriority::Foreground)
            .expect("second foreground should promote its preparation flight");
        wait_for_state(&cache, |state| {
            state.waiting_foreground == 2 && state.waiting_preparation == 0
        })
        .await;

        drop(holding_permit);
        let first = tokio::time::timeout(Duration::from_secs(2), acquired_rx.recv())
            .await
            .expect("one promoted preparation should acquire the only slot")
            .expect("a promoted preparation should report acquisition");
        assert!(first < 2);
        {
            let state = cache.lock_state();
            assert_eq!(1, state.active_chunk_permits);
            assert_eq!(1, state.active_foreground_permits);
            assert_eq!(1, state.waiting_foreground);
        }
        assert!(matches!(
            acquired_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        let mut release = [Some(release_tx0), Some(release_tx1)];
        let _ = release[first]
            .take()
            .expect("first release sender")
            .send(());
        let second = tokio::time::timeout(Duration::from_secs(2), acquired_rx.recv())
            .await
            .expect("the other promoted preparation should acquire after release")
            .expect("the other promoted preparation should report acquisition");
        assert_ne!(first, second);
        {
            let state = cache.lock_state();
            assert_eq!(1, state.active_chunk_permits);
            assert_eq!(1, state.active_foreground_permits);
            assert_eq!(0, state.waiting_foreground);
        }
        let _ = release[second]
            .take()
            .expect("second release sender")
            .send(());
        for task in [task0, task1] {
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .expect("promoted preparation should finish")
                .expect("promoted preparation task should not panic")
                .expect("promoted preparation should complete");
        }
        wait_for_state(&cache, |state| state.active_chunk_permits == 0).await;
        assert_eq!(1, cache.chunk_permits.available_permits());
        drop(foreground_follower0);
        drop(foreground_follower1);
        drop(preparation_flight0);
        drop(preparation_flight1);
    }

    #[tokio::test]
    async fn queued_ordinary_foreground_stays_ahead_of_promoted_preparation() {
        let cache = Arc::new(HlsRangeCache::new(2, 0));
        let holding_permit0 = cache
            .acquire_chunk_permit(
                &chunk_key("session", "fairness-holder-0", 0),
                HlsRangePriority::Foreground,
                &continue_control(),
            )
            .await
            .expect("first foreground should occupy a slot");
        let holding_permit1 = cache
            .acquire_chunk_permit(
                &chunk_key("session", "fairness-holder-1", 0),
                HlsRangePriority::Foreground,
                &continue_control(),
            )
            .await
            .expect("second foreground should occupy a slot");

        let ordinary_cache = Arc::clone(&cache);
        let (ordinary_acquired_tx, ordinary_acquired_rx) = oneshot::channel();
        let (ordinary_release_tx, ordinary_release_rx) = oneshot::channel();
        let ordinary = tokio::spawn(async move {
            let _permit = ordinary_cache
                .acquire_chunk_permit(
                    &chunk_key("session", "ordinary-foreground", 0),
                    HlsRangePriority::Foreground,
                    &continue_control(),
                )
                .await
                .expect("ordinary foreground should acquire its queued slot");
            let _ = ordinary_acquired_tx.send(());
            let _ = ordinary_release_rx.await;
        });
        wait_for_state(&cache, |state| state.waiting_foreground == 1).await;

        let promoted_key = chunk_key("session", "fairness-promoted", 512);
        let preparation_flight = cache
            .claim_chunk(promoted_key.clone(), HlsRangePriority::PlaybackPreparation)
            .expect("preparation should own the promoted flight");
        let (promoted_acquired_tx, mut promoted_acquired_rx) = mpsc::unbounded_channel();
        let (promoted_release_tx, promoted_task) = spawn_preparation_waiter(
            Arc::clone(&cache),
            promoted_key.clone(),
            0,
            continue_control(),
            promoted_acquired_tx,
        );
        let foreground_follower = cache
            .claim_chunk(promoted_key, HlsRangePriority::Foreground)
            .expect("foreground should promote the preparation flight");
        wait_for_state(&cache, |state| {
            state.waiting_foreground == 2 && state.waiting_preparation == 0
        })
        .await;

        drop(holding_permit0);
        tokio::time::timeout(Duration::from_secs(2), ordinary_acquired_rx)
            .await
            .expect("the earlier ordinary foreground should acquire first")
            .expect("ordinary foreground should report acquisition");
        assert!(matches!(
            promoted_acquired_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        {
            let state = cache.lock_state();
            assert_eq!(2, state.active_chunk_permits);
            assert_eq!(1, state.waiting_foreground);
        }

        let _ = ordinary_release_tx.send(());
        tokio::time::timeout(Duration::from_secs(2), promoted_acquired_rx.recv())
            .await
            .expect("promoted preparation should acquire after ordinary foreground")
            .expect("promoted preparation should report acquisition");
        {
            let state = cache.lock_state();
            assert_eq!(2, state.active_chunk_permits);
            assert_eq!(0, state.waiting_foreground);
            assert_eq!(2, state.active_foreground_permits);
        }

        let _ = promoted_release_tx.send(());
        tokio::time::timeout(Duration::from_secs(2), promoted_task)
            .await
            .expect("promoted preparation should finish")
            .expect("promoted preparation task should not panic")
            .expect("promoted preparation should complete");
        ordinary
            .await
            .expect("ordinary foreground task should not panic");
        drop(holding_permit1);
        wait_for_state(&cache, |state| state.active_chunk_permits == 0).await;
        drop(foreground_follower);
        drop(preparation_flight);
    }

    #[tokio::test]
    async fn preparation_without_original_flight_keeps_discovery_admission() {
        let cache = Arc::new(HlsRangeCache::new(2, 0));
        let holding_permit0 = cache
            .acquire_chunk_permit(
                &chunk_key("session", "discovery-holder-0", 0),
                HlsRangePriority::Foreground,
                &continue_control(),
            )
            .await
            .expect("first foreground should occupy a slot");
        let holding_permit1 = cache
            .acquire_chunk_permit(
                &chunk_key("session", "discovery-holder-1", 0),
                HlsRangePriority::Foreground,
                &continue_control(),
            )
            .await
            .expect("second foreground should occupy a slot");

        let key = chunk_key("session", "discovery-with-later-flight", 0);
        let (acquired_tx, mut acquired_rx) = mpsc::unbounded_channel();
        let (release_tx, task) = spawn_preparation_waiter(
            Arc::clone(&cache),
            key.clone(),
            0,
            continue_control(),
            acquired_tx,
        );
        wait_for_state(&cache, |state| state.waiting_preparation == 1).await;

        let later_flight = cache
            .claim_chunk(key.clone(), HlsRangePriority::PlaybackPreparation)
            .expect("later discovery should claim a new flight");
        let foreground_follower = cache
            .claim_chunk(key.clone(), HlsRangePriority::Foreground)
            .expect("foreground should promote the later flight");
        tokio::time::sleep(Duration::from_millis(60)).await;
        {
            let state = cache.lock_state();
            assert_eq!(2, state.active_chunk_permits);
            assert_eq!(1, state.waiting_preparation);
            assert_eq!(0, state.waiting_foreground);
            assert!(
                !state
                    .flights
                    .get(&key)
                    .expect("later flight should remain registered")
                    .promoted_preparation_permit
            );
        }

        drop(holding_permit0);
        assert_eq!(
            Some(0),
            tokio::time::timeout(Duration::from_secs(2), acquired_rx.recv())
                .await
                .expect("no-flight discovery preparation should use spare capacity")
        );
        {
            let state = cache.lock_state();
            assert_eq!(2, state.active_chunk_permits);
            assert_eq!(1, state.active_preparation_transfers);
            assert_eq!(1, state.active_foreground_permits);
            assert_eq!(0, state.waiting_preparation);
            assert_eq!(0, state.waiting_foreground);
            assert!(
                !state
                    .flights
                    .get(&key)
                    .expect("later flight should remain registered")
                    .promoted_preparation_permit
            );
        }

        let _ = release_tx.send(());
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("discovery preparation should finish")
            .expect("discovery preparation task should not panic")
            .expect("discovery preparation should complete");
        drop(holding_permit1);
        wait_for_state(&cache, |state| state.active_chunk_permits == 0).await;
        assert_eq!(2, cache.chunk_permits.available_permits());
        drop(foreground_follower);
        drop(later_flight);
    }

    #[tokio::test]
    async fn cancelling_one_promoted_waiter_does_not_strand_another() {
        let cache = Arc::new(HlsRangeCache::new(2, 0));
        let holding_permit0 = cache
            .acquire_chunk_permit(
                &chunk_key("session", "cancel-holder-0", 0),
                HlsRangePriority::Foreground,
                &continue_control(),
            )
            .await
            .expect("first foreground should occupy a slot");
        let holding_permit1 = cache
            .acquire_chunk_permit(
                &chunk_key("session", "cancel-holder-1", 0),
                HlsRangePriority::Foreground,
                &continue_control(),
            )
            .await
            .expect("second foreground should occupy a slot");

        let key0 = chunk_key("session", "cancel-promoted-0", 0);
        let key1 = chunk_key("session", "cancel-promoted-1", 512);
        let preparation_flight0 = cache
            .claim_chunk(key0.clone(), HlsRangePriority::PlaybackPreparation)
            .expect("first preparation should own its flight");
        let preparation_flight1 = cache
            .claim_chunk(key1.clone(), HlsRangePriority::PlaybackPreparation)
            .expect("second preparation should own its flight");
        let cancelled0 = Arc::new(AtomicBool::new(false));
        let (acquired_tx, mut acquired_rx) = mpsc::unbounded_channel();
        let (_release_tx0, task0) = spawn_preparation_waiter(
            Arc::clone(&cache),
            key0.clone(),
            0,
            cancel_control(Arc::clone(&cancelled0)),
            acquired_tx.clone(),
        );
        let (release_tx1, task1) = spawn_preparation_waiter(
            Arc::clone(&cache),
            key1.clone(),
            1,
            continue_control(),
            acquired_tx,
        );
        wait_for_state(&cache, |state| state.waiting_preparation == 2).await;
        let foreground_follower0 = cache
            .claim_chunk(key0, HlsRangePriority::Foreground)
            .expect("first foreground should promote its preparation flight");
        let foreground_follower1 = cache
            .claim_chunk(key1, HlsRangePriority::Foreground)
            .expect("second foreground should promote its preparation flight");
        wait_for_state(&cache, |state| {
            state.waiting_foreground == 2 && state.waiting_preparation == 0
        })
        .await;

        cancelled0.store(true, Ordering::SeqCst);
        let cancelled_result = tokio::time::timeout(Duration::from_secs(2), task0)
            .await
            .expect("cancelled promoted owner should stop waiting")
            .expect("cancelled promoted owner should not panic");
        assert!(matches!(cancelled_result, Err(HlsRangeError::Cancelled)));
        drop(foreground_follower0);
        wait_for_state(&cache, |state| state.waiting_foreground == 1).await;
        {
            let state = cache.lock_state();
            assert_eq!(2, state.active_chunk_permits);
            assert_eq!(0, state.waiting_preparation);
        }

        drop(holding_permit0);
        assert_eq!(
            Some(1),
            tokio::time::timeout(Duration::from_secs(2), acquired_rx.recv())
                .await
                .expect("remaining promoted owner should acquire the released slot")
        );
        {
            let state = cache.lock_state();
            assert_eq!(2, state.active_chunk_permits);
            assert_eq!(0, state.waiting_foreground);
            assert_eq!(2, state.active_foreground_permits);
        }
        let _ = release_tx1.send(());
        tokio::time::timeout(Duration::from_secs(2), task1)
            .await
            .expect("remaining promoted owner should finish")
            .expect("remaining promoted owner should not panic")
            .expect("remaining promoted owner should complete");
        drop(holding_permit1);
        wait_for_state(&cache, |state| state.active_chunk_permits == 0).await;
        {
            let state = cache.lock_state();
            assert_eq!(0, state.waiting_foreground);
            assert_eq!(0, state.waiting_preparation);
            assert_eq!(0, state.active_foreground_permits);
        }
        assert_eq!(2, cache.chunk_permits.available_permits());
        drop(foreground_follower1);
        drop(preparation_flight0);
        drop(preparation_flight1);
    }

    #[tokio::test]
    async fn retracting_one_promoted_waiter_does_not_strand_another() {
        let cache = Arc::new(HlsRangeCache::new(2, 0));
        let holding_permit0 = cache
            .acquire_chunk_permit(
                &chunk_key("session", "retract-holder-0", 0),
                HlsRangePriority::Foreground,
                &continue_control(),
            )
            .await
            .expect("first foreground should occupy a slot");
        let holding_permit1 = cache
            .acquire_chunk_permit(
                &chunk_key("session", "retract-holder-1", 0),
                HlsRangePriority::Foreground,
                &continue_control(),
            )
            .await
            .expect("second foreground should occupy a slot");

        let key0 = chunk_key("session", "retract-promoted-0", 0);
        let key1 = chunk_key("session", "retract-promoted-1", 512);
        let preparation_flight0 = cache
            .claim_chunk(key0.clone(), HlsRangePriority::PlaybackPreparation)
            .expect("first preparation should own its flight");
        let preparation_flight1 = cache
            .claim_chunk(key1.clone(), HlsRangePriority::PlaybackPreparation)
            .expect("second preparation should own its flight");
        let (acquired_tx, mut acquired_rx) = mpsc::unbounded_channel();
        let (_release_tx0, task0) = spawn_preparation_waiter(
            Arc::clone(&cache),
            key0.clone(),
            0,
            continue_control(),
            acquired_tx.clone(),
        );
        let (release_tx1, task1) = spawn_preparation_waiter(
            Arc::clone(&cache),
            key1.clone(),
            1,
            continue_control(),
            acquired_tx,
        );
        wait_for_state(&cache, |state| state.waiting_preparation == 2).await;
        let foreground_follower0 = cache
            .claim_chunk(key0, HlsRangePriority::Foreground)
            .expect("first foreground should promote its preparation flight");
        let foreground_follower1 = cache
            .claim_chunk(key1, HlsRangePriority::Foreground)
            .expect("second foreground should promote its preparation flight");
        wait_for_state(&cache, |state| {
            state.waiting_foreground == 2 && state.waiting_preparation == 0
        })
        .await;

        drop(foreground_follower0);
        let retracted_result = tokio::time::timeout(Duration::from_secs(2), task0)
            .await
            .expect("retracted promoted owner should stop waiting")
            .expect("retracted promoted owner should not panic");
        assert!(matches!(retracted_result, Err(HlsRangeError::Preempted)));
        wait_for_state(&cache, |state| state.waiting_foreground == 1).await;
        {
            let state = cache.lock_state();
            assert_eq!(2, state.active_chunk_permits);
            assert_eq!(0, state.waiting_preparation);
        }

        drop(holding_permit0);
        assert_eq!(
            Some(1),
            tokio::time::timeout(Duration::from_secs(2), acquired_rx.recv())
                .await
                .expect("remaining promoted owner should acquire the released slot")
        );
        {
            let state = cache.lock_state();
            assert_eq!(2, state.active_chunk_permits);
            assert_eq!(0, state.waiting_foreground);
            assert_eq!(2, state.active_foreground_permits);
        }
        let _ = release_tx1.send(());
        tokio::time::timeout(Duration::from_secs(2), task1)
            .await
            .expect("remaining promoted owner should finish")
            .expect("remaining promoted owner should not panic")
            .expect("remaining promoted owner should complete");
        drop(holding_permit1);
        wait_for_state(&cache, |state| state.active_chunk_permits == 0).await;
        {
            let state = cache.lock_state();
            assert_eq!(0, state.waiting_foreground);
            assert_eq!(0, state.waiting_preparation);
            assert_eq!(0, state.active_foreground_permits);
        }
        assert_eq!(2, cache.chunk_permits.available_permits());
        drop(foreground_follower1);
        drop(preparation_flight0);
        drop(preparation_flight1);
    }
}
