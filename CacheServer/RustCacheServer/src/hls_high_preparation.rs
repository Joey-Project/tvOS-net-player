use std::{
    collections::HashMap,
    ops::Range,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{
    AppState,
    bbdown_adapter::{
        BilibiliPlaybackAbrLevel, BilibiliPlaybackVariant, BilibiliPlaybackVariantKind,
    },
    bilibili_playback::BilibiliContentIdentity,
    hls::{
        HlsAbrGroupKind, HlsMediaResource, HlsMediaResourceMetadata, HlsPlaybackSession,
        HlsVariant, HlsVariantMetadata,
    },
    hls_cache::{
        HlsCacheFillControl, PlaybackPreparationBatch, PlaybackPreparationError,
        PlaybackPreparationRequest,
    },
    hls_playback_context::{PlaybackContextSnapshot, PlaybackTimingSample, is_canonical_uuid_v4},
    hls_playback_progress::PlaybackProgressIntent,
    hls_range_cache::HlsRangeError,
    hls_readiness::{CacheWindowReason, CacheWindowState, PlaybackWindow},
    playback_policy::variant_is_avplayer_h264_aac_hls_compatible,
    playback_request_urgency::PlaybackSidxMap,
};

const HIGH_PREPARATION_HISTORY_LIMIT: usize = 16;
const HIGH_PREPARATION_TRACKED_READER_LIMIT: usize = 64;
const HIGH_PREPARATION_GLOBAL_JOB_LIMIT: usize = 2;
const HIGH_PREPARATION_READER_STATE_TTL: Duration = Duration::from_secs(60);
const HIGH_PREPARATION_COOLDOWN: Duration = Duration::from_secs(10);
const HIGH_PREPARATION_DEFER_COOLDOWN: Duration = Duration::from_secs(2);
const HIGH_PREPARATION_ALREADY_READY_COOLDOWN: Duration = Duration::from_secs(15);
const HIGH_PREPARATION_JOB_DEADLINE: Duration = Duration::from_secs(15);
const HIGH_PREPARATION_PROTECTED_WALL_SECONDS: f64 = 20.0;
const HIGH_PREPARATION_MEDIA_WINDOW_SECONDS: f64 = 20.0;
const PLAYHEAD_SEEK_TOLERANCE_SECONDS: f64 = 3.0;

#[derive(Clone)]
pub(crate) struct HighPreparationCoordinator {
    inner: Arc<HighPreparationCoordinatorInner>,
}

struct HighPreparationCoordinatorInner {
    enabled: AtomicBool,
    #[cfg(test)]
    job_deadline_millis: std::sync::atomic::AtomicU64,
    state: Mutex<HighPreparationCoordinatorState>,
    global_jobs: Arc<Semaphore>,
}

#[derive(Default)]
struct HighPreparationCoordinatorState {
    next_plan_id: u64,
    readers: HashMap<String, HighPreparationReaderState>,
    active_plans: HashMap<u64, ActivePlanMetrics>,
    observation: HighPreparationObservation,
}

struct ActivePlanMetrics {
    requested_resource_ranges: u32,
    reader_id: String,
}

#[derive(Default)]
struct HighPreparationReaderState {
    epoch: u64,
    active_plan_id: Option<u64>,
    cooldown_until: Option<Instant>,
    last_seen: Option<Instant>,
    last_sample: Option<ObservedReaderSample>,
}

#[derive(Clone)]
struct ObservedReaderSample {
    context_id: String,
    session_id: String,
    session_generation: u64,
    sample: PlaybackTimingSample,
    observed_at: Instant,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct HighPreparationObservation {
    pub(crate) admitted_plans: u64,
    /// A plan is complete only when the selected logical window is readable.
    pub(crate) completed_plans: u64,
    pub(crate) deferred_plans: u64,
    /// Logical resource ranges submitted to the existing preparation batch, not origin attempts.
    pub(crate) requested_resource_ranges: u64,
    /// Selected resource ranges readable at verification; a foreground flight may have supplied them.
    pub(crate) readable_resource_ranges: u64,
    pub(crate) deferred_resource_ranges: u64,
    pub(crate) plan_history: Vec<HighPreparationPlanObservation>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HighPreparationPlanObservation {
    pub(crate) plan_id: u64,
    pub(crate) context_id: Option<String>,
    pub(crate) http_reader_id: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) session_generation: Option<u64>,
    pub(crate) target_variant_id: Option<String>,
    pub(crate) window_start_seconds: Option<f64>,
    pub(crate) window_end_seconds: Option<f64>,
    pub(crate) initial_window_readable: Option<bool>,
    pub(crate) selected_spans: Vec<HighPreparationResourceSpanObservation>,
    pub(crate) outcome: HighPreparationPlanOutcome,
    pub(crate) defer_reason: Option<HighPreparationDeferReason>,
    /// Count passed to the cache batch API; not a wire-request or transferred-byte count.
    pub(crate) requested_resource_ranges: u32,
    /// Readability result only; it does not attribute origin bytes to this plan.
    pub(crate) readable_resource_ranges: u32,
    pub(crate) deferred_resource_ranges: u32,
    pub(crate) window_readable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HighPreparationResourceKind {
    Video,
    Audio,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HighPreparationResourceSpanObservation {
    pub(crate) kind: HighPreparationResourceKind,
    pub(crate) start_seconds: f64,
    pub(crate) end_seconds: f64,
    pub(crate) byte_length: u64,
    pub(crate) readable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HighPreparationPlanOutcome {
    Admitted,
    Completed,
    Deferred,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HighPreparationDeferReason {
    Disabled,
    InvalidReader,
    NoCurrentContext,
    SessionUnavailable,
    MissingContentIdentity,
    IneligiblePrimary,
    InsufficientBuffer,
    MissingOriginSupply,
    MissingSidxMap,
    UnsafeWindow,
    RangeBatchLimit,
    NoRuntime,
    ReaderStateLimit,
    Cooldown,
    AlreadyActive,
    GlobalJobLimit,
    WindowAlreadyReadable,
    WindowNoLongerRelevant,
    CacheInspectionUnavailable,
    JobDeadlineExceeded,
    OwnershipChanged,
    Seek,
    PausedOrStopped,
    Preempted,
    RangeFailure,
    WindowNotReadable,
}

impl Default for HighPreparationCoordinator {
    fn default() -> Self {
        Self {
            inner: Arc::new(HighPreparationCoordinatorInner {
                enabled: AtomicBool::new(true),
                #[cfg(test)]
                job_deadline_millis: std::sync::atomic::AtomicU64::new(0),
                state: Mutex::new(HighPreparationCoordinatorState::default()),
                global_jobs: Arc::new(Semaphore::new(HIGH_PREPARATION_GLOBAL_JOB_LIMIT)),
            }),
        }
    }
}

impl HighPreparationCoordinator {
    #[cfg(test)]
    pub(crate) fn set_enabled_for_tests(&self, enabled: bool) {
        self.inner.enabled.store(enabled, Ordering::Release);
    }

    #[cfg(test)]
    fn set_job_deadline_for_tests(&self, deadline: Duration) {
        self.inner.job_deadline_millis.store(
            deadline.as_millis().min(u128::from(u64::MAX)) as u64,
            Ordering::Release,
        );
    }

    #[cfg(test)]
    pub(crate) fn observation_for_tests(&self) -> HighPreparationObservation {
        let state = self
            .inner
            .state
            .lock()
            .expect("HLS high-preparation coordinator lock poisoned");
        state.observation.clone()
    }

    fn enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Acquire)
    }

    fn job_deadline(&self) -> Duration {
        #[cfg(test)]
        {
            let deadline_millis = self.inner.job_deadline_millis.load(Ordering::Acquire);
            if deadline_millis != 0 {
                return Duration::from_millis(deadline_millis);
            }
        }
        HIGH_PREPARATION_JOB_DEADLINE
    }

    fn observe_reader_sample(
        &self,
        reader_id: &str,
        snapshot: &PlaybackContextSnapshot,
    ) -> Option<(u64, bool)> {
        let now = Instant::now();
        let mut state = self
            .inner
            .state
            .lock()
            .expect("HLS high-preparation coordinator lock poisoned");
        state.readers.retain(|_, reader| {
            reader.active_plan_id.is_some()
                || reader.last_seen.is_some_and(|at| {
                    now.saturating_duration_since(at) < HIGH_PREPARATION_READER_STATE_TTL
                })
        });

        if !state.readers.contains_key(reader_id)
            && state.readers.len() >= HIGH_PREPARATION_TRACKED_READER_LIMIT
        {
            let oldest_idle = state
                .readers
                .iter()
                .filter(|(_, reader)| reader.active_plan_id.is_none())
                .min_by_key(|(_, reader)| reader.last_seen)
                .map(|(reader_id, _)| reader_id.clone());
            let oldest_idle = oldest_idle?;
            state.readers.remove(&oldest_idle);
        }

        let reader = state.readers.entry(reader_id.to_owned()).or_default();
        let mut seek_detected = false;
        if let Some(previous) = &reader.last_sample {
            let binding_changed = previous.context_id != snapshot.context_id
                || previous.session_id != snapshot.session_id
                || previous.session_generation != snapshot.session_generation;
            let elapsed = now
                .saturating_duration_since(previous.observed_at)
                .as_secs_f64();
            let expected_playhead =
                previous.sample.playhead_seconds + elapsed * previous.sample.playback_rate;
            let playhead_delta = snapshot.sample.playhead_seconds - expected_playhead;
            seek_detected = !binding_changed
                && playhead_delta.is_finite()
                && playhead_delta.abs() > PLAYHEAD_SEEK_TOLERANCE_SECONDS;
            if binding_changed || seek_detected {
                reader.epoch = reader.epoch.saturating_add(1);
                reader.cooldown_until = None;
            }
        }
        reader.last_sample = Some(ObservedReaderSample {
            context_id: snapshot.context_id.clone(),
            session_id: snapshot.session_id.clone(),
            session_generation: snapshot.session_generation,
            sample: snapshot.sample,
            observed_at: now,
        });
        reader.last_seen = Some(now);
        Some((reader.epoch, seek_detected))
    }

    fn reserve_job(
        &self,
        reader_id: &str,
        epoch: u64,
    ) -> Result<(u64, OwnedSemaphorePermit), HighPreparationDeferReason> {
        let now = Instant::now();
        let mut state = self
            .inner
            .state
            .lock()
            .expect("HLS high-preparation coordinator lock poisoned");
        let reader = state
            .readers
            .get_mut(reader_id)
            .ok_or(HighPreparationDeferReason::ReaderStateLimit)?;
        if reader.epoch != epoch {
            return Err(HighPreparationDeferReason::Seek);
        }
        if reader.active_plan_id.is_some() {
            return Err(HighPreparationDeferReason::AlreadyActive);
        }
        if reader.cooldown_until.is_some_and(|until| now < until) {
            return Err(HighPreparationDeferReason::Cooldown);
        }
        let permit = Arc::clone(&self.inner.global_jobs)
            .try_acquire_owned()
            .map_err(|_| HighPreparationDeferReason::GlobalJobLimit)?;
        state.next_plan_id = state.next_plan_id.saturating_add(1).max(1);
        let plan_id = state.next_plan_id;
        let reader = state
            .readers
            .get_mut(reader_id)
            .expect("reader state remains present while coordinator lock is held");
        reader.active_plan_id = Some(plan_id);
        reader.cooldown_until = now.checked_add(HIGH_PREPARATION_COOLDOWN);
        reader.last_seen = Some(now);
        Ok((plan_id, permit))
    }

    fn record_deferred(
        &self,
        mut observation: HighPreparationPlanObservation,
        reason: HighPreparationDeferReason,
        cooldown: Duration,
    ) {
        let now = Instant::now();
        let mut state = self
            .inner
            .state
            .lock()
            .expect("HLS high-preparation coordinator lock poisoned");
        state.next_plan_id = state.next_plan_id.saturating_add(1).max(1);
        observation.plan_id = state.next_plan_id;
        observation.outcome = HighPreparationPlanOutcome::Deferred;
        observation.defer_reason = Some(reason);
        observation.deferred_resource_ranges = observation
            .requested_resource_ranges
            .saturating_sub(observation.readable_resource_ranges);
        state.observation.deferred_plans = state.observation.deferred_plans.saturating_add(1);
        state.observation.deferred_resource_ranges = state
            .observation
            .deferred_resource_ranges
            .saturating_add(u64::from(observation.deferred_resource_ranges));
        if let Some(reader_id) = observation.http_reader_id.as_deref()
            && let Some(reader) = state.readers.get_mut(reader_id)
        {
            reader.last_seen = Some(now);
            if !cooldown.is_zero() {
                reader.cooldown_until = now.checked_add(cooldown);
            }
        }
        push_observation(&mut state.observation, observation);
    }

    fn record_admitted(&self, observation: HighPreparationPlanObservation) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("HLS high-preparation coordinator lock poisoned");
        state.observation.admitted_plans = state.observation.admitted_plans.saturating_add(1);
        state.active_plans.insert(
            observation.plan_id,
            ActivePlanMetrics {
                requested_resource_ranges: observation.requested_resource_ranges,
                reader_id: observation.http_reader_id.clone().unwrap_or_default(),
            },
        );
        push_observation(&mut state.observation, observation);
    }

    fn record_initial_window_readability(&self, plan_id: u64, readable: Option<bool>) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("HLS high-preparation coordinator lock poisoned");
        if let Some(observation) = find_observation_mut(&mut state.observation, plan_id) {
            observation.initial_window_readable = readable;
        }
    }

    fn record_requested_resource_ranges(&self, plan_id: u64, count: usize) {
        let count = count.min(u32::MAX as usize) as u32;
        let mut state = self
            .inner
            .state
            .lock()
            .expect("HLS high-preparation coordinator lock poisoned");
        state.observation.requested_resource_ranges = state
            .observation
            .requested_resource_ranges
            .saturating_add(u64::from(count));
        if let Some(metrics) = state.active_plans.get_mut(&plan_id) {
            metrics.requested_resource_ranges = count;
        }
        if let Some(observation) = find_observation_mut(&mut state.observation, plan_id) {
            observation.requested_resource_ranges = count;
        }
    }

    fn finish_plan(
        &self,
        plan_id: u64,
        outcome: HighPreparationPlanOutcome,
        reason: Option<HighPreparationDeferReason>,
        readable_resource_ranges: usize,
        window_readable: bool,
        readable_spans: &[bool],
    ) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("HLS high-preparation coordinator lock poisoned");
        let Some(metrics) = state.active_plans.remove(&plan_id) else {
            return;
        };
        let readable_resource_ranges = readable_resource_ranges.min(u32::MAX as usize) as u32;
        let deferred_resource_ranges = metrics
            .requested_resource_ranges
            .saturating_sub(readable_resource_ranges);
        if let Some(observation) = find_observation_mut(&mut state.observation, plan_id) {
            observation.outcome = outcome;
            observation.defer_reason = reason;
            observation.readable_resource_ranges = readable_resource_ranges;
            observation.deferred_resource_ranges = deferred_resource_ranges;
            observation.window_readable = window_readable;
            for (span, readable) in observation.selected_spans.iter_mut().zip(readable_spans) {
                span.readable = *readable;
            }
        }
        state.observation.readable_resource_ranges = state
            .observation
            .readable_resource_ranges
            .saturating_add(u64::from(readable_resource_ranges));
        state.observation.deferred_resource_ranges = state
            .observation
            .deferred_resource_ranges
            .saturating_add(u64::from(deferred_resource_ranges));
        match outcome {
            HighPreparationPlanOutcome::Completed => {
                state.observation.completed_plans =
                    state.observation.completed_plans.saturating_add(1);
            }
            HighPreparationPlanOutcome::Deferred
            | HighPreparationPlanOutcome::Cancelled
            | HighPreparationPlanOutcome::Failed => {
                state.observation.deferred_plans =
                    state.observation.deferred_plans.saturating_add(1);
            }
            HighPreparationPlanOutcome::Admitted => {}
        }
        if let Some(reader) = state.readers.get_mut(&metrics.reader_id) {
            let cooldown = match (outcome, reason) {
                (HighPreparationPlanOutcome::Completed, _) => HIGH_PREPARATION_COOLDOWN,
                (_, Some(HighPreparationDeferReason::WindowAlreadyReadable)) => {
                    HIGH_PREPARATION_ALREADY_READY_COOLDOWN
                }
                (
                    _,
                    Some(
                        HighPreparationDeferReason::Seek
                        | HighPreparationDeferReason::OwnershipChanged,
                    ),
                ) => Duration::ZERO,
                _ => HIGH_PREPARATION_DEFER_COOLDOWN,
            };
            reader.cooldown_until = Instant::now().checked_add(cooldown);
        }
    }

    fn is_job_active(&self, reader_id: &str, plan_id: u64, epoch: u64) -> bool {
        self.inner
            .state
            .lock()
            .expect("HLS high-preparation coordinator lock poisoned")
            .readers
            .get(reader_id)
            .is_some_and(|reader| reader.active_plan_id == Some(plan_id) && reader.epoch == epoch)
    }

    fn finish_reader_job(&self, reader_id: &str, plan_id: u64) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("HLS high-preparation coordinator lock poisoned");
        if let Some(reader) = state.readers.get_mut(reader_id)
            && reader.active_plan_id == Some(plan_id)
        {
            reader.active_plan_id = None;
            reader.last_seen = Some(Instant::now());
        }
    }
}

fn push_observation(
    observation: &mut HighPreparationObservation,
    plan: HighPreparationPlanObservation,
) {
    if observation.plan_history.len() >= HIGH_PREPARATION_HISTORY_LIMIT {
        if let Some(evicted) = observation
            .plan_history
            .iter()
            .position(|existing| existing.outcome != HighPreparationPlanOutcome::Admitted)
        {
            observation.plan_history.remove(evicted);
        } else {
            return;
        }
    }
    observation.plan_history.push(plan);
}

fn find_observation_mut(
    observation: &mut HighPreparationObservation,
    plan_id: u64,
) -> Option<&mut HighPreparationPlanObservation> {
    observation
        .plan_history
        .iter_mut()
        .find(|plan| plan.plan_id == plan_id)
}

#[derive(Clone)]
struct PreparedHighResource {
    resource: HlsMediaResource,
    map: Arc<PlaybackSidxMap>,
    bytes: Range<u64>,
}

#[derive(Clone)]
struct HighPreparationPlan {
    plan_id: u64,
    epoch: u64,
    context_id: String,
    reader_id: String,
    session_id: String,
    session_generation: u64,
    accepted_identity: BilibiliContentIdentity,
    target: HlsVariant,
    target_variant_id: String,
    window: PlaybackWindow,
    resources: Vec<PreparedHighResource>,
}

struct HighPreparationReaderJobGuard {
    coordinator: HighPreparationCoordinator,
    reader_id: String,
    plan_id: u64,
}

impl Drop for HighPreparationReaderJobGuard {
    fn drop(&mut self) {
        self.coordinator.finish_plan(
            self.plan_id,
            HighPreparationPlanOutcome::Failed,
            Some(HighPreparationDeferReason::RangeFailure),
            0,
            false,
            &[],
        );
        self.coordinator
            .finish_reader_job(&self.reader_id, self.plan_id);
    }
}

struct HighPreparationDraft {
    epoch: u64,
    context_id: String,
    reader_id: String,
    session_id: String,
    session_generation: u64,
    accepted_identity: BilibiliContentIdentity,
    target: HlsVariant,
    window: PlaybackWindow,
    resources: Vec<PreparedHighResource>,
}

impl AppState {
    pub(crate) fn maybe_start_hls_high_preparation(&self, http_reader_id: &str) {
        let coordinator = &self.hls_high_preparation;
        let mut observation = empty_plan_observation(http_reader_id);
        if !is_canonical_uuid_v4(http_reader_id) {
            coordinator.record_deferred(
                observation,
                HighPreparationDeferReason::InvalidReader,
                Duration::ZERO,
            );
            return;
        }

        let Some(snapshot) = self.current_hls_playback_context_for_reader(http_reader_id) else {
            coordinator.record_deferred(
                observation,
                HighPreparationDeferReason::NoCurrentContext,
                Duration::ZERO,
            );
            return;
        };
        observation = observation_for_context(http_reader_id, &snapshot);

        if !coordinator.enabled() {
            coordinator.record_deferred(
                observation,
                HighPreparationDeferReason::Disabled,
                Duration::ZERO,
            );
            return;
        }

        let Some((epoch, _seek_detected)) =
            coordinator.observe_reader_sample(http_reader_id, &snapshot)
        else {
            coordinator.record_deferred(
                observation,
                HighPreparationDeferReason::ReaderStateLimit,
                Duration::ZERO,
            );
            return;
        };

        let draft = match build_high_preparation_draft(
            self,
            http_reader_id,
            &snapshot,
            epoch,
            &mut observation,
        ) {
            Ok(draft) => draft,
            Err(reason) => {
                coordinator.record_deferred(observation, reason, HIGH_PREPARATION_DEFER_COOLDOWN);
                return;
            }
        };

        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            coordinator.record_deferred(
                observation,
                HighPreparationDeferReason::NoRuntime,
                Duration::ZERO,
            );
            return;
        };
        let (plan_id, permit) = match coordinator.reserve_job(http_reader_id, epoch) {
            Ok(reservation) => reservation,
            Err(reason) => {
                coordinator.record_deferred(observation, reason, Duration::ZERO);
                return;
            }
        };

        let target_variant_id = draft.target.id.clone();
        observation.plan_id = plan_id;
        observation.target_variant_id = Some(safe_observation_id(&target_variant_id));
        observation.window_start_seconds = Some(draft.window.start_seconds);
        observation.window_end_seconds = Some(draft.window.end_seconds());
        observation.outcome = HighPreparationPlanOutcome::Admitted;
        observation.defer_reason = None;
        let plan = HighPreparationPlan {
            plan_id,
            epoch: draft.epoch,
            context_id: draft.context_id,
            reader_id: draft.reader_id,
            session_id: draft.session_id,
            session_generation: draft.session_generation,
            accepted_identity: draft.accepted_identity,
            target: draft.target,
            target_variant_id,
            window: draft.window,
            resources: draft.resources,
        };
        coordinator.record_admitted(observation);
        let state = Arc::new(self.clone());
        let coordinator = coordinator.clone();
        runtime.spawn(async move {
            run_high_preparation(state, coordinator, plan, permit).await;
        });
    }
}

fn empty_plan_observation(reader_id: &str) -> HighPreparationPlanObservation {
    HighPreparationPlanObservation {
        plan_id: 0,
        context_id: None,
        http_reader_id: safe_observation_id_option(reader_id),
        session_id: None,
        session_generation: None,
        target_variant_id: None,
        window_start_seconds: None,
        window_end_seconds: None,
        initial_window_readable: None,
        selected_spans: Vec::new(),
        outcome: HighPreparationPlanOutcome::Deferred,
        defer_reason: None,
        requested_resource_ranges: 0,
        readable_resource_ranges: 0,
        deferred_resource_ranges: 0,
        window_readable: false,
    }
}

fn observation_for_context(
    reader_id: &str,
    snapshot: &PlaybackContextSnapshot,
) -> HighPreparationPlanObservation {
    let mut observation = empty_plan_observation(reader_id);
    observation.context_id = safe_observation_id_option(&snapshot.context_id);
    observation.session_id = safe_observation_id_option(&snapshot.session_id);
    observation.session_generation = Some(snapshot.session_generation);
    observation
}

fn safe_observation_id_option(value: &str) -> Option<String> {
    Some(safe_observation_id(value))
}

fn safe_observation_id(value: &str) -> String {
    if value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        value.to_owned()
    } else {
        "redacted".to_owned()
    }
}

fn build_high_preparation_draft(
    state: &AppState,
    reader_id: &str,
    snapshot: &PlaybackContextSnapshot,
    epoch: u64,
    observation: &mut HighPreparationPlanObservation,
) -> Result<HighPreparationDraft, HighPreparationDeferReason> {
    let handle = state
        .hls_sessions
        .get_with_generation(&snapshot.session_id)
        .filter(|handle| handle.generation == snapshot.session_generation)
        .ok_or(HighPreparationDeferReason::SessionUnavailable)?;
    let accepted_identity = handle
        .session
        .accepted_identity
        .clone()
        .filter(BilibiliContentIdentity::is_complete)
        .ok_or(HighPreparationDeferReason::MissingContentIdentity)?;
    let target = select_advertised_primary_target(state, &handle.session, handle.generation)
        .ok_or(HighPreparationDeferReason::IneligiblePrimary)?;
    observation.target_variant_id = safe_observation_id_option(&target.id);

    let desired_start =
        safe_future_start(snapshot.sample).ok_or(HighPreparationDeferReason::InsufficientBuffer)?;
    let mut source_resources = vec![target.video.clone()];
    if let Some(audio) = &target.audio {
        source_resources.push(audio.clone());
    }
    let mut mapped_resources = Vec::with_capacity(source_resources.len());
    let mut mapped_end = f64::INFINITY;
    for resource in source_resources {
        let map = state
            .hls_cache
            .playback_sidx_map(&snapshot.session_id, snapshot.session_generation, &resource)
            .ok_or(HighPreparationDeferReason::MissingSidxMap)?;
        let end = map
            .media_end_seconds()
            .ok_or(HighPreparationDeferReason::MissingSidxMap)?;
        mapped_end = mapped_end.min(end);
        mapped_resources.push((resource, map));
    }
    let requested_end = desired_start + HIGH_PREPARATION_MEDIA_WINDOW_SECONDS;
    if !requested_end.is_finite() || requested_end <= desired_start {
        return Err(HighPreparationDeferReason::UnsafeWindow);
    }
    let window_end = requested_end.min(mapped_end);
    let window = PlaybackWindow::new(desired_start, window_end - desired_start)
        .ok_or(HighPreparationDeferReason::UnsafeWindow)?;
    observation.window_start_seconds = Some(window.start_seconds);
    observation.window_end_seconds = Some(window.end_seconds());

    let mut prepared_resources = Vec::with_capacity(mapped_resources.len());
    for (resource, map) in mapped_resources {
        let selected = map
            .preparation_range_for_window(window.start_seconds, window.end_seconds())
            .ok_or(HighPreparationDeferReason::UnsafeWindow)?;
        let kind = match resource.request.kind {
            crate::bbdown_adapter::BilibiliMediaRequestKind::Video => {
                HighPreparationResourceKind::Video
            }
            crate::bbdown_adapter::BilibiliMediaRequestKind::Audio => {
                HighPreparationResourceKind::Audio
            }
            crate::bbdown_adapter::BilibiliMediaRequestKind::FlvSegment => {
                return Err(HighPreparationDeferReason::IneligiblePrimary);
            }
        };
        observation
            .selected_spans
            .push(HighPreparationResourceSpanObservation {
                kind,
                start_seconds: selected.start_seconds,
                end_seconds: selected.end_seconds,
                byte_length: selected.bytes.end - selected.bytes.start,
                readable: false,
            });
        prepared_resources.push(PreparedHighResource {
            resource,
            map,
            bytes: selected.bytes,
        });
    }

    for prepared in &prepared_resources {
        let required_bitrate = prepared
            .resource
            .request
            .bandwidth
            .ok_or(HighPreparationDeferReason::MissingOriginSupply)?;
        let supply = state
            .cdn_history
            .recent_origin_supply(&prepared.resource.request)
            .ok_or(HighPreparationDeferReason::MissingOriginSupply)?;
        if !optional_preparation_supply_sufficient(
            supply.throughput_bps,
            required_bitrate,
            snapshot.sample.playback_rate,
        ) {
            return Err(HighPreparationDeferReason::MissingOriginSupply);
        }
    }

    let requests = prepared_resources
        .iter()
        .map(|prepared| PlaybackPreparationRequest {
            resource: &prepared.resource,
            requested: prepared.bytes.clone(),
        })
        .collect::<Vec<_>>();
    state
        .hls_cache
        .validate_playback_preparation_batch(&PlaybackPreparationBatch {
            session_id: &snapshot.session_id,
            requests: &requests,
        })
        .map_err(|_| HighPreparationDeferReason::RangeBatchLimit)?;

    Ok(HighPreparationDraft {
        epoch,
        context_id: snapshot.context_id.clone(),
        reader_id: reader_id.to_owned(),
        session_id: snapshot.session_id.clone(),
        session_generation: snapshot.session_generation,
        accepted_identity,
        target,
        window,
        resources: prepared_resources,
    })
}

fn safe_future_start(sample: PlaybackTimingSample) -> Option<f64> {
    if !sample.playhead_seconds.is_finite()
        || sample.playhead_seconds < 0.0
        || !sample.continuous_buffered_end_seconds.is_finite()
        || sample.continuous_buffered_end_seconds < sample.playhead_seconds
        || !sample.playback_rate.is_finite()
        || sample.playback_rate <= 0.0
        || sample.playback_rate > 16.0
    {
        return None;
    }
    let protected_end =
        sample.playhead_seconds + HIGH_PREPARATION_PROTECTED_WALL_SECONDS * sample.playback_rate;
    if !protected_end.is_finite() || sample.continuous_buffered_end_seconds < protected_end {
        return None;
    }
    Some(protected_end.max(sample.continuous_buffered_end_seconds))
}

fn optional_preparation_supply_sufficient(
    throughput_bps: u64,
    nominal_bitrate_bps: u64,
    playback_rate: f64,
) -> bool {
    const RATE_SCALE: u128 = 1_000_000;
    if throughput_bps == 0
        || nominal_bitrate_bps == 0
        || !playback_rate.is_finite()
        || playback_rate <= 0.0
        || playback_rate > 16.0
    {
        return false;
    }
    let scaled_rate = (playback_rate * RATE_SCALE as f64).ceil();
    if !scaled_rate.is_finite() || scaled_rate <= 0.0 || scaled_rate >= u64::MAX as f64 {
        return false;
    }
    let Some(numerator) = u128::from(nominal_bitrate_bps)
        .checked_mul(scaled_rate as u128)
        .and_then(|value| value.checked_mul(3))
    else {
        return false;
    };
    let denominator = RATE_SCALE * 2;
    let Some(rounded_numerator) = numerator.checked_add(denominator - 1) else {
        return false;
    };
    let required = rounded_numerator / denominator;
    u64::try_from(required).is_ok_and(|required| throughput_bps >= required)
}

fn select_advertised_primary_target(
    state: &AppState,
    session: &HlsPlaybackSession,
    generation: u64,
) -> Option<HlsVariant> {
    let master_session =
        session.prepare_master_playlist_with_resource_availability(|resource_id| {
            state
                .hls_cache
                .open_cached_resource(&session.id, resource_id)
                .is_some()
        })?;
    let preference = session.effective_policy.weak_network_preference;
    let advertised_ids = master_session
        .advertised_variants_with_filter(|variant| {
            state.hls_network_policy.variant_is_advertisable_for_policy(
                preference,
                &session.id,
                generation,
                &variant.id,
            )
        })
        .into_iter()
        .map(|variant| variant.id.clone())
        .collect::<Vec<_>>();

    let primary = &session.variant;
    if !advertised_ids.iter().any(|id| id == &primary.id) {
        return None;
    }
    let primary_metadata = unique_variant_metadata(session, &primary.id)?;
    let primary_abr = primary_metadata.abr.as_ref()?;
    if primary_metadata.kind != BilibiliPlaybackVariantKind::Dash
        || !primary_abr.switchable
        || primary_abr.level_count < 2
    {
        return None;
    }
    let group = session
        .abr
        .groups
        .iter()
        .find(|group| group.id == primary_abr.group_id)?;
    if group.kind != HlsAbrGroupKind::DashVideo
        || group.level_count < 2
        || !group.variant_ids.iter().any(|id| id == &primary.id)
        || !group.variant_ids.iter().any(|id| id != &primary.id)
    {
        return None;
    }
    let primary_adapter = adapter_variant_from_session(primary_metadata, primary)?;
    if !variant_is_avplayer_h264_aac_hls_compatible(&primary_adapter) {
        return None;
    }

    let has_advertised_sibling = session.alternate_variants.iter().any(|alternate| {
        if alternate.id == primary.id || !advertised_ids.iter().any(|id| id == &alternate.id) {
            return false;
        }
        let Some(metadata) = unique_variant_metadata(session, &alternate.id) else {
            return false;
        };
        let Some(level) = metadata.abr.as_ref() else {
            return false;
        };
        if metadata.kind != BilibiliPlaybackVariantKind::Dash
            || !level.switchable
            || level.level_count < 2
            || level.group_id != primary_abr.group_id
            || !group.variant_ids.iter().any(|id| id == &alternate.id)
            || primary.audio.is_some() != alternate.audio.is_some()
        {
            return false;
        }
        adapter_variant_from_session(metadata, alternate)
            .is_some_and(|variant| variant_is_avplayer_h264_aac_hls_compatible(&variant))
    });
    has_advertised_sibling.then(|| primary.clone())
}

fn unique_variant_metadata<'a>(
    session: &'a HlsPlaybackSession,
    variant_id: &str,
) -> Option<&'a HlsVariantMetadata> {
    let mut matching = session
        .variants
        .iter()
        .filter(|variant| variant.id == variant_id);
    let metadata = matching.next()?;
    matching.next().is_none().then_some(metadata)
}

fn adapter_variant_from_session(
    metadata: &HlsVariantMetadata,
    variant: &HlsVariant,
) -> Option<BilibiliPlaybackVariant> {
    if metadata.id != variant.id || metadata.kind != BilibiliPlaybackVariantKind::Dash {
        return None;
    }
    let video_metadata = unique_media_metadata(
        metadata,
        crate::bbdown_adapter::BilibiliMediaRequestKind::Video,
    )?;
    if !resource_metadata_matches(&variant.video.request, video_metadata) {
        return None;
    }
    let audio = match (
        &variant.audio,
        unique_media_metadata_optional(
            metadata,
            crate::bbdown_adapter::BilibiliMediaRequestKind::Audio,
        ),
    ) {
        (Some(audio), Some(audio_metadata))
            if resource_metadata_matches(&audio.request, audio_metadata) =>
        {
            Some(audio.request.clone())
        }
        (None, None) => None,
        _ => return None,
    };
    let expected_media_count = 1 + usize::from(audio.is_some());
    if metadata.media.len() != expected_media_count {
        return None;
    }
    Some(BilibiliPlaybackVariant {
        id: metadata.id.clone(),
        kind: metadata.kind,
        content_id: metadata.content_id.clone(),
        bandwidth: metadata.bandwidth,
        codecs: metadata.codecs.clone(),
        mime_types: metadata.mime_types.clone(),
        width: metadata.width,
        height: metadata.height,
        frame_rate: metadata.frame_rate.clone(),
        duration_seconds: metadata.duration_seconds,
        abr: metadata.abr.as_ref().map(|level| BilibiliPlaybackAbrLevel {
            group_id: level.group_id.clone(),
            level_index: level.level_index,
            level_count: level.level_count,
            switchable: level.switchable,
        }),
        video: Some(variant.video.request.clone()),
        audio,
        flv_segments: Vec::new(),
    })
}

fn unique_media_metadata(
    variant: &HlsVariantMetadata,
    kind: crate::bbdown_adapter::BilibiliMediaRequestKind,
) -> Option<&HlsMediaResourceMetadata> {
    unique_media_metadata_optional(variant, kind)
}

fn unique_media_metadata_optional(
    variant: &HlsVariantMetadata,
    kind: crate::bbdown_adapter::BilibiliMediaRequestKind,
) -> Option<&HlsMediaResourceMetadata> {
    let mut matching = variant.media.iter().filter(|media| media.kind == kind);
    let metadata = matching.next()?;
    matching.next().is_none().then_some(metadata)
}

fn resource_metadata_matches(
    request: &crate::bbdown_adapter::BilibiliMediaRequest,
    metadata: &HlsMediaResourceMetadata,
) -> bool {
    metadata.kind == request.kind
        && metadata.stream_id == request.stream_id
        && metadata.mime_type == request.mime_type
        && metadata.codecs == request.codecs
        && metadata.bandwidth == request.bandwidth
        && metadata.width == request.width
        && metadata.height == request.height
        && metadata.frame_rate == request.frame_rate
        && metadata.size == request.size
        && metadata.duration_seconds == request.duration_seconds
        && metadata.cache_key == request.cache_key
}

async fn run_high_preparation(
    state: Arc<AppState>,
    coordinator: HighPreparationCoordinator,
    plan: HighPreparationPlan,
    permit: OwnedSemaphorePermit,
) {
    let _permit = permit;
    let _job_guard = HighPreparationReaderJobGuard {
        coordinator: coordinator.clone(),
        reader_id: plan.reader_id.clone(),
        plan_id: plan.plan_id,
    };
    if tokio::time::timeout(
        coordinator.job_deadline(),
        run_high_preparation_inner(Arc::clone(&state), coordinator.clone(), plan.clone()),
    )
    .await
    .is_err()
    {
        coordinator.finish_plan(
            plan.plan_id,
            HighPreparationPlanOutcome::Failed,
            Some(HighPreparationDeferReason::JobDeadlineExceeded),
            0,
            false,
            &vec![false; plan.resources.len()],
        );
    }
}

async fn run_high_preparation_inner(
    state: Arc<AppState>,
    coordinator: HighPreparationCoordinator,
    plan: HighPreparationPlan,
) {
    let initial_inspection = state
        .hls_cache
        .inspect_cached_playback_window(&plan.session_id, &plan.target, plan.window)
        .await;
    let initial_window_readable = match initial_inspection.state {
        CacheWindowState::Readable => Some(true),
        CacheWindowState::Missing => Some(false),
        CacheWindowState::Unknown => None,
    };
    coordinator.record_initial_window_readability(plan.plan_id, initial_window_readable);
    match initial_inspection.state {
        CacheWindowState::Readable => {
            coordinator.finish_plan(
                plan.plan_id,
                HighPreparationPlanOutcome::Deferred,
                Some(HighPreparationDeferReason::WindowAlreadyReadable),
                plan.resources.len(),
                true,
                &vec![true; plan.resources.len()],
            );
            return;
        }
        CacheWindowState::Missing if initial_inspection.reason == CacheWindowReason::NotCached => {}
        CacheWindowState::Missing | CacheWindowState::Unknown => {
            coordinator.finish_plan(
                plan.plan_id,
                HighPreparationPlanOutcome::Deferred,
                Some(HighPreparationDeferReason::CacheInspectionUnavailable),
                0,
                false,
                &vec![false; plan.resources.len()],
            );
            return;
        }
    }

    let cancel_reason = Arc::new(Mutex::new(None));
    let control_state = Arc::clone(&state);
    let control_coordinator = coordinator.clone();
    let control_plan = plan.clone();
    let control_cancel_reason = Arc::clone(&cancel_reason);
    let control =
        move || match plan_control_reason(&control_state, &control_coordinator, &control_plan) {
            None => HlsCacheFillControl::Continue,
            Some(reason) => {
                *control_cancel_reason
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
                HlsCacheFillControl::Cancel
            }
        };

    let requests = plan
        .resources
        .iter()
        .map(|prepared| PlaybackPreparationRequest {
            resource: &prepared.resource,
            requested: prepared.bytes.clone(),
        })
        .collect::<Vec<_>>();
    coordinator.record_requested_resource_ranges(plan.plan_id, requests.len());
    let preparation_result = state
        .hls_cache
        .prepare_playback_batch(
            &state.hls_upstream_client,
            PlaybackPreparationBatch {
                session_id: &plan.session_id,
                requests: &requests,
            },
            &control,
        )
        .await;

    let readable_spans = read_selected_ranges(&state, &plan).await;
    let readable_resource_ranges = readable_spans.iter().filter(|readable| **readable).count();
    let final_inspection = state
        .hls_cache
        .inspect_cached_playback_window(&plan.session_id, &plan.target, plan.window)
        .await;
    let window_readable = final_inspection.state == CacheWindowState::Readable;
    let current_reason = plan_control_reason(&state, &coordinator, &plan);
    let cancellation_reason = *cancel_reason
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let control_reason = current_reason.or(cancellation_reason);

    let (outcome, reason) = match preparation_result {
        Ok(_)
            if control_reason.is_none()
                && window_readable
                && readable_resource_ranges == plan.resources.len() =>
        {
            (HighPreparationPlanOutcome::Completed, None)
        }
        Ok(_) if let Some(reason) = control_reason => {
            (HighPreparationPlanOutcome::Cancelled, Some(reason))
        }
        Ok(_) => (
            HighPreparationPlanOutcome::Failed,
            Some(HighPreparationDeferReason::WindowNotReadable),
        ),
        Err(PlaybackPreparationError::Range(HlsRangeError::Preempted)) => (
            HighPreparationPlanOutcome::Deferred,
            Some(HighPreparationDeferReason::Preempted),
        ),
        Err(PlaybackPreparationError::Range(HlsRangeError::Cancelled))
            if let Some(reason) = control_reason =>
        {
            (HighPreparationPlanOutcome::Cancelled, Some(reason))
        }
        Err(PlaybackPreparationError::Range(
            HlsRangeError::SessionRemoving | HlsRangeError::IdentityChanged,
        )) => (
            HighPreparationPlanOutcome::Cancelled,
            Some(HighPreparationDeferReason::OwnershipChanged),
        ),
        Err(_) => (
            HighPreparationPlanOutcome::Failed,
            Some(HighPreparationDeferReason::RangeFailure),
        ),
    };
    coordinator.finish_plan(
        plan.plan_id,
        outcome,
        reason,
        readable_resource_ranges,
        window_readable,
        &readable_spans,
    );
}

async fn read_selected_ranges(state: &AppState, plan: &HighPreparationPlan) -> Vec<bool> {
    let mut readable = Vec::with_capacity(plan.resources.len());
    for prepared in &plan.resources {
        let expected_length = prepared.bytes.end.saturating_sub(prepared.bytes.start);
        let result = state
            .hls_cache
            .read_resource_range(&plan.session_id, &prepared.resource, prepared.bytes.clone())
            .await;
        readable.push(matches!(result, Ok(Some(bytes)) if bytes.len() as u64 == expected_length));
    }
    readable
}

fn plan_control_reason(
    state: &AppState,
    coordinator: &HighPreparationCoordinator,
    plan: &HighPreparationPlan,
) -> Option<HighPreparationDeferReason> {
    if !coordinator.enabled() {
        return Some(HighPreparationDeferReason::Disabled);
    }
    if !coordinator.is_job_active(&plan.reader_id, plan.plan_id, plan.epoch) {
        return Some(HighPreparationDeferReason::Seek);
    }
    let Some(snapshot) = state.current_hls_playback_context_for_reader(&plan.reader_id) else {
        return Some(HighPreparationDeferReason::OwnershipChanged);
    };
    if snapshot.context_id != plan.context_id
        || snapshot.session_id != plan.session_id
        || snapshot.session_generation != plan.session_generation
    {
        return Some(HighPreparationDeferReason::OwnershipChanged);
    }
    let Some(handle) = state
        .hls_sessions
        .get_with_generation(&plan.session_id)
        .filter(|handle| handle.generation == plan.session_generation)
    else {
        return Some(HighPreparationDeferReason::OwnershipChanged);
    };
    if handle.session.accepted_identity.as_ref() != Some(&plan.accepted_identity)
        || handle.session.variant != plan.target
    {
        return Some(HighPreparationDeferReason::OwnershipChanged);
    }
    let Some(current_target) =
        select_advertised_primary_target(state, &handle.session, handle.generation)
    else {
        return Some(HighPreparationDeferReason::IneligiblePrimary);
    };
    if current_target.id != plan.target_variant_id {
        return Some(HighPreparationDeferReason::IneligiblePrimary);
    }
    if state
        .hls_playback_progress
        .snapshot_for_session(&plan.session_id)
        .is_some_and(|progress| {
            matches!(
                progress.last_intent,
                PlaybackProgressIntent::Paused | PlaybackProgressIntent::Stopped
            )
        })
    {
        return Some(HighPreparationDeferReason::PausedOrStopped);
    }
    if safe_future_start(snapshot.sample).is_none() {
        return Some(HighPreparationDeferReason::InsufficientBuffer);
    }
    if plan.window.end_seconds() <= snapshot.sample.playhead_seconds {
        return Some(HighPreparationDeferReason::WindowNoLongerRelevant);
    }
    for prepared in &plan.resources {
        let Some(current_map) = state.hls_cache.playback_sidx_map(
            &plan.session_id,
            plan.session_generation,
            &prepared.resource,
        ) else {
            return Some(HighPreparationDeferReason::MissingSidxMap);
        };
        if !Arc::ptr_eq(&prepared.map, &current_map) {
            return Some(HighPreparationDeferReason::MissingSidxMap);
        }
        let Some(required_bitrate) = prepared.resource.request.bandwidth else {
            return Some(HighPreparationDeferReason::MissingOriginSupply);
        };
        let Some(supply) = state
            .cdn_history
            .recent_origin_supply(&prepared.resource.request)
        else {
            return Some(HighPreparationDeferReason::MissingOriginSupply);
        };
        if !optional_preparation_supply_sufficient(
            supply.throughput_bps,
            required_bitrate,
            snapshot.sample.playback_rate,
        ) {
            return Some(HighPreparationDeferReason::MissingOriginSupply);
        }
    }
    None
}

#[cfg(test)]
mod integration_tests {
    use std::{
        collections::HashMap,
        ops::Range,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::Duration,
    };

    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{HeaderMap, StatusCode, header},
        response::Response,
        routing::get,
    };
    use tempfile::TempDir;
    use tokio::{
        net::TcpListener,
        sync::{Notify, watch},
        task::JoinHandle,
    };
    use tonic::Request;

    use super::*;
    use crate::{
        bbdown_adapter::{
            BilibiliMediaRequest, BilibiliMediaRequestKind, BilibiliPlaybackAbrGroup,
            BilibiliPlaybackAbrGroupKind, BilibiliPlaybackAbrLevel, BilibiliPlaybackAbrMetadata,
            BilibiliPlaybackVariantKind,
        },
        bilibili_playback::{BilibiliContentIdentity, BilibiliContentKind},
        config::CacheServerOptions,
        generated::tvos_net_player::v1::{
            BilibiliPlaybackSession, BilibiliPlaybackVariant, BilibiliTaskResultItem,
            CloseHlsPlaybackContextRequest, HlsPlaybackTimingSample, OpenHlsPlaybackContextRequest,
            PlaybackProtocol, PlaybackSource, TaskState, UpdateHlsPlaybackContextRequest,
            cache_service_server::CacheService,
        },
        grpc_services::CacheGrpcService,
        hls::HlsPlaybackSession,
        playback_policy::PlaybackPolicy,
    };

    const SESSION_ID: &str = "high-preparation-fixture";
    const READER_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const VIDEO_BITRATE_BPS: u64 = 100_000;
    const AUDIO_BITRATE_BPS: u64 = 64_000;
    const FRAGMENT_BYTES: usize = 1_100_000;

    #[derive(Clone, Debug)]
    struct OriginRange {
        resource: String,
        range: Range<usize>,
        body_bytes: usize,
    }

    struct OriginGate {
        enabled: AtomicBool,
        minimum_start: AtomicU64,
        claimed: AtomicBool,
        started: Notify,
        release: watch::Sender<bool>,
    }

    struct OriginState {
        resources: HashMap<String, Vec<u8>>,
        requests: Mutex<Vec<OriginRange>>,
        gate: Option<Arc<OriginGate>>,
    }

    impl OriginState {
        fn ranges(&self) -> Vec<OriginRange> {
            self.requests
                .lock()
                .expect("origin request list should remain available")
                .clone()
        }
    }

    async fn video_origin(
        State(state): State<Arc<OriginState>>,
        headers: HeaderMap,
    ) -> Response<Body> {
        origin_response(state, "video", headers).await
    }

    async fn audio_origin(
        State(state): State<Arc<OriginState>>,
        headers: HeaderMap,
    ) -> Response<Body> {
        origin_response(state, "audio", headers).await
    }

    async fn origin_response(
        state: Arc<OriginState>,
        resource: &str,
        headers: HeaderMap,
    ) -> Response<Body> {
        let Some(data) = state.resources.get(resource) else {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .expect("not-found response should build");
        };
        let range = match headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok())
        {
            Some(value) => match parse_origin_range(value, data.len()) {
                Some(range) => range,
                None => {
                    return Response::builder()
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header(header::CONTENT_RANGE, format!("bytes */{}", data.len()))
                        .body(Body::empty())
                        .expect("range error response should build");
                }
            },
            None => 0..data.len(),
        };

        if let Some(gate) = &state.gate
            && gate.enabled.load(Ordering::Acquire)
            && resource == "video"
            && range.start as u64 >= gate.minimum_start.load(Ordering::Acquire)
            && !gate.claimed.swap(true, Ordering::AcqRel)
        {
            gate.started.notify_one();
            let mut released = gate.release.subscribe();
            if !*released.borrow() {
                let _ = released.wait_for(|value| *value).await;
            }
        }

        let body = data[range.clone()].to_vec();
        state
            .requests
            .lock()
            .expect("origin request list should remain available")
            .push(OriginRange {
                resource: resource.to_owned(),
                range: range.clone(),
                body_bytes: body.len(),
            });
        let status = if headers.contains_key(header::RANGE) {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        };
        let mut response = Response::builder()
            .status(status)
            .header(header::ETAG, "\"fixture-v1\"")
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, body.len().to_string());
        if status == StatusCode::PARTIAL_CONTENT {
            response = response.header(
                header::CONTENT_RANGE,
                format!("bytes {}-{}/{}", range.start, range.end - 1, data.len()),
            );
        }
        response
            .body(Body::from(body))
            .expect("origin range response should build")
    }

    fn parse_origin_range(value: &str, total: usize) -> Option<Range<usize>> {
        let value = value.strip_prefix("bytes=")?;
        let (start, end) = value.split_once('-')?;
        let start = start.parse::<usize>().ok()?;
        let end = end.parse::<usize>().ok()?;
        let end = end.checked_add(1)?;
        (start < end && end <= total).then_some(start..end)
    }

    struct Harness {
        _temp: TempDir,
        state: AppState,
        session: HlsPlaybackSession,
        generation: u64,
        origin: Arc<OriginState>,
        media_url: String,
        client: reqwest::Client,
        gate: Option<Arc<OriginGate>>,
        tasks: Vec<JoinHandle<()>>,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            for task in &self.tasks {
                task.abort();
            }
        }
    }

    impl Harness {
        async fn new(with_gate: bool) -> Self {
            let temp = tempfile::tempdir().expect("fixture cache directory should be created");
            let root = temp
                .path()
                .canonicalize()
                .expect("fixture cache root should canonicalize");
            let state = AppState::new(CacheServerOptions {
                root_path: root.clone(),
                task_state_path: root.join(".state").join("tasks.json"),
                bilibili_worker_enabled: false,
                ..CacheServerOptions::default()
            });
            let video = indexed_mp4(&[12_000, 18_000, 10_000, 10_000], FRAGMENT_BYTES, 0x31);
            let audio = indexed_mp4(&[10_000, 15_000, 15_000, 10_000], FRAGMENT_BYTES / 2, 0x72);
            let (release, _) = watch::channel(false);
            let gate = with_gate.then(|| {
                Arc::new(OriginGate {
                    enabled: AtomicBool::new(false),
                    minimum_start: AtomicU64::new(u64::MAX),
                    claimed: AtomicBool::new(false),
                    started: Notify::new(),
                    release,
                })
            });
            let mut resources = HashMap::new();
            resources.insert("video".to_owned(), video.clone());
            resources.insert("audio".to_owned(), audio.clone());
            let origin = Arc::new(OriginState {
                resources,
                requests: Mutex::new(Vec::new()),
                gate: gate.clone(),
            });

            let origin_listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("fixture origin should bind");
            let origin_address = origin_listener.local_addr().unwrap();
            let origin_router = Router::new()
                .route("/video.m4s", get(video_origin))
                .route("/audio.m4s", get(audio_origin))
                .with_state(Arc::clone(&origin));
            let origin_task = tokio::spawn(async move {
                axum::serve(origin_listener, origin_router)
                    .await
                    .expect("fixture origin should serve");
            });
            let origin_url = format!("http://{origin_address}");
            let session = fixture_session(&origin_url, video.len() as u64, audio.len() as u64);
            let generation = state.register_hls_playback_session(session.clone());
            authorize_fixture_session(&state, &session);

            let media_listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("fixture media router should bind");
            let media_address = media_listener.local_addr().unwrap();
            let media_router = crate::media_router(state.clone());
            let media_task = tokio::spawn(async move {
                axum::serve(media_listener, media_router)
                    .await
                    .expect("production media router should serve");
            });
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(8))
                .build()
                .expect("bounded loopback client should build");

            Self {
                _temp: temp,
                state,
                session,
                generation,
                origin,
                media_url: format!("http://{media_address}"),
                client,
                gate,
                tasks: vec![origin_task, media_task],
            }
        }

        fn resource_pairs(&self) -> Vec<(String, HlsMediaResource)> {
            vec![
                ("video".to_owned(), self.session.variant.video.clone()),
                (
                    "audio".to_owned(),
                    self.session
                        .variant
                        .audio
                        .as_ref()
                        .expect("fixture has required audio")
                        .clone(),
                ),
            ]
        }

        async fn load_maps(&self) {
            for playlist in ["video.m3u8", "audio.m3u8"] {
                let response = self
                    .client
                    .get(format!(
                        "{}/hls/{SESSION_ID}/segments/{playlist}?playback_reader={READER_ID}",
                        self.media_url
                    ))
                    .send()
                    .await
                    .expect("production media playlist request should complete");
                let status = response.status();
                let text = response
                    .text()
                    .await
                    .expect("production playlist should be readable");
                assert_eq!(
                    StatusCode::OK,
                    status,
                    "playlist body={text:?}; fixture origin ranges={:?}",
                    self.origin.ranges()
                );
                assert!(text.contains("#EXT-X-BYTERANGE:"));
            }
            for (_, resource) in self.resource_pairs() {
                assert!(
                    self.state
                        .hls_cache
                        .playback_sidx_map(SESSION_ID, self.generation, &resource)
                        .is_some(),
                    "the production playlist path should register a verified SIDX map"
                );
            }
        }

        async fn request_uncached_playback_ranges(&self) -> Vec<OriginRange> {
            let mut playback_origin_ranges = Vec::new();
            for (path, resource) in self.resource_pairs() {
                let map = self
                    .state
                    .hls_cache
                    .playback_sidx_map(SESSION_ID, self.generation, &resource)
                    .expect("playlist should register exact SIDX map");
                let playback_start_seconds = match path.as_str() {
                    "video" => 40.0,
                    "audio" => 40.0,
                    _ => unreachable!("fixture resource kind is fixed"),
                };
                let playback_range = map
                    .preparation_range_for_window(
                        playback_start_seconds,
                        playback_start_seconds + 0.5,
                    )
                    .expect("late playback fragment should overlap its timing window");
                assert!(
                    playback_range.bytes.start >= 1024 * 1024,
                    "requested playback bytes must begin beyond the media startup probe"
                );
                assert!(
                    playback_range.start_seconds >= 40.0,
                    "playback fragment must not overlap the optional preparation window"
                );
                let origin_cursor = self.origin.ranges().len();
                let response = self
                    .client
                    .get(format!(
                        "{}/hls/{SESSION_ID}/segments/{}?playback_reader={READER_ID}",
                        self.media_url, resource.id
                    ))
                    .header(
                        header::RANGE,
                        format!(
                            "bytes={}-{}",
                            playback_range.bytes.start,
                            playback_range.bytes.end - 1
                        ),
                    )
                    .send()
                    .await
                    .expect("real late-fragment playback transfer should complete");
                assert_eq!(StatusCode::PARTIAL_CONTENT, response.status());
                let body = response
                    .bytes()
                    .await
                    .expect("real late-fragment playback body should complete");
                assert_eq!(
                    playback_range.bytes.end - playback_range.bytes.start,
                    body.len() as u64
                );
                let new_origin_ranges = self.origin.ranges();
                let new_origin_ranges = &new_origin_ranges[origin_cursor..];
                assert!(
                    new_origin_ranges.iter().any(|entry| {
                        entry.resource == path
                            && entry.range.start < playback_range.bytes.end as usize
                            && entry.range.end > playback_range.bytes.start as usize
                            && entry.body_bytes > 0
                    }),
                    "origin traffic after this GET must cover its uncached {path} playback range"
                );
                playback_origin_ranges.extend(new_origin_ranges.iter().cloned());
            }
            for (_, resource) in self.resource_pairs() {
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        if self
                            .state
                            .cdn_history
                            .recent_origin_supply(&resource.request)
                            .is_some()
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("completed playback transfer should publish measured source supply");
                let supply = self
                    .state
                    .cdn_history
                    .recent_origin_supply(&resource.request)
                    .expect("completed playback transfer should leave fresh source supply");
                assert!(optional_preparation_supply_sufficient(
                    supply.throughput_bps,
                    resource.request.bandwidth.unwrap(),
                    1.5,
                ));
            }
            playback_origin_ranges
        }

        async fn open_context(
            &self,
        ) -> crate::generated::tvos_net_player::v1::HlsPlaybackContextTicket {
            // Generated trait calls do not prove gRPC transport or AppCore reporting acceptance.
            let response = CacheGrpcService::new(self.state.clone())
                .open_hls_playback_context(Request::new(OpenHlsPlaybackContextRequest {
                    session_id: SESSION_ID.to_owned(),
                    expected_session_generation: self.generation,
                    http_reader_id: READER_ID.to_owned(),
                }))
                .await
                .expect("in-process generated Open should accept the fixture session");
            response.into_inner()
        }

        async fn update_context(
            &self,
            ticket: &crate::generated::tvos_net_player::v1::HlsPlaybackContextTicket,
            playhead_seconds: f64,
            buffered_end_seconds: f64,
            playback_rate: f64,
        ) -> crate::generated::tvos_net_player::v1::HlsPlaybackContextTicket {
            CacheGrpcService::new(self.state.clone())
                .update_hls_playback_context(Request::new(UpdateHlsPlaybackContextRequest {
                    context_id: ticket.context_id.clone(),
                    challenge: ticket.challenge.clone(),
                    sequence: ticket.next_sequence,
                    sample: Some(HlsPlaybackTimingSample {
                        playhead_seconds,
                        continuous_buffered_end_seconds: buffered_end_seconds,
                        playback_rate,
                    }),
                }))
                .await
                .expect("in-process generated Update should accept the fixture sample")
                .into_inner()
        }

        async fn close_context(&self, context_id: &str) {
            CacheGrpcService::new(self.state.clone())
                .close_hls_playback_context(Request::new(CloseHlsPlaybackContextRequest {
                    context_id: context_id.to_owned(),
                }))
                .await
                .expect("in-process generated Close should retire the fixture context");
        }
    }

    async fn wait_for_terminal_plan(
        state: &AppState,
        context_id: &str,
        timeout: Duration,
    ) -> HighPreparationPlanObservation {
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(plan) = state
                    .hls_high_preparation
                    .observation_for_tests()
                    .plan_history
                    .into_iter()
                    .find(|plan| {
                        plan.context_id.as_deref() == Some(context_id)
                            && plan.requested_resource_ranges > 0
                            && plan.outcome != HighPreparationPlanOutcome::Admitted
                    })
                {
                    break plan;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("admitted plan should reach a terminal observation")
    }

    async fn wait_for_idle_coordinator(state: &AppState, timeout: Duration) {
        tokio::time::timeout(timeout, async {
            loop {
                let coordinator = &state.hls_high_preparation;
                let idle = {
                    let state = coordinator.inner.state.lock().unwrap();
                    state.active_plans.is_empty()
                        && state
                            .readers
                            .get(READER_ID)
                            .is_none_or(|reader| reader.active_plan_id.is_none())
                        && coordinator.inner.global_jobs.available_permits()
                            == HIGH_PREPARATION_GLOBAL_JOB_LIMIT
                };
                if idle {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("retired job should release reader and global slots");
    }

    #[tokio::test]
    async fn disabled_coordinator_preserves_context_reporting_and_reader_requests() {
        let harness = Harness::new(false).await;
        harness.load_maps().await;
        harness
            .state
            .hls_high_preparation
            .set_enabled_for_tests(false);
        let opened = harness.open_context().await;
        let updated = harness.update_context(&opened, 0.0, 20.0, 1.0).await;

        let snapshot = harness
            .state
            .current_hls_playback_context_for_reader(READER_ID)
            .expect("disabled optional work must not disable context reporting");
        assert_eq!(updated.context_id, snapshot.context_id);
        assert_eq!(0.0, snapshot.sample.playhead_seconds);
        assert_eq!(20.0, snapshot.sample.continuous_buffered_end_seconds);

        let observation = harness.state.hls_high_preparation.observation_for_tests();
        assert_eq!(observation.admitted_plans, 0);
        assert_eq!(observation.deferred_plans, 1);
        assert_eq!(
            Some(HighPreparationDeferReason::Disabled),
            observation
                .plan_history
                .last()
                .and_then(|plan| plan.defer_reason),
            "OFF should defer only the optional coordinator"
        );

        let video = harness.session.variant.video.clone();
        let playback_range = harness
            .state
            .hls_cache
            .playback_sidx_map(SESSION_ID, harness.generation, &video)
            .unwrap()
            .preparation_range_for_window(0.0, 0.5)
            .unwrap()
            .bytes;
        let response = harness
            .client
            .get(format!(
                "{}/hls/{SESSION_ID}/segments/{}?playback_reader={READER_ID}",
                harness.media_url, video.id
            ))
            .header(
                header::RANGE,
                format!("bytes={}-{}", playback_range.start, playback_range.end - 1),
            )
            .send()
            .await
            .expect("reader-bound playback request should remain schedulable");
        assert_eq!(StatusCode::PARTIAL_CONTENT, response.status());
        let body = response
            .bytes()
            .await
            .expect("reader-bound playback body should remain readable");
        assert_eq!(playback_range.end - playback_range.start, body.len() as u64);
        assert_eq!(
            0,
            harness
                .state
                .hls_high_preparation
                .observation_for_tests()
                .admitted_plans
        );
    }

    fn fixture_session(origin_url: &str, video_size: u64, audio_size: u64) -> HlsPlaybackSession {
        let primary = adapter_variant(
            "primary",
            &format!("{origin_url}/video.m4s"),
            &format!("{origin_url}/audio.m4s"),
            video_size,
            audio_size,
            0,
        );
        let alternate = adapter_variant(
            "compatible-alternate",
            &format!("{origin_url}/alternate-video.m4s"),
            &format!("{origin_url}/alternate-audio.m4s"),
            video_size,
            audio_size,
            1,
        );
        let abr = BilibiliPlaybackAbrMetadata {
            groups: vec![BilibiliPlaybackAbrGroup {
                id: "dash-video".to_owned(),
                kind: BilibiliPlaybackAbrGroupKind::DashVideo,
                variant_ids: vec![primary.id.clone(), alternate.id.clone()],
                level_count: 2,
                min_bandwidth: Some(VIDEO_BITRATE_BPS + AUDIO_BITRATE_BPS),
                max_bandwidth: Some(VIDEO_BITRATE_BPS * 2 + AUDIO_BITRATE_BPS),
            }],
        };
        let variants = [primary.clone(), alternate];
        let mut session = HlsPlaybackSession::from_playback_entry_with_policy(
            SESSION_ID,
            "High preparation fixture",
            &primary,
            &abr,
            &variants,
            PlaybackPolicy::default(),
        )
        .expect("switchable H.264/AAC fixture should build a normal HLS session");
        session.accepted_identity = Some(BilibiliContentIdentity {
            kind: BilibiliContentKind::VideoPage,
            aid: Some(1),
            bvid: Some("BV1xx411c7mD".to_owned()),
            cid: Some(2),
            epid: None,
        });
        session
    }

    fn adapter_variant(
        id: &str,
        video_url: &str,
        audio_url: &str,
        video_size: u64,
        audio_size: u64,
        level_index: u32,
    ) -> crate::bbdown_adapter::BilibiliPlaybackVariant {
        use crate::bbdown_adapter::BilibiliPlaybackVariant;

        let video = media_request(
            video_url,
            BilibiliMediaRequestKind::Video,
            video_size,
            VIDEO_BITRATE_BPS * (u64::from(level_index) + 1),
            &format!("{id}-video-source"),
            "video/mp4",
            "avc1.640028",
        );
        let audio = media_request(
            audio_url,
            BilibiliMediaRequestKind::Audio,
            audio_size,
            AUDIO_BITRATE_BPS,
            &format!("{id}-audio-source"),
            "audio/mp4",
            "mp4a.40.2",
        );
        BilibiliPlaybackVariant {
            id: id.to_owned(),
            kind: BilibiliPlaybackVariantKind::Dash,
            content_id: "cid-1".to_owned(),
            bandwidth: Some(VIDEO_BITRATE_BPS * (u64::from(level_index) + 1) + AUDIO_BITRATE_BPS),
            codecs: vec!["avc1.640028".to_owned(), "mp4a.40.2".to_owned()],
            mime_types: vec!["video/mp4".to_owned(), "audio/mp4".to_owned()],
            width: Some(1280),
            height: Some(720),
            frame_rate: Some("30".to_owned()),
            duration_seconds: Some(50),
            abr: Some(BilibiliPlaybackAbrLevel {
                group_id: "dash-video".to_owned(),
                level_index,
                level_count: 2,
                switchable: true,
            }),
            video: Some(video),
            audio: Some(audio),
            flv_segments: Vec::new(),
        }
    }

    fn media_request(
        url: &str,
        kind: BilibiliMediaRequestKind,
        size: u64,
        bandwidth: u64,
        source_hash: &str,
        mime_type: &str,
        codecs: &str,
    ) -> BilibiliMediaRequest {
        let is_video = kind == BilibiliMediaRequestKind::Video;
        BilibiliMediaRequest {
            kind,
            stream_id: None,
            url: url.to_owned(),
            backup_urls: Vec::new(),
            headers: vec![crate::bbdown_adapter::BilibiliHttpHeader {
                name: "referer".to_owned(),
                value: "https://www.bilibili.com".to_owned(),
            }],
            mime_type: Some(mime_type.to_owned()),
            codecs: Some(codecs.to_owned()),
            bandwidth: Some(bandwidth),
            width: is_video.then_some(1280),
            height: is_video.then_some(720),
            frame_rate: is_video.then(|| "30".to_owned()),
            size: Some(size),
            duration_seconds: Some(50),
            cache_key: crate::bbdown_adapter::BilibiliMediaCacheKey {
                content_id: "cid-1".to_owned(),
                media_kind: kind,
                stream_id: None,
                codecs: Some(codecs.to_owned()),
                source_hash: source_hash.to_owned(),
            },
        }
    }

    fn authorize_fixture_session(state: &AppState, session: &HlsPlaybackSession) {
        let playback_source = PlaybackSource {
            item_id: SESSION_ID.to_owned(),
            variant_id: session.variant.id.clone(),
            protocol: PlaybackProtocol::Hls.into(),
            uri: format!("http://media.example.test/hls/{SESSION_ID}/master.m3u8"),
            expires_at: None,
        };
        let playback_session = BilibiliPlaybackSession {
            id: SESSION_ID.to_owned(),
            title: session.title.clone(),
            content_id: "cid-1".to_owned(),
            selected_variant_id: session.variant.id.clone(),
            selected_variant: Some(BilibiliPlaybackVariant {
                id: session.variant.id.clone(),
                label: "1280x720".to_owned(),
                source_kind: "dash".to_owned(),
                container: "mp4".to_owned(),
                video_codec: "avc1.640028".to_owned(),
                audio_codec: "mp4a.40.2".to_owned(),
                width: 1280,
                height: 720,
                bitrate: (VIDEO_BITRATE_BPS + AUDIO_BITRATE_BPS) as i64,
                size_bytes: 0,
            }),
            variants: Vec::new(),
            transcoding_plan: None,
            effective_policy: Some(session.effective_policy.to_proto()),
        };
        let task = state
            .tasks
            .create_bilibili_playback_task("BV1high-preparation-fixture", None, None)
            .expect("fixture playback task should be created");
        state
            .tasks
            .complete_playback_results_playable(
                &task.task.id,
                "Loopback playback fixture".to_owned(),
                "Fixture session is playable.".to_owned(),
                playback_source.clone(),
                playback_session.clone(),
                vec![BilibiliTaskResultItem {
                    id: SESSION_ID.to_owned(),
                    selection_id: "page:1".to_owned(),
                    title: session.title.clone(),
                    subtitle: String::new(),
                    source_kind: "video_page".to_owned(),
                    content_id: "cid-1".to_owned(),
                    index: 1,
                    state: TaskState::Playable.into(),
                    message: "Playable".to_owned(),
                    library_item_id: String::new(),
                    playback_source: Some(playback_source),
                    playback_session: Some(playback_session),
                    identity: None,
                    hls_cache_fill_status: None,
                }],
            )
            .expect("fixture playback result should authorize the HLS session");
    }

    fn indexed_mp4(durations: &[u32], fragment_bytes: usize, pattern: u8) -> Vec<u8> {
        let media = durations
            .iter()
            .enumerate()
            .map(|(index, _)| {
                mp4_box(
                    *b"mdat",
                    &vec![pattern.wrapping_add(index as u8); fragment_bytes],
                )
            })
            .collect::<Vec<_>>();
        let mut payload = Vec::new();
        payload.extend([0_u8; 4]);
        payload.extend(1_u32.to_be_bytes());
        payload.extend(1_000_u32.to_be_bytes());
        payload.extend(0_u32.to_be_bytes());
        payload.extend(0_u32.to_be_bytes());
        payload.extend([0_u8; 2]);
        payload.extend(u16::try_from(durations.len()).unwrap().to_be_bytes());
        for (media, duration) in media.iter().zip(durations) {
            payload.extend(u32::try_from(media.len()).unwrap().to_be_bytes());
            payload.extend(duration.to_be_bytes());
            payload.extend(0x9000_0000_u32.to_be_bytes());
        }
        let mut bytes = Vec::new();
        bytes.extend(mp4_box(*b"ftyp", b"isom"));
        bytes.extend(mp4_box(*b"moov", b"metadata"));
        bytes.extend(mp4_box(*b"sidx", &payload));
        for fragment in media {
            bytes.extend(fragment);
        }
        bytes.extend(terminal_mfra());
        bytes
    }

    fn mp4_box(kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + payload.len()).unwrap();
        let mut bytes = Vec::with_capacity(size as usize);
        bytes.extend(size.to_be_bytes());
        bytes.extend(kind);
        bytes.extend(payload);
        bytes
    }

    fn terminal_mfra() -> Vec<u8> {
        let mut tfra_payload = vec![0_u8; 4];
        tfra_payload.extend(1_u32.to_be_bytes());
        tfra_payload.extend(0_u32.to_be_bytes());
        tfra_payload.extend(0_u32.to_be_bytes());
        let tfra = mp4_box(*b"tfra", &tfra_payload);
        let mfro_size = 16_u32;
        let mfra_size = 8 + u32::try_from(tfra.len()).unwrap() + mfro_size;
        let mut mfro_payload = vec![0_u8; 4];
        mfro_payload.extend(mfra_size.to_be_bytes());
        let mut mfra_payload = tfra;
        mfra_payload.extend(mp4_box(*b"mfro", &mfro_payload));
        mp4_box(*b"mfra", &mfra_payload)
    }

    #[tokio::test]
    async fn accepted_context_after_real_playback_supply_prepares_same_av_window() {
        let harness = Harness::new(false).await;
        harness.load_maps().await;
        harness.state.maybe_start_hls_high_preparation(READER_ID);
        assert_eq!(
            0,
            harness
                .state
                .hls_high_preparation
                .observation_for_tests()
                .admitted_plans,
            "no context must never start optional work"
        );

        let opened = harness.open_context().await;
        let _first_update = harness.update_context(&opened, 0.0, 20.0, 1.0).await;
        let initial = harness.state.hls_high_preparation.observation_for_tests();
        let initial_defer = initial
            .plan_history
            .iter()
            .rev()
            .find(|plan| plan.context_id.as_deref() == Some(opened.context_id.as_str()))
            .expect("accepted context should produce a bounded defer observation");
        assert_eq!(
            initial_defer.defer_reason,
            Some(HighPreparationDeferReason::MissingOriginSupply)
        );
        assert_eq!(initial.admitted_plans, 0);
        for (_, resource) in harness.resource_pairs() {
            assert!(
                harness
                    .state
                    .cdn_history
                    .recent_origin_supply(&resource.request)
                    .is_none(),
                "playlist/index probes are not measured playback supply"
            );
        }

        let playback_ranges = harness.request_uncached_playback_ranges().await;
        assert!(
            playback_ranges
                .iter()
                .any(|range| range.resource == "video")
        );
        assert!(
            playback_ranges
                .iter()
                .any(|range| range.resource == "audio")
        );
        harness.close_context(&opened.context_id).await;
        let measured_context = harness.open_context().await;
        let origin_before_preparation = harness.origin.ranges().len();
        let measured_update = harness
            .update_context(&measured_context, 0.0, 20.0, 1.0)
            .await;
        let plan = wait_for_terminal_plan(
            &harness.state,
            &measured_update.context_id,
            Duration::from_secs(6),
        )
        .await;

        assert_eq!(plan.outcome, HighPreparationPlanOutcome::Completed);
        assert_eq!(plan.initial_window_readable, Some(false));
        assert!(plan.window_readable);
        assert_eq!(plan.window_start_seconds, Some(20.0));
        assert_eq!(plan.window_end_seconds, Some(40.0));
        assert_eq!(plan.requested_resource_ranges, 2);
        assert_eq!(plan.readable_resource_ranges, 2);
        assert_eq!(plan.deferred_resource_ranges, 0);
        assert_eq!(plan.selected_spans.len(), 2);
        let video_span = plan
            .selected_spans
            .iter()
            .find(|span| span.kind == HighPreparationResourceKind::Video)
            .expect("video span should be observed");
        let audio_span = plan
            .selected_spans
            .iter()
            .find(|span| span.kind == HighPreparationResourceKind::Audio)
            .expect("required audio span should be observed");
        assert_eq!(
            (video_span.start_seconds, video_span.end_seconds),
            (12.0, 40.0)
        );
        assert_eq!(
            (audio_span.start_seconds, audio_span.end_seconds),
            (10.0, 40.0)
        );
        assert!(video_span.start_seconds <= plan.window_start_seconds.unwrap());
        assert!(video_span.end_seconds >= plan.window_end_seconds.unwrap());
        assert!(audio_span.start_seconds <= plan.window_start_seconds.unwrap());
        assert!(audio_span.end_seconds >= plan.window_end_seconds.unwrap());
        assert_ne!(
            (video_span.start_seconds, video_span.end_seconds),
            (audio_span.start_seconds, audio_span.end_seconds),
            "different whole-fragment boundaries still cover the same logical window"
        );
        assert!(video_span.readable && audio_span.readable);

        let video_map = harness
            .state
            .hls_cache
            .playback_sidx_map(
                SESSION_ID,
                harness.generation,
                &harness.session.variant.video,
            )
            .unwrap();
        let audio_resource = harness.session.variant.audio.as_ref().unwrap();
        let audio_map = harness
            .state
            .hls_cache
            .playback_sidx_map(SESSION_ID, harness.generation, audio_resource)
            .unwrap();
        let selected_video = video_map.preparation_range_for_window(20.0, 40.0).unwrap();
        let selected_audio = audio_map.preparation_range_for_window(20.0, 40.0).unwrap();
        let inspection = harness
            .state
            .hls_cache
            .inspect_cached_playback_window(
                SESSION_ID,
                &harness.session.variant,
                PlaybackWindow::new(20.0, 20.0).unwrap(),
            )
            .await;
        assert_eq!(inspection.state, CacheWindowState::Readable);

        let new_origin_ranges = harness.origin.ranges();
        let new_origin_ranges = &new_origin_ranges[origin_before_preparation..];
        for (resource, selected) in [
            ("video", selected_video.bytes),
            ("audio", selected_audio.bytes),
        ] {
            assert!(
                new_origin_ranges.iter().any(|transfer| {
                    transfer.resource == resource
                        && transfer.range.start < selected.end as usize
                        && transfer.range.end > selected.start as usize
                        && transfer.body_bytes > 0
                }),
                "isolated origin should serve new bytes covering {resource} selection"
            );
        }
        let aggregate = harness.state.hls_high_preparation.observation_for_tests();
        assert_eq!(aggregate.admitted_plans, 1);
        assert_eq!(aggregate.completed_plans, 1);
        assert_eq!(aggregate.requested_resource_ranges, 2);
        assert_eq!(aggregate.readable_resource_ranges, 2);
        assert_eq!(
            aggregate.deferred_resource_ranges, 0,
            "readability is not an origin-byte or own-publication claim"
        );
    }

    #[tokio::test]
    async fn loaded_end_progress_preserves_active_plan_and_seek_retires_it() {
        let harness = Harness::new(true).await;
        harness.load_maps().await;
        harness.request_uncached_playback_ranges().await;
        let video = harness.session.variant.video.clone();
        let high_range = harness
            .state
            .hls_cache
            .playback_sidx_map(SESSION_ID, harness.generation, &video)
            .unwrap()
            .preparation_range_for_window(20.0, 40.0)
            .unwrap()
            .bytes;
        let gate = harness.gate.as_ref().unwrap();
        gate.minimum_start
            .store(high_range.start, Ordering::Release);
        gate.enabled.store(true, Ordering::Release);

        let opened = harness.open_context().await;
        let first = harness.update_context(&opened, 0.0, 20.0, 1.0).await;
        tokio::time::timeout(Duration::from_secs(4), gate.started.notified())
            .await
            .expect("high preparation should reach the gated real origin range");
        let snapshot = harness
            .state
            .current_hls_playback_context_for_reader(READER_ID)
            .expect("accepted context should be current");
        let (plan_id, epoch) = {
            let state = harness
                .state
                .hls_high_preparation
                .inner
                .state
                .lock()
                .unwrap();
            let reader = state.readers.get(READER_ID).unwrap();
            (reader.active_plan_id.unwrap(), reader.epoch)
        };
        let mut observation = observation_for_context(READER_ID, &snapshot);
        let draft = build_high_preparation_draft(
            &harness.state,
            READER_ID,
            &snapshot,
            epoch,
            &mut observation,
        )
        .expect("fresh in-flight fixture plan should remain eligible");
        let target_variant_id = draft.target.id.clone();
        let plan = HighPreparationPlan {
            plan_id,
            epoch: draft.epoch,
            context_id: draft.context_id,
            reader_id: draft.reader_id,
            session_id: draft.session_id,
            session_generation: draft.session_generation,
            accepted_identity: draft.accepted_identity,
            target: draft.target,
            target_variant_id,
            window: draft.window,
            resources: draft.resources,
        };

        let progressed = harness.update_context(&first, 1.0, 30.0, 1.0).await;
        assert_eq!(
            Some(30.0),
            safe_future_start(PlaybackTimingSample {
                playhead_seconds: 1.0,
                continuous_buffered_end_seconds: 30.0,
                playback_rate: 1.0,
            })
        );
        assert!(
            plan.window.start_seconds < 30.0,
            "the current buffer end has advanced beyond the admitted plan start"
        );
        assert_eq!(
            plan_control_reason(&harness.state, &harness.state.hls_high_preparation, &plan),
            None,
            "healthy loaded-end advancement alone must not retire a useful fill"
        );
        assert_eq!(
            1,
            harness
                .state
                .hls_high_preparation
                .observation_for_tests()
                .admitted_plans,
            "an update during single-flight work must not admit a duplicate job"
        );

        let unsafe_update = harness.update_context(&progressed, 2.0, 21.0, 1.0).await;
        assert_eq!(
            plan_control_reason(&harness.state, &harness.state.hls_high_preparation, &plan),
            Some(HighPreparationDeferReason::InsufficientBuffer),
            "a fresh sample without the full wall-time protection must retire the plan"
        );
        let restored = harness.update_context(&unsafe_update, 2.0, 30.0, 1.0).await;
        assert_eq!(
            plan_control_reason(&harness.state, &harness.state.hls_high_preparation, &plan),
            None,
            "restored safe continuous timing should keep the same future window useful"
        );
        let _seek = harness.update_context(&restored, 10.0, 30.0, 1.0).await;
        assert_eq!(
            plan_control_reason(&harness.state, &harness.state.hls_high_preparation, &plan),
            Some(HighPreparationDeferReason::Seek),
            "a playhead jump beyond tolerance must retire the prior epoch"
        );
        gate.release
            .send(true)
            .expect("test gate should release the in-flight origin body");
        let terminal =
            wait_for_terminal_plan(&harness.state, &opened.context_id, Duration::from_secs(5))
                .await;
        assert_eq!(terminal.outcome, HighPreparationPlanOutcome::Cancelled);
        assert_eq!(
            terminal.defer_reason,
            Some(HighPreparationDeferReason::Seek)
        );
        assert!(terminal.requested_resource_ranges > 0);
        wait_for_idle_coordinator(&harness.state, Duration::from_secs(2)).await;
        let state = harness
            .state
            .hls_high_preparation
            .inner
            .state
            .lock()
            .unwrap();
        assert!(state.active_plans.is_empty());
        assert!(
            state
                .readers
                .get(READER_ID)
                .unwrap()
                .active_plan_id
                .is_none()
        );
    }

    #[tokio::test]
    async fn replacing_session_generation_retires_active_preparation() {
        let harness = Harness::new(true).await;
        harness.load_maps().await;
        harness.request_uncached_playback_ranges().await;
        let video = harness.session.variant.video.clone();
        let high_range = harness
            .state
            .hls_cache
            .playback_sidx_map(SESSION_ID, harness.generation, &video)
            .unwrap()
            .preparation_range_for_window(20.0, 40.0)
            .unwrap()
            .bytes;
        let gate = harness.gate.as_ref().unwrap();
        gate.minimum_start
            .store(high_range.start, Ordering::Release);
        gate.enabled.store(true, Ordering::Release);

        let opened = harness.open_context().await;
        let _updated = harness.update_context(&opened, 0.0, 20.0, 1.0).await;
        tokio::time::timeout(Duration::from_secs(4), gate.started.notified())
            .await
            .expect("optional preparation should reach the gated origin range");

        let mut replacement = harness.session.clone();
        replacement.accepted_identity = Some(BilibiliContentIdentity {
            kind: BilibiliContentKind::VideoPage,
            aid: Some(3),
            bvid: Some("BV1yy411c7mD".to_owned()),
            cid: Some(4),
            epid: None,
        });
        let replacement_generation = harness.state.register_hls_playback_session(replacement);
        assert_ne!(harness.generation, replacement_generation);

        gate.release
            .send(true)
            .expect("test gate should release the in-flight origin body");
        let terminal =
            wait_for_terminal_plan(&harness.state, &opened.context_id, Duration::from_secs(5))
                .await;
        assert_eq!(terminal.outcome, HighPreparationPlanOutcome::Cancelled);
        assert_eq!(
            terminal.defer_reason,
            Some(HighPreparationDeferReason::OwnershipChanged)
        );
        wait_for_idle_coordinator(&harness.state, Duration::from_secs(2)).await;
    }

    #[tokio::test]
    async fn total_job_deadline_releases_plan_slot_and_range_flight_can_retry() {
        let harness = Harness::new(true).await;
        harness.load_maps().await;
        harness.request_uncached_playback_ranges().await;
        harness
            .state
            .hls_high_preparation
            .set_job_deadline_for_tests(Duration::from_millis(1_500));
        let video = harness.session.variant.video.clone();
        let high_range = harness
            .state
            .hls_cache
            .playback_sidx_map(SESSION_ID, harness.generation, &video)
            .unwrap()
            .preparation_range_for_window(20.0, 40.0)
            .unwrap()
            .bytes;
        let gate = harness.gate.as_ref().unwrap();
        gate.minimum_start
            .store(high_range.start, Ordering::Release);
        gate.enabled.store(true, Ordering::Release);

        let opened = harness.open_context().await;
        let _updated = harness.update_context(&opened, 0.0, 20.0, 1.0).await;
        tokio::time::timeout(Duration::from_secs(4), gate.started.notified())
            .await
            .expect("bounded preparation should reach the gated origin request");
        let terminal =
            wait_for_terminal_plan(&harness.state, &opened.context_id, Duration::from_secs(4))
                .await;
        assert_eq!(terminal.outcome, HighPreparationPlanOutcome::Failed);
        assert_eq!(
            terminal.defer_reason,
            Some(HighPreparationDeferReason::JobDeadlineExceeded)
        );
        wait_for_idle_coordinator(&harness.state, Duration::from_secs(2)).await;
        {
            let state = harness
                .state
                .hls_high_preparation
                .inner
                .state
                .lock()
                .unwrap();
            assert!(
                state.active_plans.is_empty(),
                "deadline must retire active metrics"
            );
            assert!(
                state
                    .readers
                    .get(READER_ID)
                    .unwrap()
                    .active_plan_id
                    .is_none()
            );
        }
        assert_eq!(
            HIGH_PREPARATION_GLOBAL_JOB_LIMIT,
            harness
                .state
                .hls_high_preparation
                .inner
                .global_jobs
                .available_permits(),
            "the total-job timeout must release its global permit"
        );
        assert_eq!(
            (0, 0),
            harness.state.hls_cache.range_activity_counts(),
            "the timed-out batch must release its range activity and reservations"
        );
        assert!(
            !harness
                .state
                .hls_cache
                .has_range_flight_overlapping_for_test(SESSION_ID, &video.id, &high_range,),
            "the timed-out range flight must be retired before retry"
        );

        gate.release.send_replace(true);
        let control = || HlsCacheFillControl::Continue;
        let retry_range = high_range.clone();
        let retry_requests = [PlaybackPreparationRequest {
            resource: &video,
            requested: retry_range.clone(),
        }];
        let retried = tokio::time::timeout(
            Duration::from_secs(4),
            harness.state.hls_cache.prepare_playback_batch(
                &harness.state.hls_upstream_client,
                PlaybackPreparationBatch {
                    session_id: SESSION_ID,
                    requests: &retry_requests,
                },
                &control,
            ),
        )
        .await
        .expect("a canceled single-flight range should not block a batch retry")
        .expect("retried batch should complete after its origin is released");
        assert_eq!(1, retried.len());
        assert_eq!(retry_range, retried[0].requested);
    }
}

#[cfg(test)]
#[path = "hls_high_preparation/planning_tests.rs"]
mod planning_tests;
