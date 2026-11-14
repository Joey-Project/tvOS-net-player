use super::*;
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use axum::{
    body::{Body, Bytes},
    extract::{Request as HttpRequest, State},
    http::{Method, StatusCode},
    middleware::{Next, from_fn_with_state},
    response::Response,
};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;
use tonic::Request as GrpcRequest;
use uuid::Uuid;

use crate::{
    generated::tvos_net_player::v1::{
        CloseHlsPlaybackContextRequest, HlsPlaybackContextTicket, HlsPlaybackTimingSample,
        OpenHlsPlaybackContextRequest, UpdateHlsPlaybackContextRequest,
        cache_service_server::CacheService,
    },
    grpc_services::CacheGrpcService,
    hls::HlsMediaResource,
    hls_high_preparation::{
        HighPreparationObservation, HighPreparationPlanObservation, HighPreparationPlanOutcome,
        HighPreparationResourceKind,
    },
    hls_playback_context::PlaybackTimingSample,
    playback_request_urgency::PlaybackSidxMap,
};

const ARM_REPORT_LIMIT: usize = 64 * 1024;
const CONTROL_MESSAGE_LIMIT: usize = 4 * 1024;
const CONTROL_SAMPLE_LIMIT: usize = 1_024;
const CAUSAL_EVENT_LIMIT: usize = 96;
const TRACKED_PLAN_LIMIT: usize = 16;
const HIGH_PREPARATION_CONTROL_VALIDITY_MILLIS: u64 = 5_000;
const HIGH_PREPARATION_CONTROL_INTERVAL: Duration = Duration::from_secs(2);
const HIGH_PREPARATION_PROTECTED_WALL_SECONDS: f64 = 20.0;
const HIGH_PREPARATION_WINDOW_SECONDS: f64 = 20.0;
const ARM_TRANSPORT_BOUNDARY: &str = "in_process_generated_cache_service_trait";
const OPT_IN_FIXTURE_GENERATION_TIMEOUT: Duration = Duration::from_secs(240);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArmMode {
    Off,
    On,
}

impl ArmMode {
    fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
        }
    }

    fn enabled(self) -> bool {
        self == Self::On
    }
}

#[derive(Serialize)]
struct ArmReport {
    schema_version: u32,
    arm: &'static str,
    status: &'static str,
    eligible: bool,
    transport_boundary: &'static str,
    fixture_generation_timeout_seconds: u64,
    fixture_is_fresh: bool,
    cache_is_fresh: bool,
    reader: ReaderReport,
    native: NativeReport,
    plans: Vec<PlanReport>,
    origin_range_transfers: Vec<RangeTransferReport>,
    player_range_transfers: Vec<RangeTransferReport>,
    plan_history_retention: PlanHistoryRetentionReport,
    transfer_event_retention: TransferEventRetentionReport,
    logical_range_counts: LogicalRangeCounts,
    attribution: AttributionReport,
    first_recovery_high_origin_request: Option<RecoveryRangeRequest>,
    first_recovery_high_completed_source_body: Option<FirstCompletedSourceBody>,
    first_recovery_high_decoded_monotonic_seconds: Option<f64>,
    recovery_preparation: Option<RecoveryPreparationReport>,
    inconclusive_reason: Option<String>,
}

#[derive(Serialize)]
struct ReaderReport {
    id: String,
    context_id: String,
    session_id: String,
    session_generation: u64,
}

#[derive(Serialize)]
struct NativeReport {
    mode: &'static str,
    requested_duration_seconds: u64,
    deadline_seconds: u64,
    phase_completed: bool,
    muted_verified: bool,
    system_audio_mute_verified: bool,
    automatic_waiting_enabled: bool,
    high_low_high_decoded: bool,
    decode_censored: bool,
    seek_count: u64,
    rate_change_count: u64,
    item_replacement_count: u64,
    buffer_cap_set: bool,
    challenge_sample_count: u64,
    challenge_samples_truncated: bool,
}

#[derive(Serialize)]
struct LogicalRangeCounts {
    requested_resource_ranges: u64,
    readable_resource_ranges: u64,
    deferred_resource_ranges: u64,
}

#[derive(Serialize)]
struct AttributionReport {
    exclusive_attribution: &'static str,
    causal_faster_selection_claim: bool,
}

#[derive(Serialize)]
struct PlanHistoryRetentionReport {
    observed_matching_candidates: u64,
    internally_retained_records: usize,
    internal_record_limit: usize,
    serialized_plan_records: usize,
    serialization_record_limit: usize,
    producer_history_limit: usize,
    truncated: bool,
}

#[derive(Serialize)]
struct TransferEventRetentionReport {
    origin_recovery_range_events_observed: u64,
    origin_witnesses_retained: usize,
    origin_ring_limit: usize,
    origin_ring_truncated: bool,
    player_recovery_range_events_observed: u64,
    player_witnesses_retained: usize,
    player_ring_limit: usize,
    player_ring_truncated: bool,
}

#[derive(Clone, Serialize)]
struct SelectedSpanReport {
    kind: &'static str,
    resource_id: String,
    variant_id: String,
    context_id: String,
    session_id: String,
    session_generation: u64,
    logical_window_start_seconds: f64,
    logical_window_end_seconds: f64,
    byte_range_start: u64,
    byte_range_end_exclusive: u64,
    before_read_state: &'static str,
    after_read_state: &'static str,
    newly_readable_bytes_lower_bound: u64,
    readable: bool,
}

#[derive(Clone, Serialize)]
struct PlanReport {
    plan_id: u64,
    context_id: String,
    reader_id: String,
    session_id: String,
    session_generation: u64,
    target_variant_id: Option<String>,
    window_start_seconds: f64,
    window_end_seconds: f64,
    initial_window_readable: Option<bool>,
    outcome: &'static str,
    window_readable: bool,
    admitted_sample_monotonic_seconds: f64,
    terminal_observed_monotonic_seconds: Option<f64>,
    selected_spans: Vec<SelectedSpanReport>,
}

#[derive(Clone, Serialize)]
struct RangeTransferReport {
    context_id: String,
    reader_id: String,
    session_id: String,
    session_generation: u64,
    variant_id: String,
    resource_id: String,
    logical_window_start_seconds: f64,
    logical_window_end_seconds: f64,
    range_start: u64,
    range_end_exclusive: u64,
    status_code: u16,
    body_complete: Option<bool>,
    bytes_observed: Option<u64>,
    request_started_monotonic_seconds: f64,
    completed_monotonic_seconds: Option<f64>,
}

#[derive(Serialize)]
struct RecoveryRangeRequest {
    elapsed_ms: u64,
    monotonic_seconds: Option<f64>,
    range_start: u64,
    range_length: u64,
    status_code: u16,
}

#[derive(Serialize)]
struct FirstCompletedSourceBody {
    resource_id: String,
    range_start: u64,
    range_end_exclusive: u64,
    status_code: u16,
    body_complete: bool,
    bytes_observed: u64,
    request_started_monotonic_seconds: f64,
    completed_monotonic_seconds: Option<f64>,
}

#[derive(Clone, Serialize)]
struct RecoveryPreparationReport {
    plan_id: u64,
    window_start_seconds: f64,
    window_end_seconds: f64,
    completed: bool,
    initial_window_readable: Option<bool>,
    window_readable: bool,
    initial_missing_to_final_readable: bool,
    newly_readable_bytes_lower_bound: u64,
    newly_readable_semantics: &'static str,
    completed_origin_range_overlaps: bool,
    player_range_overlap_status: &'static str,
    admitted_sample_monotonic_seconds: f64,
    terminal_observed_monotonic_seconds: Option<f64>,
    selected_spans: Vec<SelectedSpanReport>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadState {
    Missing,
    Readable,
    Unknown,
}

impl ReadState {
    fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Readable => "readable",
            Self::Unknown => "unknown",
        }
    }
}

fn readable_transition_lower_bound(before: ReadState, after: ReadState) -> u64 {
    u64::from(before == ReadState::Missing && after == ReadState::Readable)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WindowKey {
    context_id: String,
    session_id: String,
    session_generation: u64,
    variant_id: String,
    start_bits: u64,
    end_bits: u64,
}

#[derive(Clone)]
struct CandidateRange {
    kind: HighPreparationResourceKind,
    resource: HlsMediaResource,
    map: Arc<PlaybackSidxMap>,
    bytes: std::ops::Range<u64>,
    start_seconds: f64,
    end_seconds: f64,
    before: ReadState,
}

#[derive(Clone)]
struct BeforeWindow {
    key: WindowKey,
    accepted_identity: crate::bilibili_playback::BilibiliContentIdentity,
    start_seconds: f64,
    end_seconds: f64,
    sample_monotonic_seconds: f64,
    ranges: Vec<CandidateRange>,
}

struct CapturedPlan {
    before: BeforeWindow,
    observation: HighPreparationPlanObservation,
    admitted_sample_monotonic_seconds: f64,
    terminal_observed_monotonic_seconds: Option<f64>,
    final_states: Option<Vec<ReadState>>,
    stable_maps: bool,
}

#[derive(Default)]
struct PlanLedger {
    entries: HashMap<u64, CapturedPlan>,
    order: VecDeque<u64>,
    observed_candidates: u64,
    truncated: bool,
}

#[derive(Clone)]
struct ResourceIdentity {
    resource_id: String,
    kind: HighPreparationResourceKind,
}

#[derive(Clone)]
struct ReaderBinding {
    reader_id: String,
    context_id: String,
    session_id: String,
    session_generation: u64,
}

#[derive(Clone)]
struct RawRangeEvent {
    resource_id: String,
    kind: HighPreparationResourceKind,
    range_start: u64,
    range_end_exclusive: u64,
    status_code: u16,
    body_complete: Option<bool>,
    bytes_observed: Option<u64>,
    request_started_monotonic_seconds: f64,
    completed_monotonic_seconds: Option<f64>,
}

struct TransferLedger {
    phase: Arc<AtomicU8>,
    binding: ReaderBinding,
    origin_resources: HashMap<String, ResourceIdentity>,
    player_resources: HashMap<String, ResourceIdentity>,
    origin_events: Mutex<VecDeque<RawRangeEvent>>,
    player_events: Mutex<VecDeque<RawRangeEvent>>,
    origin_events_observed: std::sync::atomic::AtomicU64,
    player_events_observed: std::sync::atomic::AtomicU64,
    first_recovery_high_body: Mutex<Option<RawRangeEvent>>,
    origin_events_truncated: AtomicBool,
    player_events_truncated: AtomicBool,
}

#[derive(Clone)]
struct OriginBodySeed {
    event: RawRangeEvent,
}

struct OriginBodyStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>,
    ledger: Arc<TransferLedger>,
    seed: OriginBodySeed,
    bytes_observed: u64,
    finished: bool,
}

#[derive(Deserialize)]
struct NativeSample {
    sample_request_id: String,
    context_id: String,
    challenge: String,
    reader_id: String,
    session_generation: u64,
    sequence: u64,
    sample_monotonic_seconds: f64,
    playhead_seconds: Option<f64>,
    continuous_buffered_end_seconds: Option<f64>,
    playback_rate: Option<f64>,
    time_control_status: String,
}

#[derive(Serialize)]
struct NativeChallenge<'a> {
    schema_version: u32,
    sample_request_id: &'a str,
    context_id: &'a str,
    challenge: &'a str,
    reader_id: &'a str,
    session_generation: u64,
    sequence: u64,
    validity_millis: u64,
}

impl TransferLedger {
    fn new(
        phase: Arc<std::sync::atomic::AtomicU8>,
        session: &HlsPlaybackSession,
        binding: ReaderBinding,
    ) -> Result<Arc<Self>, String> {
        let audio = session
            .variant
            .audio
            .as_ref()
            .ok_or_else(|| "selected high variant has no required audio resource".to_owned())?;
        let video_identity = ResourceIdentity {
            resource_id: session.variant.video.id.clone(),
            kind: HighPreparationResourceKind::Video,
        };
        let audio_identity = ResourceIdentity {
            resource_id: audio.id.clone(),
            kind: HighPreparationResourceKind::Audio,
        };
        let mut origin_resources = HashMap::new();
        origin_resources.insert("high.mp4".to_owned(), video_identity.clone());
        origin_resources.insert("audio.mp4".to_owned(), audio_identity.clone());
        let mut player_resources = HashMap::new();
        player_resources.insert(video_identity.resource_id.clone(), video_identity);
        player_resources.insert(audio_identity.resource_id.clone(), audio_identity);
        Ok(Arc::new(Self {
            phase,
            binding,
            origin_resources,
            player_resources,
            origin_events: Mutex::new(VecDeque::new()),
            player_events: Mutex::new(VecDeque::new()),
            origin_events_observed: std::sync::atomic::AtomicU64::new(0),
            player_events_observed: std::sync::atomic::AtomicU64::new(0),
            first_recovery_high_body: Mutex::new(None),
            origin_events_truncated: AtomicBool::new(false),
            player_events_truncated: AtomicBool::new(false),
        }))
    }

    fn record_origin_event(&self, event: RawRangeEvent) {
        if event.request_started_monotonic_seconds.is_finite()
            && event.request_started_monotonic_seconds > 0.0
            && event.status_code == StatusCode::PARTIAL_CONTENT.as_u16()
            && event.body_complete == Some(true)
            && event.bytes_observed.is_some_and(|bytes| bytes > 0)
            && event.kind == HighPreparationResourceKind::Video
            && let Ok(mut first) = self.first_recovery_high_body.lock()
            && first.is_none()
        {
            *first = Some(event.clone());
        }
        push_recent(
            &self.origin_events,
            &self.origin_events_observed,
            &self.origin_events_truncated,
            event,
        );
    }

    fn record_player_event(&self, event: RawRangeEvent) {
        push_recent(
            &self.player_events,
            &self.player_events_observed,
            &self.player_events_truncated,
            event,
        );
    }
}

async fn observe_origin_body(
    State(ledger): State<Arc<TransferLedger>>,
    request: HttpRequest,
    next: Next,
) -> Response<Body> {
    let phase = ledger.phase.load(Ordering::Acquire).min(PHASE_RECOVERY);
    let capture = if phase == PHASE_RECOVERY && request.method() == Method::GET {
        (|| {
            let name = request
                .uri()
                .path()
                .strip_prefix('/')
                .filter(|path| !path.contains('/'))?;
            let identity = ledger.origin_resources.get(name)?.clone();
            let (range_start, range_end_exclusive) = parse_half_open_range(request.headers())?;
            let request_started_monotonic_seconds = monotonic_clock_seconds().ok()?;
            Some((
                identity,
                range_start,
                range_end_exclusive,
                request_started_monotonic_seconds,
            ))
        })()
    } else {
        None
    };

    let response = next.run(request).await;
    let Some((identity, range_start, range_end_exclusive, request_started_monotonic_seconds)) =
        capture
    else {
        return response;
    };
    let event = RawRangeEvent {
        resource_id: identity.resource_id,
        kind: identity.kind,
        range_start,
        range_end_exclusive,
        status_code: response.status().as_u16(),
        body_complete: None,
        bytes_observed: None,
        request_started_monotonic_seconds,
        completed_monotonic_seconds: None,
    };
    response.map(|body| {
        Body::from_stream(OriginBodyStream {
            inner: Box::pin(body.into_data_stream()),
            ledger,
            seed: OriginBodySeed { event },
            bytes_observed: 0,
            finished: false,
        })
    })
}

async fn observe_player_ingress(
    State(ledger): State<Arc<TransferLedger>>,
    request: HttpRequest,
    next: Next,
) -> Response<Body> {
    let capture = if ledger.phase.load(Ordering::Acquire) == PHASE_RECOVERY
        && player_reader_matches(request.uri(), &ledger.binding.reader_id)
    {
        player_request_identity(&request, &ledger)
            .and_then(|identity| {
                parse_half_open_range(request.headers()).map(|range| (identity, range))
            })
            .and_then(|(identity, (range_start, range_end_exclusive))| {
                monotonic_clock_seconds().ok().map(|request_started| {
                    (identity, range_start, range_end_exclusive, request_started)
                })
            })
    } else {
        None
    };
    let response = next.run(request).await;
    let Some((identity, range_start, range_end_exclusive, request_started_monotonic_seconds)) =
        capture
    else {
        return response;
    };
    ledger.record_player_event(RawRangeEvent {
        resource_id: identity.resource_id,
        kind: identity.kind,
        range_start,
        range_end_exclusive,
        status_code: response.status().as_u16(),
        body_complete: None,
        bytes_observed: None,
        request_started_monotonic_seconds,
        completed_monotonic_seconds: None,
    });
    response
}

fn player_reader_matches(uri: &axum::http::Uri, expected_reader: &str) -> bool {
    let Some(query) = uri.query() else {
        return false;
    };
    let mut matching =
        url::form_urlencoded::parse(query.as_bytes()).filter(|(name, _)| name == "playback_reader");
    matching
        .next()
        .is_some_and(|(_, value)| value == expected_reader)
        && matching.next().is_none()
}

fn player_request_identity(
    request: &HttpRequest,
    ledger: &TransferLedger,
) -> Option<ResourceIdentity> {
    let path = request.uri().path().strip_prefix("/hls/")?;
    let (session_id, resource_path) = path.split_once("/segments/")?;
    if session_id != ledger.binding.session_id || resource_path.contains('/') {
        return None;
    }
    ledger.player_resources.get(resource_path).cloned()
}

impl OriginBodyStream {
    fn finish(&mut self, body_complete: bool) {
        if self.finished {
            return;
        }
        self.finished = true;
        let mut event = self.seed.event.clone();
        let expected_bytes = event.range_end_exclusive.saturating_sub(event.range_start);
        event.body_complete = Some(body_complete && self.bytes_observed == expected_bytes);
        event.bytes_observed = Some(self.bytes_observed);
        event.completed_monotonic_seconds = monotonic_clock_seconds().ok();
        self.ledger.record_origin_event(event);
    }
}

impl Stream for OriginBodyStream {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(context) {
            Poll::Ready(Some(Ok(bytes))) => {
                this.bytes_observed = this.bytes_observed.saturating_add(bytes.len() as u64);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.finish(false);
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.finish(true);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for OriginBodyStream {
    fn drop(&mut self) {
        self.finish(false);
    }
}

fn push_recent(
    events: &Mutex<VecDeque<RawRangeEvent>>,
    observed: &std::sync::atomic::AtomicU64,
    truncated: &AtomicBool,
    event: RawRangeEvent,
) {
    let _ = observed.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        Some(value.saturating_add(1))
    });
    let Ok(mut events) = events.lock() else {
        return;
    };
    if events.len() == CAUSAL_EVENT_LIMIT {
        events.pop_front();
        truncated.store(true, Ordering::Release);
    }
    events.push_back(event);
}

impl PlanLedger {
    fn insert(&mut self, plan_id: u64, captured: CapturedPlan) {
        if self.entries.contains_key(&plan_id) {
            return;
        }
        self.observed_candidates = self.observed_candidates.saturating_add(1);
        if self.entries.len() >= TRACKED_PLAN_LIMIT {
            self.truncated = true;
            return;
        }
        self.order.push_back(plan_id);
        self.entries.insert(plan_id, captured);
    }
}

fn plan_matches_key(plan: &HighPreparationPlanObservation, key: &WindowKey) -> bool {
    plan.context_id.as_deref() == Some(key.context_id.as_str())
        && plan.session_id.as_deref() == Some(key.session_id.as_str())
        && plan.session_generation == Some(key.session_generation)
        && plan.target_variant_id.as_deref() == Some(key.variant_id.as_str())
        && plan
            .window_start_seconds
            .is_some_and(|value| value.to_bits() == key.start_bits)
        && plan
            .window_end_seconds
            .is_some_and(|value| value.to_bits() == key.end_bits)
}

fn plan_outcome_name(outcome: HighPreparationPlanOutcome) -> &'static str {
    match outcome {
        HighPreparationPlanOutcome::Admitted => "Admitted",
        HighPreparationPlanOutcome::Completed => "Completed",
        HighPreparationPlanOutcome::Deferred => "Deferred",
        HighPreparationPlanOutcome::Cancelled => "Cancelled",
        HighPreparationPlanOutcome::Failed => "Failed",
    }
}

fn span_kind_name(kind: HighPreparationResourceKind) -> &'static str {
    match kind {
        HighPreparationResourceKind::Video => "Video",
        HighPreparationResourceKind::Audio => "Audio",
    }
}

fn parse_half_open_range(headers: &axum::http::HeaderMap) -> Option<(u64, u64)> {
    let (start, inclusive_end) = numeric_range_header(headers)?;
    Some((start, inclusive_end.checked_add(1)?))
}

fn exact_read_state(
    result: Result<Option<Vec<u8>>, crate::hls_range_cache::HlsRangeError>,
    expected: u64,
) -> ReadState {
    match result {
        Ok(Some(bytes)) if u64::try_from(bytes.len()).ok() == Some(expected) => ReadState::Readable,
        Ok(None) => ReadState::Missing,
        Ok(Some(_)) | Err(_) => ReadState::Unknown,
    }
}

fn challenge_response_is_fresh(
    sample: &NativeSample,
    request_id: &str,
    ticket: &HlsPlaybackContextTicket,
    requested_at: f64,
    now: f64,
) -> bool {
    sample.sample_request_id == request_id
        && sample.context_id == ticket.context_id
        && sample.challenge == ticket.challenge
        && sample.reader_id == ticket.http_reader_id
        && sample.session_generation == ticket.session_generation
        && sample.sequence == ticket.next_sequence
        && sample.sample_monotonic_seconds.is_finite()
        && sample.sample_monotonic_seconds > requested_at
        && now.is_finite()
        && now >= sample.sample_monotonic_seconds
        && now - sample.sample_monotonic_seconds
            <= (ticket
                .validity_millis
                .min(HIGH_PREPARATION_CONTROL_VALIDITY_MILLIS) as f64
                / 1000.0)
        && sample
            .playhead_seconds
            .is_some_and(|value| value.is_finite() && value >= 0.0)
        && sample
            .continuous_buffered_end_seconds
            .is_some_and(|value| value.is_finite())
        && sample
            .continuous_buffered_end_seconds
            .zip(sample.playhead_seconds)
            .is_some_and(|(end, playhead)| end >= playhead)
        && sample
            .playback_rate
            .is_some_and(|value| value.is_finite() && value > 0.0 && value <= 16.0)
        && sample.time_control_status == "playing"
}

fn publish_challenge(directory: &Path, value: &NativeChallenge<'_>) -> Result<(), String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| "native challenge could not be encoded".to_owned())?;
    if bytes.is_empty() || bytes.len() > CONTROL_MESSAGE_LIMIT {
        return Err("native challenge exceeded its 4 KiB limit".to_owned());
    }
    let temporary = directory.join(format!("challenge-{}.tmp", value.sample_request_id));
    let destination = directory.join("challenge.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|_| "private native challenge file could not be created".to_owned())?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| "private native challenge file could not be persisted".to_owned())?;
    fs::rename(&temporary, &destination)
        .map_err(|_| "native challenge could not be atomically published".to_owned())
}

fn read_native_sample(path: &Path) -> Result<Option<NativeSample>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("native sample metadata could not be inspected".to_owned()),
    };
    if !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > CONTROL_MESSAGE_LIMIT as u64
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err("native sample was not a bounded private regular file".to_owned());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "native sample could not be opened without following links".to_owned())?;
    let opened = file
        .metadata()
        .map_err(|_| "opened native sample metadata could not be verified".to_owned())?;
    if !opened.is_file()
        || opened.dev() != metadata.dev()
        || opened.ino() != metadata.ino()
        || opened.len() != metadata.len()
    {
        return Err("native sample identity changed while it was opened".to_owned());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(CONTROL_MESSAGE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "native sample could not be read within its bound".to_owned())?;
    if bytes.is_empty() || bytes.len() > CONTROL_MESSAGE_LIMIT {
        return Err("native sample exceeded its 4 KiB limit".to_owned());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| "native sample did not match the continuous-control schema".to_owned())
}

async fn wait_for_native_sample(
    directory: &Path,
    request_id: &str,
    ticket: &HlsPlaybackContextTicket,
    requested_at: f64,
    probe: &AbortTask<Result<ProcessCapture, String>>,
) -> Result<Option<NativeSample>, String> {
    let validity = ticket
        .validity_millis
        .min(HIGH_PREPARATION_CONTROL_VALIDITY_MILLIS);
    if validity == 0 {
        return Err("server playback challenge had no validity window".to_owned());
    }
    let deadline = Instant::now() + Duration::from_millis(validity);
    let sample_path = directory.join("sample.json");
    loop {
        if probe.is_finished() {
            return Ok(None);
        }
        if let Some(sample) = read_native_sample(&sample_path)?
            && sample.sample_request_id == request_id
        {
            let now = monotonic_clock_seconds()?;
            if !challenge_response_is_fresh(&sample, request_id, ticket, requested_at, now) {
                return Err(
                    "native challenge response failed identity or freshness checks".to_owned(),
                );
            }
            return Ok(Some(sample));
        }
        if Instant::now() >= deadline {
            return Err("native challenge response exceeded its bounded validity".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn capture_before_window(
    state: &AppState,
    session: &HlsPlaybackSession,
    binding: &ReaderBinding,
    sample: PlaybackTimingSample,
    sample_monotonic_seconds: f64,
) -> Option<BeforeWindow> {
    let accepted_identity = session
        .accepted_identity
        .clone()
        .filter(crate::bilibili_playback::BilibiliContentIdentity::is_complete)?;
    let protected_end =
        sample.playhead_seconds + HIGH_PREPARATION_PROTECTED_WALL_SECONDS * sample.playback_rate;
    if !protected_end.is_finite() || sample.continuous_buffered_end_seconds < protected_end {
        return None;
    }
    let window_start = protected_end.max(sample.continuous_buffered_end_seconds);
    let requested_end = window_start + HIGH_PREPARATION_WINDOW_SECONDS;
    if !window_start.is_finite() || !requested_end.is_finite() || requested_end <= window_start {
        return None;
    }

    let mut resources = vec![(
        HighPreparationResourceKind::Video,
        session.variant.video.clone(),
    )];
    resources.push((
        HighPreparationResourceKind::Audio,
        session.variant.audio.as_ref()?.clone(),
    ));
    let mut mapped = Vec::with_capacity(resources.len());
    let mut mapped_end = f64::INFINITY;
    for (kind, resource) in resources {
        let map = state.hls_cache.playback_sidx_map(
            &binding.session_id,
            binding.session_generation,
            &resource,
        )?;
        mapped_end = mapped_end.min(map.media_end_seconds()?);
        mapped.push((kind, resource, map));
    }
    let window_end = requested_end.min(mapped_end);
    if !window_end.is_finite() || window_end <= window_start {
        return None;
    }

    let mut ranges = Vec::with_capacity(mapped.len());
    for (kind, resource, map) in mapped {
        let selected = map.preparation_range_for_window(window_start, window_end)?;
        let expected = selected.bytes.end.checked_sub(selected.bytes.start)?;
        if expected == 0 {
            return None;
        }
        let before = exact_read_state(
            state
                .hls_cache
                .read_resource_range(&binding.session_id, &resource, selected.bytes.clone())
                .await,
            expected,
        );
        ranges.push(CandidateRange {
            kind,
            resource,
            map,
            bytes: selected.bytes,
            start_seconds: selected.start_seconds,
            end_seconds: selected.end_seconds,
            before,
        });
    }

    Some(BeforeWindow {
        key: WindowKey {
            context_id: binding.context_id.clone(),
            session_id: binding.session_id.clone(),
            session_generation: binding.session_generation,
            variant_id: session.variant.id.clone(),
            start_bits: window_start.to_bits(),
            end_bits: window_end.to_bits(),
        },
        accepted_identity,
        start_seconds: window_start,
        end_seconds: window_end,
        sample_monotonic_seconds,
        ranges,
    })
}

fn plan_selected_ranges_match(
    plan: &HighPreparationPlanObservation,
    before: &BeforeWindow,
    binding: &ReaderBinding,
) -> bool {
    if plan.context_id.as_deref() != Some(binding.context_id.as_str())
        || plan.http_reader_id.as_deref() != Some(binding.reader_id.as_str())
        || plan.session_id.as_deref() != Some(binding.session_id.as_str())
        || plan.session_generation != Some(binding.session_generation)
        || plan.target_variant_id.as_deref() != Some(before.key.variant_id.as_str())
        || plan.window_start_seconds.map(f64::to_bits) != Some(before.key.start_bits)
        || plan.window_end_seconds.map(f64::to_bits) != Some(before.key.end_bits)
        || plan.selected_spans.len() != before.ranges.len()
    {
        return false;
    }
    plan.selected_spans
        .iter()
        .zip(&before.ranges)
        .all(|(span, range)| {
            span.kind == range.kind
                && span.start_seconds.to_bits() == range.start_seconds.to_bits()
                && span.end_seconds.to_bits() == range.end_seconds.to_bits()
                && span.byte_length == range.bytes.end.saturating_sub(range.bytes.start)
        })
}

async fn capture_final_states(state: &AppState, before: &BeforeWindow) -> (Vec<ReadState>, bool) {
    let Some(current) = state
        .hls_sessions
        .get_with_generation(&before.key.session_id)
        .filter(|handle| handle.generation == before.key.session_generation)
    else {
        return (vec![ReadState::Unknown; before.ranges.len()], false);
    };
    let stable_identity = current.session.accepted_identity.as_ref()
        == Some(&before.accepted_identity)
        && current.session.variant.id == before.key.variant_id;
    let mut states = Vec::with_capacity(before.ranges.len());
    let mut stable_maps = stable_identity;
    for range in &before.ranges {
        let current_map = state.hls_cache.playback_sidx_map(
            &before.key.session_id,
            before.key.session_generation,
            &range.resource,
        );
        let map_is_stable = current_map
            .as_ref()
            .is_some_and(|map| Arc::ptr_eq(map, &range.map));
        stable_maps &= map_is_stable;
        if !map_is_stable {
            states.push(ReadState::Unknown);
            continue;
        }
        let expected = range.bytes.end.saturating_sub(range.bytes.start);
        let state = exact_read_state(
            state
                .hls_cache
                .read_resource_range(&before.key.session_id, &range.resource, range.bytes.clone())
                .await,
            expected,
        );
        states.push(state);
    }
    (states, stable_maps)
}

async fn open_generated_context(
    state: &AppState,
    session_id: &str,
    generation: u64,
    reader_id: &str,
) -> Result<HlsPlaybackContextTicket, String> {
    CacheGrpcService::new(state.clone())
        .open_hls_playback_context(GrpcRequest::new(OpenHlsPlaybackContextRequest {
            session_id: session_id.to_owned(),
            expected_session_generation: generation,
            http_reader_id: reader_id.to_owned(),
        }))
        .await
        .map(|response| response.into_inner())
        .map_err(|_| "in-process generated Open rejected the test playback binding".to_owned())
}

async fn close_generated_context(state: &AppState, context_id: &str) -> Result<(), String> {
    CacheGrpcService::new(state.clone())
        .close_hls_playback_context(GrpcRequest::new(CloseHlsPlaybackContextRequest {
            context_id: context_id.to_owned(),
        }))
        .await
        .map(|_| ())
        .map_err(|_| "in-process generated Close did not retire the test context".to_owned())
}

async fn refresh_plan_ledger(
    state: &AppState,
    observation: &HighPreparationObservation,
    ledger: &mut PlanLedger,
    candidate: Option<BeforeWindow>,
    binding: &ReaderBinding,
) -> Result<(), String> {
    if let Some(before) = candidate {
        for plan in &observation.plan_history {
            if plan.plan_id == 0
                || plan.requested_resource_ranges == 0
                || !plan_matches_key(plan, &before.key)
                || !plan_selected_ranges_match(plan, &before, binding)
            {
                continue;
            }
            ledger.insert(
                plan.plan_id,
                CapturedPlan {
                    before: before.clone(),
                    observation: plan.clone(),
                    admitted_sample_monotonic_seconds: before.sample_monotonic_seconds,
                    terminal_observed_monotonic_seconds: None,
                    final_states: None,
                    stable_maps: false,
                },
            );
        }
    }

    let updates = observation
        .plan_history
        .iter()
        .filter(|plan| ledger.entries.contains_key(&plan.plan_id))
        .map(|plan| (plan.plan_id, plan.clone()))
        .collect::<Vec<_>>();
    for (plan_id, plan) in updates {
        let needs_final_read = ledger.entries.get(&plan_id).is_some_and(|entry| {
            plan.outcome != HighPreparationPlanOutcome::Admitted && entry.final_states.is_none()
        });
        let final_read = if needs_final_read {
            let before = ledger
                .entries
                .get(&plan_id)
                .map(|entry| entry.before.clone())
                .ok_or_else(|| "captured plan disappeared before final inspection".to_owned())?;
            Some(capture_final_states(state, &before).await)
        } else {
            None
        };
        if let Some(entry) = ledger.entries.get_mut(&plan_id) {
            entry.observation = plan.clone();
            if let Some((states, stable_maps)) = final_read {
                entry.final_states = Some(states);
                entry.stable_maps = stable_maps;
                entry.terminal_observed_monotonic_seconds = monotonic_clock_seconds().ok();
            }
        }
    }
    Ok(())
}

// Keep the generated-control and fixture inputs explicit at this test boundary.
#[allow(clippy::too_many_arguments)]
async fn submit_challenged_update(
    state: &AppState,
    session: &HlsPlaybackSession,
    binding: &ReaderBinding,
    control_directory: &Path,
    probe: &AbortTask<Result<ProcessCapture, String>>,
    ticket: &HlsPlaybackContextTicket,
    ledger: &mut PlanLedger,
    capture_before_state: bool,
) -> Result<Option<(HlsPlaybackContextTicket, HighPreparationObservation)>, String> {
    if ticket.context_id != binding.context_id
        || ticket.http_reader_id != binding.reader_id
        || ticket.session_id != binding.session_id
        || ticket.session_generation != binding.session_generation
    {
        return Err("server ticket no longer matches its reader/context binding".to_owned());
    }
    let request_id = Uuid::new_v4().to_string();
    let requested_at = monotonic_clock_seconds()?;
    let challenge = NativeChallenge {
        schema_version: 1,
        sample_request_id: &request_id,
        context_id: &ticket.context_id,
        challenge: &ticket.challenge,
        reader_id: &ticket.http_reader_id,
        session_generation: ticket.session_generation,
        sequence: ticket.next_sequence,
        validity_millis: ticket
            .validity_millis
            .min(HIGH_PREPARATION_CONTROL_VALIDITY_MILLIS),
    };
    publish_challenge(control_directory, &challenge)?;
    let Some(native) =
        wait_for_native_sample(control_directory, &request_id, ticket, requested_at, probe).await?
    else {
        return Ok(None);
    };
    let sample = PlaybackTimingSample {
        playhead_seconds: native
            .playhead_seconds
            .ok_or_else(|| "native playhead was unavailable".to_owned())?,
        continuous_buffered_end_seconds: native
            .continuous_buffered_end_seconds
            .ok_or_else(|| "native continuous loaded end was unavailable".to_owned())?,
        playback_rate: native
            .playback_rate
            .ok_or_else(|| "native playback rate was unavailable".to_owned())?,
    };
    let candidate = if capture_before_state {
        capture_before_window(
            state,
            session,
            binding,
            sample,
            native.sample_monotonic_seconds,
        )
        .await
    } else {
        None
    };
    let now = monotonic_clock_seconds()?;
    if !challenge_response_is_fresh(&native, &request_id, ticket, requested_at, now) {
        return Err("native sample expired before its generated Update was submitted".to_owned());
    }
    let next = CacheGrpcService::new(state.clone())
        .update_hls_playback_context(GrpcRequest::new(UpdateHlsPlaybackContextRequest {
            context_id: ticket.context_id.clone(),
            challenge: ticket.challenge.clone(),
            sequence: ticket.next_sequence,
            sample: Some(HlsPlaybackTimingSample {
                playhead_seconds: sample.playhead_seconds,
                continuous_buffered_end_seconds: sample.continuous_buffered_end_seconds,
                playback_rate: sample.playback_rate,
            }),
        }))
        .await
        .map_err(|_| "in-process generated Update rejected the fresh native sample".to_owned())?
        .into_inner();
    if next.context_id != ticket.context_id
        || next.http_reader_id != binding.reader_id
        || next.session_id != binding.session_id
        || next.session_generation != binding.session_generation
        || next.next_sequence != ticket.next_sequence.saturating_add(1)
        || next.challenge == ticket.challenge
    {
        return Err("generated Update returned a stale or mismatched next ticket".to_owned());
    }
    let observation = state.hls_high_preparation.observation_for_tests();
    refresh_plan_ledger(state, &observation, ledger, candidate, binding).await?;
    Ok(Some((next, observation)))
}

fn selected_span_reports(
    captured: &CapturedPlan,
    binding: &ReaderBinding,
) -> Vec<SelectedSpanReport> {
    let final_states = captured.final_states.as_deref().unwrap_or(&[]);
    captured
        .before
        .ranges
        .iter()
        .enumerate()
        .map(|(index, range)| {
            let after = if captured.stable_maps {
                final_states
                    .get(index)
                    .copied()
                    .unwrap_or(ReadState::Unknown)
            } else {
                ReadState::Unknown
            };
            let lower_bound = readable_transition_lower_bound(range.before, after);
            SelectedSpanReport {
                kind: span_kind_name(range.kind),
                resource_id: range.resource.id.clone(),
                variant_id: captured.before.key.variant_id.clone(),
                context_id: binding.context_id.clone(),
                session_id: binding.session_id.clone(),
                session_generation: binding.session_generation,
                logical_window_start_seconds: captured.before.start_seconds,
                logical_window_end_seconds: captured.before.end_seconds,
                byte_range_start: range.bytes.start,
                byte_range_end_exclusive: range.bytes.end,
                before_read_state: range.before.label(),
                after_read_state: after.label(),
                newly_readable_bytes_lower_bound: lower_bound,
                readable: after == ReadState::Readable,
            }
        })
        .collect()
}

fn event_overlaps_range(event: &RawRangeEvent, range: &CandidateRange) -> bool {
    event.resource_id == range.resource.id
        && event.kind == range.kind
        && event.range_start < range.bytes.end
        && range.bytes.start < event.range_end_exclusive
}

fn reported_transfer(
    event: &RawRangeEvent,
    captured: &CapturedPlan,
    binding: &ReaderBinding,
    range: &CandidateRange,
) -> RangeTransferReport {
    RangeTransferReport {
        context_id: binding.context_id.clone(),
        reader_id: binding.reader_id.clone(),
        session_id: binding.session_id.clone(),
        session_generation: binding.session_generation,
        variant_id: captured.before.key.variant_id.clone(),
        resource_id: range.resource.id.clone(),
        logical_window_start_seconds: captured.before.start_seconds,
        logical_window_end_seconds: captured.before.end_seconds,
        range_start: event.range_start,
        range_end_exclusive: event.range_end_exclusive,
        status_code: event.status_code,
        body_complete: event.body_complete,
        bytes_observed: event.bytes_observed,
        request_started_monotonic_seconds: event.request_started_monotonic_seconds,
        completed_monotonic_seconds: event.completed_monotonic_seconds,
    }
}

struct PlanAssessment {
    report: PlanReport,
    recovery: RecoveryPreparationReport,
    origin_events: Vec<RangeTransferReport>,
    player_events: Vec<RangeTransferReport>,
    eligible: bool,
}

fn assess_plan(
    captured: &CapturedPlan,
    binding: &ReaderBinding,
    transfers: &TransferLedger,
) -> PlanAssessment {
    let spans = selected_span_reports(captured, binding);
    let lower_bound = spans
        .iter()
        .map(|span| span.newly_readable_bytes_lower_bound)
        .sum::<u64>();
    let all_final_readable = !spans.is_empty() && spans.iter().all(|span| span.readable);
    let initial_incomplete = captured.observation.initial_window_readable == Some(false);
    let final_complete = captured.observation.window_readable && all_final_readable;
    let mut origin_events = Vec::new();
    let mut player_events = Vec::new();

    if let Ok(events) = transfers.origin_events.lock() {
        for range in &captured.before.ranges {
            if let Some(event) = events.iter().find(|event| {
                event.status_code == StatusCode::PARTIAL_CONTENT.as_u16()
                    && event.body_complete == Some(true)
                    && event.request_started_monotonic_seconds
                        >= captured.admitted_sample_monotonic_seconds
                    && event.bytes_observed
                        == Some(event.range_end_exclusive.saturating_sub(event.range_start))
                    && event_overlaps_range(event, range)
            }) {
                origin_events.push(reported_transfer(event, captured, binding, range));
            }
        }
    }
    if let Ok(events) = transfers.player_events.lock() {
        for range in &captured.before.ranges {
            if let Some(event) = events.iter().find(|event| {
                event.status_code == StatusCode::PARTIAL_CONTENT.as_u16()
                    && event.request_started_monotonic_seconds
                        >= captured.admitted_sample_monotonic_seconds
                    && event_overlaps_range(event, range)
            }) {
                player_events.push(reported_transfer(event, captured, binding, range));
            }
        }
    }
    let completed_origin_overlap = !origin_events.is_empty();
    let player_overlap = !player_events.is_empty();
    let player_status = if player_overlap {
        "yes"
    } else if transfers.player_events_truncated.load(Ordering::Acquire) {
        "unknown"
    } else {
        "no"
    };
    let stable_ranges = captured.stable_maps
        && captured.final_states.as_ref().is_some_and(|states| {
            states.len() == captured.before.ranges.len()
                && states.iter().all(|state| *state == ReadState::Readable)
        });
    let initial_missing_to_final_readable =
        initial_incomplete && final_complete && stable_ranges && lower_bound > 0;
    let eligible = captured.observation.outcome == HighPreparationPlanOutcome::Completed
        && initial_missing_to_final_readable
        && completed_origin_overlap
        && player_overlap;

    let plan_report = PlanReport {
        plan_id: captured.observation.plan_id,
        context_id: captured.before.key.context_id.clone(),
        reader_id: binding.reader_id.clone(),
        session_id: captured.before.key.session_id.clone(),
        session_generation: captured.before.key.session_generation,
        target_variant_id: captured.observation.target_variant_id.clone(),
        window_start_seconds: captured.before.start_seconds,
        window_end_seconds: captured.before.end_seconds,
        initial_window_readable: captured.observation.initial_window_readable,
        outcome: plan_outcome_name(captured.observation.outcome),
        window_readable: captured.observation.window_readable,
        admitted_sample_monotonic_seconds: captured.admitted_sample_monotonic_seconds,
        terminal_observed_monotonic_seconds: captured.terminal_observed_monotonic_seconds,
        selected_spans: spans.clone(),
    };
    let recovery = RecoveryPreparationReport {
        plan_id: captured.observation.plan_id,
        window_start_seconds: captured.before.start_seconds,
        window_end_seconds: captured.before.end_seconds,
        completed: captured.observation.outcome == HighPreparationPlanOutcome::Completed,
        initial_window_readable: captured.observation.initial_window_readable,
        window_readable: captured.observation.window_readable && stable_ranges,
        initial_missing_to_final_readable,
        newly_readable_bytes_lower_bound: lower_bound,
        newly_readable_semantics: "one_byte_lower_bound_per_exact_range_none_to_full_some_transition",
        completed_origin_range_overlaps: completed_origin_overlap,
        player_range_overlap_status: player_status,
        admitted_sample_monotonic_seconds: captured.admitted_sample_monotonic_seconds,
        terminal_observed_monotonic_seconds: captured.terminal_observed_monotonic_seconds,
        selected_spans: spans,
    };
    PlanAssessment {
        report: plan_report,
        recovery,
        origin_events,
        player_events,
        eligible,
    }
}

fn decode_first_recovery_high(probe: &Value) -> Option<f64> {
    let entries = probe
        .get("video_size_expectations")?
        .get("entries")?
        .as_array()?;
    let high = entries.get(2)?;
    if !high.get("matched")?.as_bool()? {
        return None;
    }
    high.get("samples")?
        .as_array()?
        .first()?
        .get("observed_monotonic_seconds")?
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0)
}

fn first_high_request(report: &ReportStore) -> Option<RecoveryRangeRequest> {
    let report = report.lock().ok()?;
    let request = report.requests.first_recovery_high_range_request.as_ref()?;
    Some(RecoveryRangeRequest {
        elapsed_ms: request.elapsed_ms,
        monotonic_seconds: request.monotonic_seconds,
        range_start: request.range_start,
        range_length: request.range_length,
        status_code: request.status_code,
    })
}

fn first_completed_high_body(transfers: &TransferLedger) -> Option<FirstCompletedSourceBody> {
    let event = transfers.first_recovery_high_body.lock().ok()?.clone()?;
    Some(FirstCompletedSourceBody {
        resource_id: event.resource_id,
        range_start: event.range_start,
        range_end_exclusive: event.range_end_exclusive,
        status_code: event.status_code,
        body_complete: event.body_complete == Some(true),
        bytes_observed: event.bytes_observed.unwrap_or_default(),
        request_started_monotonic_seconds: event.request_started_monotonic_seconds,
        completed_monotonic_seconds: event.completed_monotonic_seconds,
    })
}

fn inconclusive_reason(assessments: &[PlanAssessment]) -> String {
    let Some(assessment) = assessments.last() else {
        return "no_recovery_plan_with_exact_before_state".to_owned();
    };
    let recovery = &assessment.recovery;
    if recovery.completed && recovery.initial_window_readable != Some(false) {
        "recovery_window_was_not_proven_initially_incomplete".to_owned()
    } else if !recovery.completed || !recovery.window_readable {
        "selected_required_audio_video_ranges_did_not_finish_readable".to_owned()
    } else if recovery.newly_readable_bytes_lower_bound == 0 {
        "no_exact_missing_to_readable_range_transition_was_proven".to_owned()
    } else if !recovery.completed_origin_range_overlaps {
        "no_completed_origin_range_overlap_after_plan_admission".to_owned()
    } else if recovery.player_range_overlap_status != "yes" {
        "no_matching_reader_bound_player_range_overlap_was_proven".to_owned()
    } else {
        "recovery_plan_did_not_meet_every_eligibility_guard".to_owned()
    }
}

// Keep report inputs explicit so the OFF/ON comparison provenance stays visible.
#[allow(clippy::too_many_arguments)]
fn build_arm_report(
    mode: ArmMode,
    binding: &ReaderBinding,
    native_probe: &Value,
    challenge_count: u64,
    control_error: Option<&str>,
    report: &ReportStore,
    transfer_ledger: &TransferLedger,
    observation: &HighPreparationObservation,
    plans: &PlanLedger,
) -> Result<ArmReport, String> {
    let continuous = native_probe
        .get("high_preparation_continuous")
        .ok_or_else(|| "probe omitted its continuous-control report".to_owned())?;
    if continuous.get("mode").and_then(Value::as_str) != Some("continuous_native_control_v1") {
        return Err("probe did not report the continuous-only mode".to_owned());
    }
    let muted_verified = continuous
        .get("muted_verified")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let automatic_waiting_enabled = continuous
        .get("automatic_waiting_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let system_audio_mute_verified =
        std::env::var("LAN_MEDIA_HIGH_PREPARATION_SYSTEM_MUTED").as_deref() == Ok("verified");
    if !muted_verified || !automatic_waiting_enabled || !system_audio_mute_verified {
        return Err("native player or system mute/automatic buffering was not verified".to_owned());
    }
    let probe_status = native_probe
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let phase_completed = matches!(probe_status, "completed" | "inconclusive")
        && continuous
            .get("ready_marker_published")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    if !phase_completed {
        return Err("continuous native automatic phase did not complete".to_owned());
    }
    if mode == ArmMode::Off && control_error.is_some() {
        return Err("OFF arm did not complete every challenged generated Update".to_owned());
    }
    let high_low_high_decoded = continuous
        .get("high_low_high_decoded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let assessments = plans
        .order
        .iter()
        .filter_map(|plan_id| plans.entries.get(plan_id))
        .map(|plan| assess_plan(plan, binding, transfer_ledger))
        .collect::<Vec<_>>();
    let chosen = assessments
        .iter()
        .position(|assessment| assessment.eligible)
        .or_else(|| assessments.len().checked_sub(1));
    let chosen_assessment = chosen.and_then(|index| assessments.get(index));
    let eligible = mode.enabled()
        && control_error.is_none()
        && !plans.truncated
        && chosen_assessment.is_some_and(|assessment| assessment.eligible);
    let status = if mode == ArmMode::Off || eligible {
        "complete"
    } else {
        "inconclusive"
    };
    let origin_events = chosen_assessment
        .map(|assessment| assessment.origin_events.clone())
        .unwrap_or_default();
    let player_events = chosen_assessment
        .map(|assessment| assessment.player_events.clone())
        .unwrap_or_default();
    let recovery_preparation = chosen_assessment.map(|assessment| assessment.recovery.clone());
    let reason = if eligible || mode == ArmMode::Off {
        None
    } else if plans.truncated {
        Some("matching_plan_history_exceeded_the_bounded_retention_limit".to_owned())
    } else if let Some(error) = control_error {
        Some(error.chars().take(160).collect())
    } else if let Some(assessment) = chosen_assessment {
        Some(inconclusive_reason(std::slice::from_ref(assessment)))
    } else {
        Some("no_recovery_plan_with_exact_before_state".to_owned())
    };
    let plan_reports = chosen_assessment
        .map(|assessment| vec![assessment.report.clone()])
        .unwrap_or_default();
    let challenge_samples_reported = continuous
        .get("challenge_sample_count")
        .and_then(Value::as_u64)
        .unwrap_or(challenge_count);
    if challenge_samples_reported > CONTROL_SAMPLE_LIMIT as u64 {
        return Err("continuous native probe exceeded its challenge response bound".to_owned());
    }
    let logical_range_counts = LogicalRangeCounts {
        requested_resource_ranges: observation.requested_resource_ranges,
        readable_resource_ranges: observation.readable_resource_ranges,
        deferred_resource_ranges: observation.deferred_resource_ranges,
    };
    let plan_history_retention = PlanHistoryRetentionReport {
        observed_matching_candidates: plans.observed_candidates,
        internally_retained_records: plans.entries.len(),
        internal_record_limit: TRACKED_PLAN_LIMIT,
        serialized_plan_records: plan_reports.len(),
        serialization_record_limit: 1,
        producer_history_limit: TRACKED_PLAN_LIMIT,
        truncated: plans.truncated,
    };
    let transfer_event_retention = TransferEventRetentionReport {
        origin_recovery_range_events_observed: transfer_ledger
            .origin_events_observed
            .load(Ordering::Acquire),
        origin_witnesses_retained: origin_events.len(),
        origin_ring_limit: CAUSAL_EVENT_LIMIT,
        origin_ring_truncated: transfer_ledger
            .origin_events_truncated
            .load(Ordering::Acquire),
        player_recovery_range_events_observed: transfer_ledger
            .player_events_observed
            .load(Ordering::Acquire),
        player_witnesses_retained: player_events.len(),
        player_ring_limit: CAUSAL_EVENT_LIMIT,
        player_ring_truncated: transfer_ledger
            .player_events_truncated
            .load(Ordering::Acquire),
    };
    Ok(ArmReport {
        schema_version: 1,
        arm: mode.label(),
        status,
        eligible,
        transport_boundary: ARM_TRANSPORT_BOUNDARY,
        fixture_generation_timeout_seconds: OPT_IN_FIXTURE_GENERATION_TIMEOUT.as_secs(),
        fixture_is_fresh: true,
        cache_is_fresh: true,
        reader: ReaderReport {
            id: binding.reader_id.clone(),
            context_id: binding.context_id.clone(),
            session_id: binding.session_id.clone(),
            session_generation: binding.session_generation,
        },
        native: NativeReport {
            mode: "continuous_native_control_v1",
            requested_duration_seconds: 600,
            deadline_seconds: 720,
            phase_completed,
            muted_verified,
            system_audio_mute_verified,
            automatic_waiting_enabled,
            high_low_high_decoded,
            decode_censored: eligible && !high_low_high_decoded,
            seek_count: 0,
            rate_change_count: 0,
            item_replacement_count: 0,
            buffer_cap_set: false,
            challenge_sample_count: challenge_samples_reported,
            challenge_samples_truncated: continuous
                .get("challenge_samples_truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        plans: plan_reports,
        origin_range_transfers: origin_events,
        player_range_transfers: player_events,
        plan_history_retention,
        transfer_event_retention,
        logical_range_counts,
        attribution: AttributionReport {
            exclusive_attribution: "not_proven",
            causal_faster_selection_claim: false,
        },
        first_recovery_high_origin_request: first_high_request(report),
        first_recovery_high_completed_source_body: first_completed_high_body(transfer_ledger),
        first_recovery_high_decoded_monotonic_seconds: decode_first_recovery_high(native_probe),
        recovery_preparation,
        inconclusive_reason: reason,
    })
}

async fn run_native_high_preparation_probe(
    probe_path: PathBuf,
    master_url: String,
    reader_id: &str,
    ready_marker: &Path,
    control_directory: &Path,
) -> Result<ProcessCapture, String> {
    ensure_loopback_url(&master_url)?;
    let mut command = Command::new(probe_path);
    command.args([
        "--url".to_owned(),
        master_url,
        "--duration".to_owned(),
        "600".to_owned(),
        "--deadline".to_owned(),
        "720".to_owned(),
        "--expect-video-sizes".to_owned(),
        "640x360,320x180,640x360".to_owned(),
        "--ready-marker".to_owned(),
        ready_marker.to_string_lossy().into_owned(),
        "--reader-id".to_owned(),
        reader_id.to_owned(),
        "--control-directory".to_owned(),
        control_directory.to_string_lossy().into_owned(),
        "--high-preparation-continuous".to_owned(),
    ]);
    run_process(
        command,
        "native-high-preparation-continuous-probe",
        Duration::from_secs(720) + PROBE_EXIT_GRACE,
        MAX_PROBE_JSON_BYTES,
    )
    .await
}

async fn validate_reader_bound_master_playlist(
    client: &reqwest::Client,
    bound_master_url: &str,
    reader_id: &str,
    case: NativeCase,
    expected: PlaylistExpectations,
    long_profile: bool,
) -> Result<HlsPlaylistPreflight, String> {
    ensure_loopback_url(bound_master_url)?;
    let bound_master_url = url::Url::parse(bound_master_url)
        .map_err(|_| "reader-bound HLS master URL was invalid".to_owned())?;
    let mut master_query = bound_master_url.query_pairs();
    if !master_query
        .next()
        .is_some_and(|(name, value)| name == "playback_reader" && value == reader_id)
        || master_query.next().is_some()
    {
        return Err("reader-bound HLS master query did not identify exactly one reader".to_owned());
    }
    let mut canonical_master_url = bound_master_url.clone();
    canonical_master_url.set_query(None);
    let expected_video_fragments = expected.video_fragments;
    let expected_audio_fragments = expected.audio_fragments;

    let preflight = validate_master_playlist(
        client,
        canonical_master_url.as_str(),
        case,
        expected,
        long_profile,
    )
    .await?;
    let maximum_bytes = playlist_response_limit(long_profile);
    let canonical_master = fetch_playlist(client, &canonical_master_url, maximum_bytes).await?;
    let bound_master = fetch_playlist(client, &bound_master_url, maximum_bytes).await?;
    compare_reader_bound_playlist(
        &canonical_master_url,
        &bound_master_url,
        &canonical_master,
        &bound_master,
        reader_id,
        false,
    )?;

    let canonical_selected = selected_high_playlist_uris(&canonical_master)?;
    let bound_selected = selected_high_playlist_uris(&bound_master)?;
    let canonical_video_url =
        resolve_same_origin_playlist(&canonical_master_url, &canonical_selected.video_uri)?;
    let bound_video_url =
        resolve_same_origin_playlist(&bound_master_url, &bound_selected.video_uri)?;
    compare_bound_and_canonical_media(
        client,
        &canonical_video_url,
        &bound_video_url,
        reader_id,
        expected_video_fragments,
        maximum_bytes,
    )
    .await?;

    match (canonical_selected.audio_uri, bound_selected.audio_uri) {
        (Some(canonical_uri), Some(bound_uri)) => {
            let canonical_url =
                resolve_same_origin_playlist(&canonical_master_url, &canonical_uri)?;
            let bound_url = resolve_same_origin_playlist(&bound_master_url, &bound_uri)?;
            compare_bound_and_canonical_media(
                client,
                &canonical_url,
                &bound_url,
                reader_id,
                expected_audio_fragments,
                maximum_bytes,
            )
            .await?;
        }
        (None, None) => {}
        _ => return Err("reader-bound master changed its selected audio playlist".to_owned()),
    }
    Ok(preflight)
}

async fn compare_bound_and_canonical_media(
    client: &reqwest::Client,
    canonical_url: &url::Url,
    bound_url: &url::Url,
    reader_id: &str,
    expected_fragments: u64,
    maximum_bytes: usize,
) -> Result<(), String> {
    let canonical_body = fetch_playlist(client, canonical_url, maximum_bytes).await?;
    let bound_body = fetch_playlist(client, bound_url, maximum_bytes).await?;
    validate_media_playlist_body(canonical_url, &canonical_body, expected_fragments)?;
    compare_reader_bound_playlist(
        canonical_url,
        bound_url,
        &canonical_body,
        &bound_body,
        reader_id,
        true,
    )
}

fn compare_reader_bound_playlist(
    canonical_url: &url::Url,
    bound_url: &url::Url,
    canonical_body: &str,
    bound_body: &str,
    reader_id: &str,
    media_playlist: bool,
) -> Result<(), String> {
    let canonical_lines = canonical_body.lines().map(str::trim).collect::<Vec<_>>();
    let bound_lines = bound_body.lines().map(str::trim).collect::<Vec<_>>();
    if canonical_lines.len() != bound_lines.len() {
        return Err("reader-bound HLS playlist changed its line structure".to_owned());
    }
    let mut index = 0;
    while index < canonical_lines.len() {
        let canonical_line = canonical_lines[index];
        let bound_line = bound_lines[index];
        if media_playlist && canonical_line.starts_with("#EXTINF:") {
            if canonical_line != bound_line
                || bound_lines.get(index + 1) != canonical_lines.get(index + 1)
            {
                return Err("reader-bound HLS segment duration or range changed".to_owned());
            }
            let range = canonical_lines
                .get(index + 1)
                .and_then(|line| line.strip_prefix("#EXT-X-BYTERANGE:"))
                .and_then(parse_hls_byte_range)
                .ok_or_else(|| "canonical HLS segment range was invalid".to_owned())?;
            let canonical_uri = canonical_lines
                .get(index + 2)
                .ok_or_else(|| "canonical HLS segment URI was missing".to_owned())?;
            let bound_uri = bound_lines
                .get(index + 2)
                .ok_or_else(|| "reader-bound HLS segment URI was missing".to_owned())?;
            compare_bound_reference(
                canonical_url,
                bound_url,
                canonical_uri,
                bound_uri,
                reader_id,
                Some(range),
            )?;
            index += 3;
            continue;
        }
        if canonical_line.starts_with('#') {
            if let (Some((canonical_tag, canonical_attrs)), Some((bound_tag, bound_attrs))) = (
                hls_uri_attributes(canonical_line),
                hls_uri_attributes(bound_line),
            ) {
                if canonical_tag != bound_tag {
                    return Err("reader-bound HLS URI tag changed".to_owned());
                }
                let mut canonical_other = canonical_attrs.clone();
                let mut bound_other = bound_attrs.clone();
                let canonical_uri = canonical_other.remove("URI");
                let bound_uri = bound_other.remove("URI");
                if canonical_other != bound_other {
                    return Err("reader-bound HLS URI attributes changed".to_owned());
                }
                match (canonical_uri, bound_uri) {
                    (Some(canonical_uri), Some(bound_uri)) => compare_bound_reference(
                        canonical_url,
                        bound_url,
                        &canonical_uri,
                        &bound_uri,
                        reader_id,
                        None,
                    )?,
                    (None, None) => {}
                    _ => {
                        return Err(
                            "reader-bound HLS URI attribute was added or removed".to_owned()
                        );
                    }
                }
            } else if canonical_line != bound_line {
                return Err("reader-bound HLS playlist metadata changed".to_owned());
            }
        } else if !canonical_line.is_empty() {
            compare_bound_reference(
                canonical_url,
                bound_url,
                canonical_line,
                bound_line,
                reader_id,
                None,
            )?;
        } else if canonical_line != bound_line {
            return Err("reader-bound HLS playlist structure changed".to_owned());
        }
        index += 1;
    }
    Ok(())
}

fn hls_uri_attributes(line: &str) -> Option<(&str, HashMap<String, String>)> {
    let (tag, value) = line.strip_prefix('#')?.split_once(':')?;
    let attributes = parse_hls_attributes(value)?;
    attributes.contains_key("URI").then_some((tag, attributes))
}

fn compare_bound_reference(
    canonical_base: &url::Url,
    bound_base: &url::Url,
    canonical_uri: &str,
    bound_uri: &str,
    reader_id: &str,
    expected_range: Option<(u64, u64)>,
) -> Result<(), String> {
    let canonical = canonical_base
        .join(canonical_uri)
        .map_err(|_| "canonical HLS URI could not be resolved".to_owned())?;
    let bound = bound_base
        .join(bound_uri)
        .map_err(|_| "reader-bound HLS URI could not be resolved".to_owned())?;
    if canonical.origin() != canonical_base.origin()
        || canonical.username() != ""
        || canonical.password().is_some()
        || canonical.fragment().is_some()
        || !canonical.path().starts_with('/')
    {
        return Err("canonical HLS URI was not same-origin".to_owned());
    }
    let normalized = validate_and_remove_reader(&bound, bound_base, reader_id, expected_range)?;
    let mut canonical_normalized = canonical.clone();
    validate_optional_segment_query(&mut canonical_normalized, expected_range)?;
    if normalized != canonical_normalized {
        return Err("reader-bound HLS URI did not match canonical content".to_owned());
    }
    ensure_loopback_url(bound.as_str())
}

fn validate_and_remove_reader(
    bound: &url::Url,
    base: &url::Url,
    reader_id: &str,
    expected_range: Option<(u64, u64)>,
) -> Result<url::Url, String> {
    if bound.origin() != base.origin()
        || bound.username() != ""
        || bound.password().is_some()
        || bound.fragment().is_some()
        || !bound.path().starts_with('/')
    {
        return Err("reader-bound HLS URI was not same-origin".to_owned());
    }
    let mut saw_reader = false;
    let mut normalized = bound.clone();
    for (name, value) in bound.query_pairs() {
        match name.as_ref() {
            "playback_reader" if !saw_reader && value == reader_id => saw_reader = true,
            "segment" => {}
            _ => return Err("reader-bound HLS URI had an unknown or duplicate query".to_owned()),
        }
    }
    if !saw_reader {
        return Err("reader-bound HLS URI omitted its reader".to_owned());
    }
    let mut pairs = bound
        .query_pairs()
        .filter(|(name, _)| name != "playback_reader");
    let remaining = pairs
        .next()
        .map(|(name, value)| (name.into_owned(), value.into_owned()));
    if pairs.next().is_some() {
        return Err("reader-bound HLS URI had multiple non-reader queries".to_owned());
    }
    normalized.set_query(None);
    if let Some((name, value)) = remaining {
        if name != "segment"
            || expected_range
                .map(|(offset, length)| format!("{offset}-{length}") != value)
                .unwrap_or(true)
        {
            return Err("reader-bound HLS segment query did not match its byte range".to_owned());
        }
        normalized.query_pairs_mut().append_pair(&name, &value);
    }
    Ok(normalized)
}

fn validate_optional_segment_query(
    canonical: &mut url::Url,
    expected_range: Option<(u64, u64)>,
) -> Result<(), String> {
    let mut pairs = canonical.query_pairs();
    let first = pairs.next();
    if pairs.next().is_some() {
        return Err("canonical HLS URI had multiple queries".to_owned());
    }
    match first {
        None => {}
        Some((name, value))
            if name == "segment"
                && expected_range
                    .is_some_and(|(offset, length)| value == format!("{offset}-{length}")) => {}
        _ => return Err("canonical HLS segment query did not match its byte range".to_owned()),
    }
    Ok(())
}

async fn run_arm(mode: ArmMode) -> Result<ArmReport, String> {
    if std::env::var("LAN_MEDIA_HIGH_PREPARATION_SYSTEM_MUTED").as_deref() != Ok("verified") {
        return Err("system audio mute was not verified before native playback".to_owned());
    }
    let probe_path = required_executable("LAN_MEDIA_ACCEPTANCE_PROBE_BINARY")?;
    let ffmpeg = find_executable("ffmpeg")?;
    let ffprobe = find_executable("ffprobe")?;
    let temporary = tempfile::Builder::new()
        .prefix("lan-high-preparation-")
        .tempdir()
        .map_err(|_| "fresh arm fixture directory could not be created".to_owned())?;
    let temporary_root = temporary
        .path()
        .canonicalize()
        .map_err(|_| "fresh arm fixture directory could not be canonicalized".to_owned())?;
    let directories = create_case_directories(&temporary_root).await?;
    let fixture_report = Arc::new(Mutex::new(AcceptanceReport::new(
        NativeCase::ProgressiveAbr,
    )));
    let fixture = generate_and_inspect_fixture(
        NativeCase::ProgressiveAbr,
        &ffmpeg,
        &ffprobe,
        &directories.fixtures,
        LONG_RECOVERY_MEDIA_DURATION_SECONDS,
        &fixture_report,
        OPT_IN_FIXTURE_GENERATION_TIMEOUT,
    )
    .await
    .map_err(|error| format!("phase=fixture-preparation; {error}"))?;
    enforce_fixture_budget(NativeCase::ProgressiveAbr, &fixture)?;

    let origin_listener = bind_loopback_listener().await?;
    let origin_address = origin_listener
        .local_addr()
        .map_err(|_| "fixture origin loopback address is unavailable".to_owned())?;
    let origin = make_origin_state(
        NativeCase::ProgressiveAbr,
        &fixture,
        origin_address,
        Arc::clone(&fixture_report),
    )?;
    let media_listener = bind_loopback_listener().await?;
    let media_address = media_listener
        .local_addr()
        .map_err(|_| "fixture media loopback address is unavailable".to_owned())?;
    let mut state = create_app_state(
        &temporary_root,
        directories,
        media_address,
        &ffmpeg,
        false,
        NativeCase::ProgressiveAbr.cache_limit_bytes(),
    )?;
    state.hls_upstream_client = local_only_http_client()?;
    state
        .hls_high_preparation
        .set_enabled_for_tests(mode.enabled());

    let task = state
        .tasks
        .create_bilibili_playback_task("native-high-preparation", None, None)
        .map_err(|_| "credential-free arm playback task could not be created".to_owned())?;
    let task_id = task.task.id;
    let policy = PlaybackPolicy {
        transcoding_preference: TranscodingPreference::Auto,
        weak_network_preference: WeakNetworkPreference::AvPlayerManaged,
        ..PlaybackPolicy::default()
    };
    let variants = create_source_variants(NativeCase::ProgressiveAbr, &fixture, origin_address)?;
    let selected = variants
        .first()
        .ok_or_else(|| "arm fixture did not contain the high primary variant".to_owned())?;
    let abr = make_abr_metadata(NativeCase::ProgressiveAbr, &fixture)?;
    let session = HlsPlaybackSession::from_playback_entry_with_policy(
        &task_id,
        "Native high-preparation comparison",
        selected,
        &abr,
        &variants,
        policy,
    )
    .map_err(|_| "production HLS arm session could not be constructed".to_owned())?;
    let master_url = format!("http://{media_address}/hls/{task_id}/master.m3u8");
    publish_playable_session(&state, &session, &task_id, &master_url)?;
    let generation = state
        .hls_sessions
        .get_with_generation(&task_id)
        .map(|handle| handle.generation)
        .ok_or_else(|| "published arm session generation could not be read".to_owned())?;
    let reader_id = Uuid::new_v4().to_string();
    let middleware_binding = ReaderBinding {
        reader_id: reader_id.clone(),
        context_id: String::new(),
        session_id: task_id.clone(),
        session_generation: generation,
    };
    let transfer_ledger =
        TransferLedger::new(Arc::clone(&origin.phase), &session, middleware_binding)?;

    let origin_router = Router::new()
        .route("/{file_name}", get(origin_get).head(origin_head))
        .with_state(Arc::clone(&origin))
        .layer(from_fn_with_state(
            Arc::clone(&transfer_ledger),
            observe_origin_body,
        ));
    let origin_task = ServerTask(Some(tokio::spawn(async move {
        let _ = axum::serve(origin_listener, origin_router).await;
    })));
    let ingress = Arc::new(LanIngressTelemetry::new(
        &session,
        Arc::clone(&fixture_report),
        Arc::clone(&origin.phase),
        Arc::clone(&origin.epoch),
    ));
    let media_router = crate::media_router(state.clone())
        .layer(from_fn_with_state(
            Arc::clone(&transfer_ledger),
            observe_player_ingress,
        ))
        .layer(from_fn_with_state(ingress, record_lan_ingress));
    let media_task = ServerTask(Some(tokio::spawn(async move {
        let _ = axum::serve(media_listener, media_router).await;
    })));

    let master_url = format!("{master_url}?playback_reader={reader_id}");
    ensure_loopback_url(&master_url)?;
    let client = local_only_http_client()?;
    let playlist_preflight = validate_reader_bound_master_playlist(
        &client,
        &master_url,
        &reader_id,
        NativeCase::ProgressiveAbr,
        PlaylistExpectations {
            video_fragments: fixture.high_video.record.fragment_count,
            audio_fragments: fixture.audio.record.fragment_count,
            video_peak_bandwidth: fixture.high_video.record.peak_bandwidth_bits_per_second,
            audio_peak_bandwidth: fixture.audio.record.peak_bandwidth_bits_per_second,
            video_target_duration: fixture.high_video.record.target_duration_seconds,
            audio_target_duration: fixture.audio.record.target_duration_seconds,
        },
        true,
    )
    .await?;
    if let Ok(mut report) = fixture_report.lock() {
        report.hls_playlist_preflight = Some(playlist_preflight);
        report.fault_timeline = FaultTimeline {
            enabled: true,
            bad_high_status_code: Some(StatusCode::SERVICE_UNAVAILABLE.as_u16()),
            ..FaultTimeline::default()
        };
    }
    origin.reset_probe_epoch();

    let control_directory = temporary_root.join("native-control");
    fs::create_dir(&control_directory)
        .map_err(|_| "private native control directory could not be created".to_owned())?;
    fs::set_permissions(&control_directory, fs::Permissions::from_mode(0o700))
        .map_err(|_| "native control directory permissions could not be restricted".to_owned())?;
    let ready_marker = temporary_root.join("decoder-ready.json");
    let recovery_marker = temporary_root.join("recovery-observation.trigger");
    let phase_result = run_arm_phase(
        &state,
        &session,
        &reader_id,
        &task_id,
        generation,
        &master_url,
        &probe_path,
        &control_directory,
        &ready_marker,
        &recovery_marker,
        &origin,
        &fixture_report,
    )
    .await;
    media_task.stop().await;
    origin_task.stop().await;
    let (probe, challenge_count, control_error, plans, observation, binding) = phase_result?;
    build_arm_report(
        mode,
        &binding,
        &probe,
        challenge_count,
        control_error.as_deref(),
        &fixture_report,
        &transfer_ledger,
        &observation,
        &plans,
    )
}

// This adapter binds one arm's independent fixture, probe, and control timeline.
#[allow(clippy::too_many_arguments)]
async fn run_arm_phase(
    state: &AppState,
    session: &HlsPlaybackSession,
    reader_id: &str,
    session_id: &str,
    generation: u64,
    master_url: &str,
    probe_path: &Path,
    control_directory: &Path,
    ready_marker: &Path,
    recovery_marker: &Path,
    origin: &Arc<OriginState>,
    report: &ReportStore,
) -> Result<
    (
        Value,
        u64,
        Option<String>,
        PlanLedger,
        HighPreparationObservation,
        ReaderBinding,
    ),
    String,
> {
    let probe_path = probe_path.to_path_buf();
    let master_url = master_url.to_owned();
    let reader_for_probe = reader_id.to_owned();
    let ready_for_probe = ready_marker.to_path_buf();
    let control_for_probe = control_directory.to_path_buf();
    let mut probe = AbortTask(Some(tokio::spawn(async move {
        run_native_high_preparation_probe(
            probe_path,
            master_url,
            &reader_for_probe,
            &ready_for_probe,
            &control_for_probe,
        )
        .await
    })));

    if let Err(error) =
        run_abr_fault_timeline(origin, &mut probe, ready_marker, report, recovery_marker).await
    {
        probe.abort();
        return Err(format!("ProgressiveAbr fault timeline failed: {error}"));
    }

    let ticket = match open_generated_context(state, session_id, generation, reader_id).await {
        Ok(ticket) => ticket,
        Err(error) => {
            probe.abort();
            return Err(error);
        }
    };
    let binding = ReaderBinding {
        reader_id: reader_id.to_owned(),
        context_id: ticket.context_id.clone(),
        session_id: session_id.to_owned(),
        session_generation: generation,
    };
    let mut ticket = ticket;
    let mut plans = PlanLedger::default();
    let mut observation = state.hls_high_preparation.observation_for_tests();
    let mut challenge_count = 0_u64;
    let mut control_error = None;

    while !probe.is_finished() {
        match submit_challenged_update(
            state,
            session,
            &binding,
            control_directory,
            &probe,
            &ticket,
            &mut plans,
            true,
        )
        .await
        {
            Ok(Some((next, current_observation))) => {
                ticket = next;
                observation = current_observation;
                challenge_count = challenge_count.saturating_add(1);
            }
            Ok(None) => break,
            Err(error) => {
                control_error = Some(error);
                break;
            }
        }
        tokio::time::sleep(HIGH_PREPARATION_CONTROL_INTERVAL).await;
    }
    while !probe.is_finished() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let process_result = timeout(Duration::from_secs(10), probe.join()).await;
    let close_result = close_generated_context(state, &binding.context_id).await;
    let process = match process_result {
        Ok(Ok(Ok(process))) => Ok(process),
        Ok(Ok(Err(error))) => Err(error),
        Ok(Err(error)) => Err(error),
        Err(_) => Err("native probe result exceeded its bounded join grace".to_owned()),
    };
    close_result?;
    let probe = parse_probe_result(process?, report)?;
    Ok((
        probe,
        challenge_count,
        control_error,
        plans,
        observation,
        binding,
    ))
}

pub(super) async fn run_native_high_preparation_off_on_comparison() {
    let artifact_directory = std::env::var_os("LAN_MEDIA_HIGH_PREPARATION_ARTIFACTS")
        .map(PathBuf::from)
        .expect("the opt-in wrapper must provide a private reports directory");
    let metadata = fs::symlink_metadata(&artifact_directory)
        .expect("the opt-in reports directory must already exist");
    assert!(metadata.is_dir() && !metadata.file_type().is_symlink());

    let off = run_arm(ArmMode::Off)
        .await
        .expect("OFF arm must complete its muted automatic native phase");
    assert_eq!(off.arm, "off");
    assert!(!off.eligible);
    write_arm_report(&artifact_directory.join("off.json"), &off)
        .expect("bounded OFF report should be written");

    let on = run_arm(ArmMode::On)
        .await
        .expect("ON arm must complete or explicitly report semantic ineligibility");
    assert_eq!(on.arm, "on");
    write_arm_report(&artifact_directory.join("on.json"), &on)
        .expect("bounded ON report should be written");
}

fn write_arm_report(path: &Path, report: &ArmReport) -> Result<(), String> {
    let bytes = serde_json::to_vec(report)
        .map_err(|_| "high-preparation arm report could not be encoded".to_owned())?;
    if bytes.len() > ARM_REPORT_LIMIT {
        return Err("high-preparation arm report exceeded its 64 KiB limit".to_owned());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "new high-preparation arm report could not be created".to_owned())?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| "high-preparation arm report could not be persisted".to_owned())
}

#[cfg(test)]
mod pure_helper_tests {
    use super::*;

    const TEST_READER: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

    fn canonical_media_playlist() -> &'static str {
        concat!(
            "#EXTM3U\n",
            "#EXT-X-TARGETDURATION:2\n",
            "#EXT-X-MAP:URI=\"video.m4s\",BYTERANGE=\"100@0\"\n",
            "#EXTINF:2.000,\n#EXT-X-BYTERANGE:200@100\nvideo.m4s?segment=100-200\n",
            "#EXTINF:2.000,\n#EXT-X-BYTERANGE:200@300\nvideo.m4s?segment=300-200\n",
            "#EXT-X-ENDLIST\n",
        )
    }

    fn bound_media_playlist() -> String {
        canonical_media_playlist()
            .replace(
                "URI=\"video.m4s\"",
                &format!("URI=\"video.m4s?playback_reader={TEST_READER}\""),
            )
            .replace(
                "video.m4s?segment=100-200",
                &format!("video.m4s?segment=100-200&playback_reader={TEST_READER}"),
            )
            .replace(
                "video.m4s?segment=300-200",
                &format!("video.m4s?segment=300-200&playback_reader={TEST_READER}"),
            )
    }

    #[test]
    fn lower_bound_counts_only_known_missing_to_readable_ranges() {
        assert_eq!(
            readable_transition_lower_bound(ReadState::Missing, ReadState::Readable),
            1
        );
        assert_eq!(
            readable_transition_lower_bound(ReadState::Readable, ReadState::Readable),
            0
        );
        assert_eq!(
            readable_transition_lower_bound(ReadState::Unknown, ReadState::Readable),
            0
        );
        assert_eq!(
            readable_transition_lower_bound(ReadState::Missing, ReadState::Unknown),
            0
        );
    }

    #[test]
    fn reader_bound_playlists_match_canonical_content_and_preserve_ranges() {
        let canonical_url = url::Url::parse("http://127.0.0.1/hls/session/video.m3u8").unwrap();
        let bound_url = url::Url::parse(&format!(
            "http://127.0.0.1/hls/session/video.m3u8?playback_reader={TEST_READER}"
        ))
        .unwrap();
        let bound = bound_media_playlist();

        let legacy_error = validate_media_playlist_body(&bound_url, &bound, 2).unwrap_err();
        assert_eq!(
            legacy_error,
            "selected production media playlist omitted a valid initialization map"
        );
        validate_media_playlist_body(&canonical_url, canonical_media_playlist(), 2).unwrap();
        compare_reader_bound_playlist(
            &canonical_url,
            &bound_url,
            canonical_media_playlist(),
            &bound,
            TEST_READER,
            true,
        )
        .unwrap();
    }

    #[test]
    fn reader_bound_playlist_rejects_invalid_binding_and_reference_changes() {
        let canonical_url = url::Url::parse("http://127.0.0.1/hls/session/video.m3u8").unwrap();
        let bound_url = url::Url::parse(&format!(
            "http://127.0.0.1/hls/session/video.m3u8?playback_reader={TEST_READER}"
        ))
        .unwrap();
        let bound = bound_media_playlist();
        let rejects = |candidate: &str| {
            compare_reader_bound_playlist(
                &canonical_url,
                &bound_url,
                canonical_media_playlist(),
                candidate,
                TEST_READER,
                true,
            )
            .is_err()
        };

        assert!(rejects(
            &bound.replace(&format!("?playback_reader={TEST_READER}",), "",)
        ));
        assert!(rejects(
            &bound.replace(TEST_READER, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",)
        ));
        assert!(rejects(&bound.replace(
            &format!("playback_reader={TEST_READER}"),
            &format!("playback_reader={TEST_READER}&playback_reader={TEST_READER}"),
        )));
        assert!(rejects(&bound.replace(
            &format!("playback_reader={TEST_READER}"),
            &format!("playback_reader={TEST_READER}&extra=1"),
        )));
        assert!(rejects(
            &bound.replace("segment=100-200", "segment=100-199")
        ));
        assert!(rejects(
            &bound.replace("BYTERANGE=\"100@0\"", "BYTERANGE=\"101@0\"")
        ));
        assert!(rejects(&bound.replace(
            &format!("video.m4s?playback_reader={TEST_READER}"),
            &format!("http://example.test/video.m4s?playback_reader={TEST_READER}"),
        )));
        assert!(rejects(&bound.replace(
            &format!("video.m4s?playback_reader={TEST_READER}"),
            &format!("video.m4s?playback_reader={TEST_READER}#fragment"),
        )));
        assert!(rejects(&bound.replace(
            &format!("video.m4s?playback_reader={TEST_READER}"),
            &format!("http://user@127.0.0.1/video.m4s?playback_reader={TEST_READER}"),
        )));
    }

    #[test]
    fn reader_bound_master_requires_exact_uri_equivalence() {
        let canonical_url = url::Url::parse("http://127.0.0.1/hls/session/master.m3u8").unwrap();
        let bound_url = url::Url::parse(&format!(
            "http://127.0.0.1/hls/session/master.m3u8?playback_reader={TEST_READER}"
        ))
        .unwrap();
        let canonical = concat!(
            "#EXTM3U\n",
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",URI=\"audio.m3u8\"\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=200000,AUDIO=\"audio\"\n",
            "video.m3u8\n",
        );
        let bound = format!(
            "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",URI=\"audio.m3u8?playback_reader={TEST_READER}\"\n#EXT-X-STREAM-INF:BANDWIDTH=200000,AUDIO=\"audio\"\nvideo.m3u8?playback_reader={TEST_READER}\n"
        );
        compare_reader_bound_playlist(
            &canonical_url,
            &bound_url,
            canonical,
            &bound,
            TEST_READER,
            false,
        )
        .unwrap();
        assert!(
            compare_reader_bound_playlist(
                &canonical_url,
                &bound_url,
                canonical,
                &bound.replace("video.m3u8?playback_reader", "other.m3u8?playback_reader"),
                TEST_READER,
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn maximum_bounded_arm_report_stays_within_serialization_limit() {
        assert_eq!(OPT_IN_FIXTURE_GENERATION_TIMEOUT.as_secs(), 240);
        let span = |index: usize| SelectedSpanReport {
            kind: if index.is_multiple_of(2) {
                "Video"
            } else {
                "Audio"
            },
            resource_id: "r".repeat(96),
            variant_id: "v".repeat(96),
            context_id: "c".repeat(96),
            session_id: "s".repeat(96),
            session_generation: 7,
            logical_window_start_seconds: 10.0,
            logical_window_end_seconds: 30.0,
            byte_range_start: index as u64 * 128,
            byte_range_end_exclusive: index as u64 * 128 + 64,
            before_read_state: "missing",
            after_read_state: "readable",
            newly_readable_bytes_lower_bound: 1,
            readable: true,
        };
        let selected_spans = (0..4).map(span).collect::<Vec<_>>();
        let plans = vec![PlanReport {
            plan_id: 0,
            context_id: "c".repeat(96),
            reader_id: "d".repeat(96),
            session_id: "s".repeat(96),
            session_generation: 7,
            target_variant_id: Some("v".repeat(96)),
            window_start_seconds: 10.0,
            window_end_seconds: 30.0,
            initial_window_readable: Some(false),
            outcome: "Completed",
            window_readable: true,
            admitted_sample_monotonic_seconds: 100.0,
            terminal_observed_monotonic_seconds: Some(110.0),
            selected_spans: selected_spans.clone(),
        }];
        let transfer = |index: usize| RangeTransferReport {
            context_id: "c".repeat(96),
            reader_id: "d".repeat(96),
            session_id: "s".repeat(96),
            session_generation: 7,
            variant_id: "v".repeat(96),
            resource_id: "r".repeat(96),
            logical_window_start_seconds: 10.0,
            logical_window_end_seconds: 30.0,
            range_start: index as u64 * 128,
            range_end_exclusive: index as u64 * 128 + 64,
            status_code: 206,
            body_complete: Some(true),
            bytes_observed: Some(64),
            request_started_monotonic_seconds: 101.0,
            completed_monotonic_seconds: Some(102.0),
        };
        let origin_range_transfers = (0..4).map(transfer).collect::<Vec<_>>();
        let player_range_transfers = (0..4).map(transfer).collect::<Vec<_>>();
        let report = ArmReport {
            schema_version: 1,
            arm: "on",
            status: "inconclusive",
            eligible: false,
            transport_boundary: ARM_TRANSPORT_BOUNDARY,
            fixture_generation_timeout_seconds: OPT_IN_FIXTURE_GENERATION_TIMEOUT.as_secs(),
            fixture_is_fresh: true,
            cache_is_fresh: true,
            reader: ReaderReport {
                id: "a".repeat(96),
                context_id: "c".repeat(96),
                session_id: "s".repeat(96),
                session_generation: 7,
            },
            native: NativeReport {
                mode: "continuous_native_control_v1",
                requested_duration_seconds: 600,
                deadline_seconds: 720,
                phase_completed: true,
                muted_verified: true,
                system_audio_mute_verified: true,
                automatic_waiting_enabled: true,
                high_low_high_decoded: false,
                decode_censored: false,
                seek_count: 0,
                rate_change_count: 0,
                item_replacement_count: 0,
                buffer_cap_set: false,
                challenge_sample_count: CONTROL_SAMPLE_LIMIT as u64,
                challenge_samples_truncated: false,
            },
            plans,
            origin_range_transfers,
            player_range_transfers,
            plan_history_retention: PlanHistoryRetentionReport {
                observed_matching_candidates: TRACKED_PLAN_LIMIT as u64,
                internally_retained_records: TRACKED_PLAN_LIMIT,
                internal_record_limit: TRACKED_PLAN_LIMIT,
                serialized_plan_records: 1,
                serialization_record_limit: 1,
                producer_history_limit: TRACKED_PLAN_LIMIT,
                truncated: false,
            },
            transfer_event_retention: TransferEventRetentionReport {
                origin_recovery_range_events_observed: CAUSAL_EVENT_LIMIT as u64,
                origin_witnesses_retained: 4,
                origin_ring_limit: CAUSAL_EVENT_LIMIT,
                origin_ring_truncated: true,
                player_recovery_range_events_observed: CAUSAL_EVENT_LIMIT as u64,
                player_witnesses_retained: 4,
                player_ring_limit: CAUSAL_EVENT_LIMIT,
                player_ring_truncated: true,
            },
            logical_range_counts: LogicalRangeCounts {
                requested_resource_ranges: 64,
                readable_resource_ranges: 32,
                deferred_resource_ranges: 32,
            },
            attribution: AttributionReport {
                exclusive_attribution: "not_proven",
                causal_faster_selection_claim: false,
            },
            first_recovery_high_origin_request: Some(RecoveryRangeRequest {
                elapsed_ms: 1_000,
                monotonic_seconds: Some(100.0),
                range_start: 0,
                range_length: 64,
                status_code: 206,
            }),
            first_recovery_high_completed_source_body: Some(FirstCompletedSourceBody {
                resource_id: "r".repeat(96),
                range_start: 0,
                range_end_exclusive: 64,
                status_code: 206,
                body_complete: true,
                bytes_observed: 64,
                request_started_monotonic_seconds: 100.0,
                completed_monotonic_seconds: Some(101.0),
            }),
            first_recovery_high_decoded_monotonic_seconds: Some(102.0),
            recovery_preparation: Some(RecoveryPreparationReport {
                plan_id: 0,
                window_start_seconds: 10.0,
                window_end_seconds: 30.0,
                completed: true,
                initial_window_readable: Some(false),
                window_readable: true,
                initial_missing_to_final_readable: true,
                newly_readable_bytes_lower_bound: 4,
                newly_readable_semantics: "one_byte_lower_bound_per_exact_range_none_to_full_some_transition",
                completed_origin_range_overlaps: true,
                player_range_overlap_status: "yes",
                admitted_sample_monotonic_seconds: 100.0,
                terminal_observed_monotonic_seconds: Some(110.0),
                selected_spans,
            }),
            inconclusive_reason: Some("x".repeat(160)),
        };

        let encoded = serde_json::to_vec(&report).expect("bounded arm report serializes");
        let value: Value = serde_json::from_slice(&encoded).expect("bounded arm report parses");
        assert_eq!(
            value["fixture_generation_timeout_seconds"].as_u64(),
            Some(240)
        );
        assert!(
            encoded.len() <= ARM_REPORT_LIMIT,
            "maximum bounded report serialized to {} bytes",
            encoded.len()
        );
    }

    #[test]
    fn challenge_response_requires_current_ticket_and_fresh_native_time() {
        let ticket = HlsPlaybackContextTicket {
            context_id: "context".to_owned(),
            challenge: "challenge".to_owned(),
            session_id: "session".to_owned(),
            session_generation: 7,
            next_sequence: 3,
            validity_millis: 5_000,
            http_reader_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_owned(),
        };
        let sample = NativeSample {
            sample_request_id: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_owned(),
            context_id: "context".to_owned(),
            challenge: "challenge".to_owned(),
            reader_id: ticket.http_reader_id.clone(),
            session_generation: 7,
            sequence: 3,
            sample_monotonic_seconds: 11.0,
            playhead_seconds: Some(4.0),
            continuous_buffered_end_seconds: Some(30.0),
            playback_rate: Some(1.0),
            time_control_status: "playing".to_owned(),
        };
        assert!(challenge_response_is_fresh(
            &sample,
            &sample.sample_request_id,
            &ticket,
            10.0,
            11.5
        ));
        assert!(!challenge_response_is_fresh(
            &sample,
            &sample.sample_request_id,
            &ticket,
            11.0,
            11.5
        ));
        assert!(!challenge_response_is_fresh(
            &sample,
            "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
            &ticket,
            10.0,
            11.5
        ));
    }
}
