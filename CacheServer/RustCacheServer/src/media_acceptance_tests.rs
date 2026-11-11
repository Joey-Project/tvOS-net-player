use std::{
    collections::HashMap,
    env,
    fs::File,
    io::{Read, Seek, SeekFrom},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::{Body, HttpBody as _},
    extract::{Path as AxumPath, Request, State},
    http::{
        HeaderMap, Response, StatusCode,
        header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE},
    },
    middleware::{Next, from_fn_with_state},
    routing::get,
};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt},
    net::TcpListener,
    process::{Child, Command},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::io::ReaderStream;

use crate::{
    AppState,
    bbdown_adapter::{
        BilibiliMediaCacheKey, BilibiliMediaRequest, BilibiliMediaRequestKind,
        BilibiliPlaybackAbrGroup, BilibiliPlaybackAbrGroupKind, BilibiliPlaybackAbrLevel,
        BilibiliPlaybackAbrMetadata, BilibiliPlaybackVariant, BilibiliPlaybackVariantKind,
    },
    bilibili_playback::{BilibiliPlaybackPlanner, BilibiliPlaybackPlanningFuture},
    bilibili_worker::BilibiliDownloadError,
    config::CacheServerOptions,
    generated::tvos_net_player::v1::{BilibiliPlaybackSession, PlaybackProtocol, PlaybackSource},
    hls::{HlsPlaybackSession, mp4_initialization_length},
    hls_cache::{HlsCacheFillControl, HlsCacheStore, HlsTranscodingExecutionConfig},
    hls_range_cache::HlsRangePriority,
    mp4_segments::{
        MAX_SIDX_TRAILER_BYTES, Mp4SegmentRange,
        hls_peak_bandwidth_for_ranges as peak_bandwidth_bits_per_second,
        hls_target_duration_for_ranges as fixture_target_duration_seconds, mp4_fragment_ranges,
        mp4_sidx_index, mp4_sidx_segment_ranges,
    },
    playback_policy::{PlaybackPolicy, TranscodingPreference, WeakNetworkPreference},
    transcoding::HlsTranscodingPlan,
};

const ABR_CASE_FULL_NAME: &str =
    "media_acceptance_tests::native_progressive_adaptation_and_recovery";
const EXTENDED_DIAGNOSTIC_CASE_FULL_NAME: &str =
    "media_acceptance_tests::native_diagnostic_extended_adaptation_and_recovery";
const PREWARM_CONTROL_CASE_FULL_NAME: &str =
    "media_prewarm_comparison_tests::native_single_player_prewarm_control";
const PREWARM_TREATMENT_CASE_FULL_NAME: &str =
    "media_prewarm_comparison_tests::native_single_player_prewarm_treatment";
const BUFFERED_CASE_FULL_NAME: &str =
    "media_acceptance_tests::native_progressive_buffered_fallback_keeps_playing";
const TRANSCODE_CASE_FULL_NAME: &str = "media_acceptance_tests::native_transcoded_output_decodes";
const MAX_REPORT_BYTES: usize = 64 * 1024;
const MAX_PROBE_JSON_BYTES: usize = 64 * 1024;
const MAX_CHILD_STDERR_BYTES: usize = 128 * 1024;
const MAX_FIXTURE_BYTES: u64 = 96 * 1024 * 1024;
const MAX_SOURCE_FIXTURE_BYTES: u64 = 60 * 1024 * 1024;
const MAX_SINGLE_HIGH_FIXTURE_BYTES: u64 = 40 * 1024 * 1024;
const MAX_SINGLE_LOW_FIXTURE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SINGLE_AUDIO_FIXTURE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_LONG_SOURCE_FIXTURE_BYTES: u64 = 1280 * 1024 * 1024;
const MAX_LONG_HIGH_FIXTURE_BYTES: u64 = 960 * 1024 * 1024;
const MAX_LONG_LOW_FIXTURE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_LONG_AUDIO_FIXTURE_BYTES: u64 = 64 * 1024 * 1024;
const BUFFERED_MEDIA_DURATION_SECONDS: u32 = 160;
const LONG_RECOVERY_MEDIA_DURATION_SECONDS: u32 = 3600;
const TRANSCODE_MEDIA_DURATION_SECONDS: u32 = 45;
const ABR_PROBE_DURATION_SECONDS: u64 = 130;
const ABR_PROBE_DEADLINE_SECONDS: u64 = 175;
const LONG_RECOVERY_PROBE_DURATION_SECONDS: u64 = 600;
const LONG_RECOVERY_PROBE_DEADLINE_SECONDS: u64 = 720;
const MINIMUM_STEADY_PLAY_SECONDS: u64 = 110;
const TRANSCODE_PROBE_DURATION_SECONDS: u64 = 30;
const ABR_CASE_TIMEOUT: Duration = Duration::from_secs(225);
const LONG_RECOVERY_CASE_TIMEOUT: Duration = Duration::from_secs(840);
const TRANSCODE_CASE_TIMEOUT: Duration = Duration::from_secs(225);
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(100);
const LONG_RECOVERY_FFMPEG_TIMEOUT: Duration = Duration::from_secs(120);
const LONG_RECOVERY_FFPROBE_TIMEOUT: Duration = Duration::from_secs(30);
const DECODER_READY_TIMEOUT: Duration = Duration::from_secs(45);
const BASELINE_WINDOW: Duration = Duration::from_secs(6);
const BAD_HIGH_WINDOW: Duration = Duration::from_secs(45);
const PROBE_EXIT_GRACE: Duration = Duration::from_secs(8);

const PHASE_BASELINE: u8 = 0;
const PHASE_BAD_HIGH: u8 = 1;
const PHASE_RECOVERY: u8 = 2;
const ABR_BASELINE_RATE_BITS_PER_SECOND: u64 = 4_000_000;
const ABR_BAD_RATE_BITS_PER_SECOND: u64 = 1_000_000;
const ABR_RECOVERY_RATE_BITS_PER_SECOND: u64 = 8_000_000;
const H264_BASELINE_LEVEL_3_CODEC: &str = "avc1.42c01e";

#[derive(Clone, Copy)]
enum NativeCase {
    BufferedContinuity,
    ProgressiveAbr,
    ExtendedDiagnostic,
    SinglePlayerPrewarmControl,
    SinglePlayerPrewarmTreatment,
    Transcoding,
}

impl NativeCase {
    fn full_name(self) -> &'static str {
        match self {
            Self::BufferedContinuity => BUFFERED_CASE_FULL_NAME,
            Self::ProgressiveAbr => ABR_CASE_FULL_NAME,
            Self::ExtendedDiagnostic => EXTENDED_DIAGNOSTIC_CASE_FULL_NAME,
            Self::SinglePlayerPrewarmControl => PREWARM_CONTROL_CASE_FULL_NAME,
            Self::SinglePlayerPrewarmTreatment => PREWARM_TREATMENT_CASE_FULL_NAME,
            Self::Transcoding => TRANSCODE_CASE_FULL_NAME,
        }
    }

    fn report_name(self) -> &'static str {
        match self {
            Self::BufferedContinuity => "buffered.json",
            Self::ProgressiveAbr => "abr.json",
            Self::ExtendedDiagnostic => "abr-extended-diagnostic.json",
            Self::SinglePlayerPrewarmControl => "prewarm-control.json",
            Self::SinglePlayerPrewarmTreatment => "prewarm-treatment.json",
            Self::Transcoding => "transcoding.json",
        }
    }

    fn deadline(self) -> Duration {
        match self {
            Self::BufferedContinuity => ABR_CASE_TIMEOUT,
            Self::ProgressiveAbr
            | Self::ExtendedDiagnostic
            | Self::SinglePlayerPrewarmControl
            | Self::SinglePlayerPrewarmTreatment => LONG_RECOVERY_CASE_TIMEOUT,
            Self::Transcoding => TRANSCODE_CASE_TIMEOUT,
        }
    }

    fn scenario(self) -> &'static str {
        match self {
            Self::BufferedContinuity => "buffered_continuity",
            Self::ProgressiveAbr => "unbuffered_recovery",
            Self::ExtendedDiagnostic => "extended_recovery_diagnostic",
            Self::SinglePlayerPrewarmControl => "single_player_prewarm_control",
            Self::SinglePlayerPrewarmTreatment => "single_player_prewarm_treatment",
            Self::Transcoding => "transcoded_output",
        }
    }

    fn source_duration_seconds(self) -> u32 {
        match self {
            Self::BufferedContinuity => BUFFERED_MEDIA_DURATION_SECONDS,
            Self::ProgressiveAbr
            | Self::ExtendedDiagnostic
            | Self::SinglePlayerPrewarmControl
            | Self::SinglePlayerPrewarmTreatment => LONG_RECOVERY_MEDIA_DURATION_SECONDS,
            Self::Transcoding => TRANSCODE_MEDIA_DURATION_SECONDS,
        }
    }

    fn uses_long_profile(self) -> bool {
        matches!(
            self,
            Self::ProgressiveAbr
                | Self::ExtendedDiagnostic
                | Self::SinglePlayerPrewarmControl
                | Self::SinglePlayerPrewarmTreatment
        )
    }

    fn uses_progressive_abr(self) -> bool {
        matches!(
            self,
            Self::BufferedContinuity
                | Self::ProgressiveAbr
                | Self::ExtendedDiagnostic
                | Self::SinglePlayerPrewarmControl
                | Self::SinglePlayerPrewarmTreatment
        )
    }

    fn probe_duration_seconds(self) -> u64 {
        match self {
            Self::BufferedContinuity => ABR_PROBE_DURATION_SECONDS,
            Self::ProgressiveAbr
            | Self::ExtendedDiagnostic
            | Self::SinglePlayerPrewarmControl
            | Self::SinglePlayerPrewarmTreatment => LONG_RECOVERY_PROBE_DURATION_SECONDS,
            Self::Transcoding => TRANSCODE_PROBE_DURATION_SECONDS,
        }
    }

    fn probe_deadline_seconds(self) -> u64 {
        match self {
            Self::BufferedContinuity => ABR_PROBE_DEADLINE_SECONDS,
            Self::ProgressiveAbr
            | Self::ExtendedDiagnostic
            | Self::SinglePlayerPrewarmControl
            | Self::SinglePlayerPrewarmTreatment => LONG_RECOVERY_PROBE_DEADLINE_SECONDS,
            Self::Transcoding => self.probe_duration_seconds() + 15,
        }
    }

    fn is_diagnostic(self) -> bool {
        matches!(
            self,
            Self::ExtendedDiagnostic
                | Self::SinglePlayerPrewarmControl
                | Self::SinglePlayerPrewarmTreatment
        )
    }

    fn is_prewarm_diagnostic(self) -> bool {
        matches!(
            self,
            Self::SinglePlayerPrewarmControl | Self::SinglePlayerPrewarmTreatment
        )
    }

    fn prewarm_mode(self) -> Option<crate::media_prewarm_comparison_tests::PrewarmMode> {
        match self {
            Self::SinglePlayerPrewarmControl => {
                Some(crate::media_prewarm_comparison_tests::PrewarmMode::Off)
            }
            Self::SinglePlayerPrewarmTreatment => {
                Some(crate::media_prewarm_comparison_tests::PrewarmMode::On)
            }
            _ => None,
        }
    }

    fn cache_limit_bytes(self) -> u64 {
        if self.uses_long_profile() {
            2 * 1024 * 1024 * 1024
        } else {
            MAX_FIXTURE_BYTES
        }
    }
}

#[derive(Clone, Serialize)]
struct AcceptanceReport {
    schema_version: u32,
    status: String,
    case_full_name: String,
    scenario: String,
    source_duration_seconds: u32,
    diagnostic_only: bool,
    probe: Option<Value>,
    fixture_media: Vec<FixtureRecord>,
    hls_playlist_preflight: Option<HlsPlaylistPreflight>,
    requests: RequestSummary,
    lan_ingress: LanIngressSummary,
    lan_body_outcomes: LanBodyOutcomeSummary,
    fault_timeline: FaultTimeline,
    transcoding_metadata: Option<TranscodingMetadata>,
    prewarm_comparison: Option<crate::media_prewarm_comparison_tests::PrewarmComparisonReport>,
    error: Option<String>,
}

impl AcceptanceReport {
    fn new(case: NativeCase) -> Self {
        Self {
            schema_version: 1,
            status: "failed".to_owned(),
            case_full_name: case.full_name().to_owned(),
            scenario: case.scenario().to_owned(),
            source_duration_seconds: case.source_duration_seconds(),
            diagnostic_only: case.is_diagnostic(),
            probe: None,
            fixture_media: Vec::new(),
            hls_playlist_preflight: None,
            requests: RequestSummary::default(),
            lan_ingress: LanIngressSummary::default(),
            lan_body_outcomes: LanBodyOutcomeSummary::default(),
            fault_timeline: FaultTimeline::default(),
            transcoding_metadata: None,
            prewarm_comparison: None,
            error: None,
        }
    }
}

type ReportStore = Arc<Mutex<AcceptanceReport>>;

#[derive(Clone, Default, Serialize)]
struct RequestSummary {
    total: u64,
    count_by_representation: [u64; 3],
    successful_by_representation: [u64; 3],
    media_payload_by_phase_and_representation: [[u64; 3]; 3],
    count_by_phase_and_representation: [[u64; 3]; 3],
    // Request-start observation at origin accounting, not response delivery completion.
    last_request_elapsed_ms_by_phase_and_representation: [[Option<u64>; 3]; 3],
    high_fault_status_count: u64,
    first_recovery_high_range_request: Option<RecoveryHighRangeRequest>,
    first_recovery_high_media_request: Option<RecoveryHighRangeRequest>,
    events: Vec<RequestEvent>,
}

#[derive(Clone, Serialize)]
struct RequestEvent {
    elapsed_ms: u64,
    representation_code: u8,
    phase_code: u8,
    range_start: u64,
    range_length: u64,
    status_code: u16,
}

#[derive(Clone, Serialize)]
struct RecoveryHighRangeRequest {
    elapsed_ms: u64,
    monotonic_seconds: Option<f64>,
    range_start: u64,
    range_length: u64,
    status_code: u16,
    media_window: Option<MediaWindow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
struct MediaWindow {
    start_seconds: f64,
    end_seconds: f64,
    first_fragment_index: u64,
    last_fragment_index: u64,
}

#[derive(Clone, Default, Serialize)]
struct LanIngressSummary {
    count_by_phase_and_representation: [[u64; 3]; 3],
    // Observed when the LAN response headers are available, not at request start or body completion.
    last_header_elapsed_ms_by_phase_and_representation: [[Option<u64>; 3]; 3],
    segment_query_present_by_phase_and_representation: [[u64; 3]; 3],
    header_samples: Vec<LanIngressHeaderSample>,
}

#[derive(Clone, Serialize)]
struct LanIngressHeaderSample {
    elapsed_ms: u64,
    representation_code: u8,
    phase_code: u8,
    range_start: Option<u64>,
    range_end: Option<u64>,
    header_status_code: u16,
    segment_query_present: bool,
}

#[derive(Clone, Default, Serialize)]
struct LanBodyOutcomeSummary {
    complete_by_representation: [u64; 3],
    error_by_representation: [u64; 3],
    dropped_by_representation: [u64; 3],
    events: Vec<LanBodyOutcomeEvent>,
}

#[derive(Clone, Serialize)]
struct LanBodyOutcomeEvent {
    header_elapsed_ms: u64,
    header_phase_code: u8,
    outcome_elapsed_ms: u64,
    representation_code: u8,
    outcome: &'static str,
    bytes_observed: u64,
}

#[derive(Clone, Copy)]
struct LanBodyOutcomeContext {
    representation_code: u8,
    header_phase_code: u8,
    header_elapsed_ms: u64,
}

struct LanIngressTelemetry {
    session_id: String,
    resource_representations: HashMap<String, u8>,
    phase: Arc<AtomicU8>,
    epoch: Arc<Mutex<Instant>>,
    report: ReportStore,
}

type IngressClassification = (u8, bool, Option<(u64, u64)>);

#[derive(Clone, Default, Serialize)]
struct FaultTimeline {
    enabled: bool,
    baseline_start_elapsed_ms: Option<u64>,
    decoder_ready_monotonic_seconds: Option<f64>,
    baseline_start_monotonic_seconds: Option<f64>,
    bad_high_start_elapsed_ms: Option<u64>,
    bad_high_end_elapsed_ms: Option<u64>,
    recovery_start_elapsed_ms: Option<u64>,
    bad_high_before_monotonic_seconds: Option<f64>,
    bad_high_after_monotonic_seconds: Option<f64>,
    recovery_before_monotonic_seconds: Option<f64>,
    recovery_after_monotonic_seconds: Option<f64>,
    bad_high_status_code: Option<u16>,
}

#[derive(Clone, Serialize)]
struct FixtureRecord {
    representation_code: u8,
    width: Option<u32>,
    height: Option<u32>,
    file_bytes: u64,
    initialization_bytes: u64,
    fragment_count: u64,
    fragment_duration_ms: u64,
    target_duration_seconds: u64,
    peak_bandwidth_bits_per_second: u64,
    legacy_single_fragment_peak_bandwidth_bits_per_second: u64,
    global_sidx_present: bool,
    progressive_index_count: Option<u64>,
    video_codec_code: u8,
    video_profile_code: u8,
    video_level: Option<i32>,
    audio_codec_code: u8,
}

#[derive(Clone, Serialize)]
struct HlsPlaylistPreflight {
    bandwidth_semantics: &'static str,
    high_video_fragment_count: u64,
    high_video_target_duration_seconds: u64,
    audio_fragment_count: u64,
    audio_target_duration_seconds: u64,
    high_video_peak_bandwidth_bits_per_second: u64,
    audio_peak_bandwidth_bits_per_second: u64,
    high_variant_peak_bandwidth_bits_per_second: u64,
    advertised_high_bandwidth_bits_per_second: u64,
}

#[derive(Clone, Serialize)]
struct TranscodingMetadata {
    executed: bool,
    output_file_bytes: u64,
    output_initialization_bytes: u64,
    output_fragment_count: u64,
    output_fragment_duration_ms: u64,
    output_peak_bandwidth_bits_per_second: u64,
    output_target_duration_seconds: u64,
    published_variant_bandwidth_bits_per_second: u64,
    output_global_sidx_present: bool,
    output_width: u32,
    output_height: u32,
    output_video_codec_code: u8,
    output_audio_codec_code: u8,
    output_audio_track_count: u64,
}

struct MediaFixture {
    high_video: InspectedMedia,
    low_video: Option<InspectedMedia>,
    audio: InspectedMedia,
}

struct InspectedMedia {
    path: PathBuf,
    record: FixtureRecord,
    fragments: Vec<Mp4SegmentRange>,
}

struct FixtureExpectation {
    representation_code: u8,
    dimensions: Option<(u32, u32)>,
    video_codec_code: u8,
    audio_codec_code: u8,
    video_profile: Option<(&'static str, i32)>,
}

struct PlaylistExpectations {
    video_fragments: u64,
    audio_fragments: u64,
    video_peak_bandwidth: u64,
    audio_peak_bandwidth: u64,
    video_target_duration: u64,
    audio_target_duration: u64,
}

struct ProbeMarkerPaths {
    ready: Option<PathBuf>,
    recovery_observation: Option<PathBuf>,
    recovery_snapshot: Option<PathBuf>,
}

#[derive(Deserialize)]
struct FfprobeOutput {
    streams: Vec<FfprobeStream>,
    format: Option<FfprobeFormat>,
}

#[derive(Deserialize)]
struct FfprobeStream {
    codec_type: String,
    codec_name: Option<String>,
    profile: Option<String>,
    level: Option<i32>,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Deserialize)]
struct FfprobeFormat {
    duration: Option<String>,
}

struct ProcessCapture {
    status_code: Option<i32>,
    success: bool,
    stdout: Vec<u8>,
    stdout_truncated: bool,
    stderr_tail: Vec<u8>,
    stderr_truncated: bool,
}

struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
}

struct OriginMedia {
    path: PathBuf,
    representation_code: u8,
    initialization_length: u64,
    fragments: Vec<Mp4SegmentRange>,
    content_type: &'static str,
}

struct OriginState {
    media: HashMap<String, OriginMedia>,
    case: NativeCase,
    phase: Arc<AtomicU8>,
    epoch: Arc<Mutex<Instant>>,
    report: ReportStore,
    pacers: [RepresentationPacer; 3],
}

impl LanIngressTelemetry {
    fn new(
        session: &HlsPlaybackSession,
        report: ReportStore,
        phase: Arc<AtomicU8>,
        epoch: Arc<Mutex<Instant>>,
    ) -> Self {
        let mut resource_representations = HashMap::new();
        for variant in std::iter::once(&session.variant).chain(session.alternate_variants.iter()) {
            let representation_code = if variant.height.is_some_and(|height| height <= 180) {
                2
            } else {
                1
            };
            resource_representations.insert(variant.video.id.clone(), representation_code);
            if let Some(audio) = &variant.audio {
                resource_representations.insert(audio.id.clone(), 3);
            }
        }
        Self {
            session_id: session.id.clone(),
            resource_representations,
            phase,
            epoch,
            report,
        }
    }

    fn classify(&self, request: &Request) -> Option<IngressClassification> {
        let mut path = request.uri().path().split('/');
        if !path.next()?.is_empty() || path.next()? != "hls" || path.next()? != self.session_id {
            return None;
        }
        if path.next()? != "segments" {
            return None;
        }
        let resource_id = path.next()?;
        if path.next().is_some() {
            return None;
        }
        let representation = *self.resource_representations.get(resource_id)?;
        let segment_query_present = request.uri().query().is_some_and(|query| {
            query
                .split('&')
                .any(|entry| entry.split('=').next() == Some("segment"))
        });
        Some((
            representation,
            segment_query_present,
            numeric_range_header(request.headers()),
        ))
    }

    fn record(
        &self,
        representation_code: u8,
        segment_query_present: bool,
        range: Option<(u64, u64)>,
        status_code: u16,
    ) -> LanBodyOutcomeContext {
        let phase_code = self.phase.load(Ordering::Acquire).min(PHASE_RECOVERY);
        let phase_index = usize::from(phase_code);
        let representation_index = usize::from(representation_code.saturating_sub(1).min(2));
        let elapsed_ms = self
            .epoch
            .lock()
            .map(|epoch| {
                Instant::now()
                    .saturating_duration_since(*epoch)
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64
            })
            .unwrap_or_default();
        let Ok(mut report) = self.report.lock() else {
            return LanBodyOutcomeContext {
                representation_code,
                header_phase_code: phase_code,
                header_elapsed_ms: elapsed_ms,
            };
        };
        let ingress = &mut report.lan_ingress;
        ingress.count_by_phase_and_representation[phase_index][representation_index] = ingress
            .count_by_phase_and_representation[phase_index][representation_index]
            .saturating_add(1);
        ingress.last_header_elapsed_ms_by_phase_and_representation[phase_index]
            [representation_index] = Some(elapsed_ms);
        if segment_query_present {
            ingress.segment_query_present_by_phase_and_representation[phase_index]
                [representation_index] = ingress.segment_query_present_by_phase_and_representation
                [phase_index][representation_index]
                .saturating_add(1);
        }
        if ingress
            .header_samples
            .iter()
            .filter(|sample| sample.phase_code == phase_code)
            .count()
            < 8
        {
            ingress.header_samples.push(LanIngressHeaderSample {
                elapsed_ms,
                representation_code,
                phase_code,
                range_start: range.map(|(start, _)| start),
                range_end: range.map(|(_, end)| end),
                header_status_code: status_code,
                segment_query_present,
            });
        }
        LanBodyOutcomeContext {
            representation_code,
            header_phase_code: phase_code,
            header_elapsed_ms: elapsed_ms,
        }
    }
}

fn numeric_range_header(headers: &HeaderMap) -> Option<(u64, u64)> {
    let value = headers.get(RANGE)?.to_str().ok()?.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    if end.contains('-') {
        return None;
    }
    Some((start.parse().ok()?, end.parse().ok()?))
}

async fn record_lan_ingress(
    State(telemetry): State<Arc<LanIngressTelemetry>>,
    request: Request,
    next: Next,
) -> Response<Body> {
    let classification = telemetry.classify(&request);
    let is_head = request.method() == axum::http::Method::HEAD;
    let mut response = next.run(request).await;
    if let Some((representation_code, segment_query_present, range)) = classification {
        let context = telemetry.record(
            representation_code,
            segment_query_present,
            range,
            response.status().as_u16(),
        );
        let exact_length = response.body().size_hint().exact();
        if is_head || (response.body().is_end_stream() && exact_length == Some(0)) {
            record_empty_lan_body_outcome(
                context,
                Arc::clone(&telemetry.report),
                Arc::clone(&telemetry.epoch),
            );
        } else {
            if let Some(length) = exact_length
                && !response.headers().contains_key(CONTENT_LENGTH)
                && let Ok(value) = length.to_string().parse()
            {
                response.headers_mut().insert(CONTENT_LENGTH, value);
            }
            response = response.map(|body| {
                Body::from_stream(LanBodyOutcomeStream::new(
                    body.into_data_stream(),
                    context,
                    Arc::clone(&telemetry.report),
                    Arc::clone(&telemetry.epoch),
                ))
            });
        }
    }
    response
}

fn record_empty_lan_body_outcome(
    context: LanBodyOutcomeContext,
    report: ReportStore,
    epoch: Arc<Mutex<Instant>>,
) {
    let mut outcome =
        LanBodyOutcomeStream::new(futures_util::stream::empty(), context, report, epoch);
    outcome.finish("complete");
}

struct LanBodyOutcomeStream {
    inner: Pin<Box<dyn Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send>>,
    context: LanBodyOutcomeContext,
    report: ReportStore,
    epoch: Arc<Mutex<Instant>>,
    bytes_observed: u64,
    finished: bool,
}

impl LanBodyOutcomeStream {
    fn new<S>(
        inner: S,
        context: LanBodyOutcomeContext,
        report: ReportStore,
        epoch: Arc<Mutex<Instant>>,
    ) -> Self
    where
        S: Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send + 'static,
    {
        Self {
            inner: Box::pin(inner),
            context,
            report,
            epoch,
            bytes_observed: 0,
            finished: false,
        }
    }

    fn finish(&mut self, outcome: &'static str) {
        if self.finished {
            return;
        }
        self.finished = true;
        let elapsed_ms = self
            .epoch
            .lock()
            .map(|epoch| {
                Instant::now()
                    .saturating_duration_since(*epoch)
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64
            })
            .unwrap_or_default();
        let Ok(mut report) = self.report.lock() else {
            return;
        };
        let representation = usize::from(self.context.representation_code.saturating_sub(1).min(2));
        let outcomes = &mut report.lan_body_outcomes;
        match outcome {
            "complete" => {
                outcomes.complete_by_representation[representation] =
                    outcomes.complete_by_representation[representation].saturating_add(1);
            }
            "error" => {
                outcomes.error_by_representation[representation] =
                    outcomes.error_by_representation[representation].saturating_add(1);
            }
            "dropped" => {
                outcomes.dropped_by_representation[representation] =
                    outcomes.dropped_by_representation[representation].saturating_add(1);
            }
            _ => return,
        }
        let event = LanBodyOutcomeEvent {
            header_elapsed_ms: self.context.header_elapsed_ms,
            header_phase_code: self.context.header_phase_code,
            outcome_elapsed_ms: elapsed_ms,
            representation_code: self.context.representation_code,
            outcome,
            bytes_observed: self.bytes_observed,
        };
        if outcomes.events.len() < 32 {
            outcomes.events.push(event);
        } else if outcome != "complete"
            && let Some(complete_index) = outcomes
                .events
                .iter()
                .position(|recorded| recorded.outcome == "complete")
        {
            outcomes.events[complete_index] = event;
        }
    }
}

impl Stream for LanBodyOutcomeStream {
    type Item = Result<axum::body::Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                self.bytes_observed = self.bytes_observed.saturating_add(bytes.len() as u64);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.finish("error");
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                self.finish("complete");
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for LanBodyOutcomeStream {
    fn drop(&mut self) {
        self.finish("dropped");
    }
}

#[derive(Clone, Default)]
struct RepresentationPacer {
    state: Arc<Mutex<PacingState>>,
}

#[derive(Default)]
struct PacingState {
    next_available_at: Option<Instant>,
}

struct ServerTask(Option<JoinHandle<()>>);

struct AbortTask<T>(Option<JoinHandle<T>>);

#[derive(Clone, Copy)]
struct ByteRange {
    start: u64,
    length: u64,
    partial: bool,
}

struct RejectNetworkPlanner;

impl BilibiliPlaybackPlanner for RejectNetworkPlanner {
    fn plan<'a>(
        &'a self,
        _request: crate::bilibili_playback::BilibiliPlaybackPlanningRequest,
    ) -> BilibiliPlaybackPlanningFuture<'a> {
        Box::pin(async {
            Err(BilibiliDownloadError::Failed(
                "native media acceptance never invokes an upstream planner".to_owned(),
            ))
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicitly selected macOS AVPlayer probe and local FFmpeg tools"]
async fn native_progressive_adaptation_and_recovery() {
    run_native_case(NativeCase::ProgressiveAbr).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic only; requires an explicitly selected macOS AVPlayer probe and local FFmpeg tools"]
async fn native_diagnostic_extended_adaptation_and_recovery() {
    run_native_case(NativeCase::ExtendedDiagnostic).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicitly selected macOS AVPlayer probe and local FFmpeg tools"]
async fn native_progressive_buffered_fallback_keeps_playing() {
    run_native_case(NativeCase::BufferedContinuity).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicitly selected macOS AVPlayer probe and local FFmpeg tools"]
async fn native_transcoded_output_decodes() {
    run_native_case(NativeCase::Transcoding).await;
}

async fn run_native_case(case: NativeCase) {
    let report = Arc::new(Mutex::new(AcceptanceReport::new(case)));
    let report_path = env::var_os("LAN_MEDIA_ACCEPTANCE_ARTIFACTS")
        .map(PathBuf::from)
        .map(|directory| directory.join(case.report_name()));

    let outcome = match report_path.as_ref() {
        Some(path) => {
            match tokio::fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new("."))).await {
                Ok(()) => match timeout(case.deadline(), run_case(case, Arc::clone(&report))).await
                {
                    Ok(result) => result,
                    Err(_) => {
                        Err("native acceptance case exceeded its finite case deadline".to_owned())
                    }
                },
                Err(_) => Err("acceptance artifact directory could not be created".to_owned()),
            }
        }
        None => Err("LAN_MEDIA_ACCEPTANCE_ARTIFACTS is required".to_owned()),
    };

    if let Ok(mut report) = report.lock() {
        if outcome.is_ok() {
            report.status = "passed".to_owned();
            report.error = None;
        } else {
            report.status = "failed".to_owned();
            report.error = outcome.as_ref().err().map(|error| bounded_error(error));
        }
    }

    let write_result = match report_path {
        Some(path) => write_report(&path, &report).await,
        None => Err("acceptance report path is unavailable".to_owned()),
    };
    assert!(
        outcome.is_ok() && write_result.is_ok(),
        "{} failed: {}; report: {}",
        case.full_name(),
        outcome
            .err()
            .unwrap_or_else(|| write_result.err().unwrap_or_default()),
        case.report_name()
    );
}

pub(crate) async fn run_single_player_prewarm_case(
    mode: crate::media_prewarm_comparison_tests::PrewarmMode,
) {
    let case = match mode {
        crate::media_prewarm_comparison_tests::PrewarmMode::Off => {
            NativeCase::SinglePlayerPrewarmControl
        }
        crate::media_prewarm_comparison_tests::PrewarmMode::On => {
            NativeCase::SinglePlayerPrewarmTreatment
        }
    };
    run_native_case(case).await;
}

async fn run_case(case: NativeCase, report: ReportStore) -> Result<(), String> {
    if case.is_prewarm_diagnostic() {
        let selected = crate::media_prewarm_comparison_tests::PrewarmMode::parse(
            env::var("LAN_MEDIA_ACCEPTANCE_PREWARM").ok().as_deref(),
        )?;
        if Some(selected) != case.prewarm_mode() {
            return Err("explicit prewarm selector did not match this named test".to_owned());
        }
    }
    let probe_path = required_executable("LAN_MEDIA_ACCEPTANCE_PROBE_BINARY")?;
    let ffmpeg = find_executable("ffmpeg")?;
    let ffprobe = find_executable("ffprobe")?;
    let temporary = tempfile::Builder::new()
        .prefix("lan-media-acceptance-")
        .tempdir()
        .map_err(|_| "temporary acceptance directory could not be created".to_owned())?;
    let temporary_root = temporary
        .path()
        .canonicalize()
        .map_err(|_| "temporary acceptance directory could not be canonicalized".to_owned())?;
    let directories = create_case_directories(&temporary_root).await?;

    let fixture_duration = case.source_duration_seconds();
    let fixture = generate_and_inspect_fixture(
        case,
        &ffmpeg,
        &ffprobe,
        &directories.fixtures,
        fixture_duration,
        &report,
    )
    .await?;
    enforce_fixture_budget(case, &fixture)?;

    let origin_listener = bind_loopback_listener().await?;
    let origin_address = origin_listener
        .local_addr()
        .map_err(|_| "local fixture origin address is unavailable".to_owned())?;
    let origin = make_origin_state(case, &fixture, origin_address, Arc::clone(&report))?;
    let origin_router = Router::new()
        .route("/{file_name}", get(origin_get).head(origin_head))
        .with_state(Arc::clone(&origin));
    let origin_task = ServerTask(Some(tokio::spawn(async move {
        let _ = axum::serve(origin_listener, origin_router).await;
    })));

    let media_listener = bind_loopback_listener().await?;
    let media_address = media_listener
        .local_addr()
        .map_err(|_| "local media listener address is unavailable".to_owned())?;
    let mut state = create_app_state(
        &temporary_root,
        directories,
        media_address,
        &ffmpeg,
        matches!(case, NativeCase::Transcoding),
        case.cache_limit_bytes(),
    )?;
    state.hls_upstream_client = local_only_http_client()?;

    let task_creation = state
        .tasks
        .create_bilibili_playback_task("native-media-acceptance", None, None)
        .map_err(|_| "credential-free local playback task could not be created".to_owned())?;
    let task_id = task_creation.task.id;
    let policy = PlaybackPolicy {
        transcoding_preference: TranscodingPreference::Auto,
        weak_network_preference: WeakNetworkPreference::AvPlayerManaged,
        ..PlaybackPolicy::default()
    };
    let source_variants = create_source_variants(case, &fixture, origin_address)?;
    let selected = source_variants
        .first()
        .ok_or_else(|| "local fixture has no selected video representation".to_owned())?;
    let abr = make_abr_metadata(case, &fixture)?;
    let mut session = HlsPlaybackSession::from_playback_entry_with_policy(
        &task_id,
        "Local native media acceptance",
        selected,
        &abr,
        &source_variants,
        policy,
    )
    .map_err(|_| "production HLS playback session construction failed".to_owned())?;
    let mut playlist_video_fragments = fixture.high_video.record.fragment_count;
    let mut playlist_video_peak = fixture.high_video.record.peak_bandwidth_bits_per_second;
    let mut playlist_video_target_duration = fixture.high_video.record.target_duration_seconds;
    if matches!(case, NativeCase::Transcoding) {
        session.transcoding =
            HlsTranscodingPlan::for_variant_with_policy(&state.options, selected, policy);
        if session.transcoding.state != crate::transcoding::HlsTranscodingPlanState::Ready {
            return Err(
                "production transcoding planner did not select a real transcode".to_owned(),
            );
        }
    }

    let media_base = format!("http://{media_address}");
    let master_url = format!("{media_base}/hls/{task_id}/master.m3u8");
    ensure_loopback_url(&master_url)?;
    publish_playable_session(&state, &session, &task_id, &master_url)?;

    if matches!(case, NativeCase::Transcoding) {
        let execution = HlsTranscodingExecutionConfig {
            ffmpeg_path: ffmpeg.clone(),
            permits: Arc::new(tokio::sync::Semaphore::new(1)),
            active_job_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let completion = timeout(
            FFMPEG_TIMEOUT + Duration::from_secs(45),
            state
                .hls_cache
                .cache_session_resources_completion_with_control(
                    &state.hls_upstream_client,
                    &session,
                    || HlsCacheFillControl::Continue,
                    |_| {},
                    Some(execution),
                ),
        )
        .await
        .map_err(|_| "production HLS cache fill and transcode exceeded its deadline".to_owned())?
        .map_err(|_| "production HLS cache fill or transcode failed".to_owned())?;
        let cached = state
            .hls_cache
            .cached_resource(&task_id, &completion.session.variant.video.id)
            .ok_or_else(|| "production transcode output was not durably cached".to_owned())?;
        let output = inspect_mp4(
            &cached.path,
            1,
            ffprobe.as_path(),
            false,
            MAX_FIXTURE_BYTES,
            Duration::from_secs(15),
        )
        .await?;
        let probe_metadata = ffprobe_media(&ffprobe, &cached.path, Duration::from_secs(15)).await?;
        let output_streams = probe_metadata.streams;
        let video = output_streams
            .iter()
            .find(|stream| stream.codec_type == "video")
            .ok_or_else(|| "production transcode output has no video track".to_owned())?;
        let audio_tracks = output_streams
            .iter()
            .filter(|stream| stream.codec_type == "audio")
            .count() as u64;
        let audio = output_streams
            .iter()
            .find(|stream| stream.codec_type == "audio")
            .ok_or_else(|| "production transcode output has no AAC track".to_owned())?;
        if video.codec_name.as_deref() != Some("h264")
            || audio.codec_name.as_deref() != Some("aac")
            || video.width != Some(640)
            || video.height != Some(360)
        {
            return Err(
                "production transcoded MP4 metadata did not match H.264/AAC 640x360".to_owned(),
            );
        }
        if cached.total_length > MAX_FIXTURE_BYTES {
            return Err("production transcoded media exceeded the 96 MiB bound".to_owned());
        }
        playlist_video_fragments = output.record.fragment_count;
        playlist_video_peak = output.record.peak_bandwidth_bits_per_second;
        playlist_video_target_duration = output.record.target_duration_seconds;
        let published_variant_bandwidth = completion.session.variant.bandwidth;
        if let Ok(mut data) = report.lock() {
            data.transcoding_metadata = Some(TranscodingMetadata {
                executed: true,
                output_file_bytes: cached.total_length,
                output_initialization_bytes: output.record.initialization_bytes,
                output_fragment_count: output.record.fragment_count,
                output_fragment_duration_ms: output.record.fragment_duration_ms,
                output_peak_bandwidth_bits_per_second: output.record.peak_bandwidth_bits_per_second,
                output_target_duration_seconds: output.record.target_duration_seconds,
                published_variant_bandwidth_bits_per_second: published_variant_bandwidth,
                output_global_sidx_present: output.record.global_sidx_present,
                output_width: video.width.unwrap_or_default(),
                output_height: video.height.unwrap_or_default(),
                output_video_codec_code: 1,
                output_audio_codec_code: 1,
                output_audio_track_count: audio_tracks,
            });
        }
        session = completion.session;
        state.register_hls_playback_session(session.clone());
    }

    let telemetry = Arc::new(LanIngressTelemetry::new(
        &session,
        Arc::clone(&report),
        Arc::clone(&origin.phase),
        Arc::clone(&origin.epoch),
    ));
    let media_router = crate::media_router(state.clone()).layer(from_fn_with_state(
        Arc::clone(&telemetry),
        record_lan_ingress,
    ));
    let media_task = ServerTask(Some(tokio::spawn(async move {
        let _ = axum::serve(media_listener, media_router).await;
    })));
    let local_client = local_only_http_client()?;
    let playlist_preflight = validate_master_playlist(
        &local_client,
        &master_url,
        case,
        PlaylistExpectations {
            video_fragments: playlist_video_fragments,
            audio_fragments: fixture.audio.record.fragment_count,
            video_peak_bandwidth: playlist_video_peak,
            audio_peak_bandwidth: fixture.audio.record.peak_bandwidth_bits_per_second,
            video_target_duration: playlist_video_target_duration,
            audio_target_duration: fixture.audio.record.target_duration_seconds,
        },
        case.uses_long_profile(),
    )
    .await?;
    if let Ok(mut data) = report.lock() {
        data.hls_playlist_preflight = Some(playlist_preflight);
    }

    origin.reset_probe_epoch();
    if case.uses_progressive_abr()
        && let Ok(mut data) = report.lock()
    {
        data.fault_timeline = FaultTimeline {
            enabled: true,
            bad_high_status_code: Some(StatusCode::SERVICE_UNAVAILABLE.as_u16()),
            ..FaultTimeline::default()
        };
    }

    let probe_duration = case.probe_duration_seconds();
    let probe_deadline = case.probe_deadline_seconds();
    let expected_sizes = match case {
        NativeCase::BufferedContinuity => "640x360,320x180",
        NativeCase::ProgressiveAbr | NativeCase::ExtendedDiagnostic => "640x360,320x180,640x360",
        NativeCase::SinglePlayerPrewarmControl | NativeCase::SinglePlayerPrewarmTreatment => {
            "640x360,320x180,640x360"
        }
        NativeCase::Transcoding => "640x360",
    };
    let ready_marker_path = if case.uses_progressive_abr() {
        let path = temporary_root.join("decoder-ready.json");
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(
                    "decoder readiness marker already existed in the private fixture root"
                        .to_owned(),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err("decoder readiness marker path could not be checked safely".to_owned());
            }
        }
        Some(path)
    } else {
        None
    };
    let recovery_marker_path = case
        .uses_progressive_abr()
        .then(|| temporary_root.join("recovery-observation.trigger"));
    let recovery_snapshot_path = case
        .is_prewarm_diagnostic()
        .then(|| temporary_root.join("recovery-snapshot.json"));
    let minimum_steady_play_seconds =
        matches!(case, NativeCase::BufferedContinuity).then_some(MINIMUM_STEADY_PLAY_SECONDS);
    let mut probe = AbortTask(Some(tokio::spawn(run_native_probe(
        probe_path,
        master_url,
        probe_duration,
        probe_deadline,
        expected_sizes.to_owned(),
        ProbeMarkerPaths {
            ready: ready_marker_path.clone(),
            recovery_observation: recovery_marker_path.clone(),
            recovery_snapshot: recovery_snapshot_path.clone(),
        },
        minimum_steady_play_seconds,
    ))));

    if case.uses_progressive_abr() {
        let timeline = run_abr_fault_timeline(
            &origin,
            &mut probe,
            ready_marker_path
                .as_deref()
                .expect("ABR marker path is present"),
            &report,
            recovery_marker_path
                .as_deref()
                .expect("recovery observation marker path is present"),
        )
        .await;
        if let Err(error) = timeline {
            probe.abort();
            return Err(error);
        }
    }

    if case.is_prewarm_diagnostic() {
        let snapshot_path = recovery_snapshot_path
            .as_deref()
            .ok_or_else(|| "diagnostic recovery snapshot path was unavailable".to_owned())?;
        let high = &session.variant.video;
        let audio =
            session.variant.audio.as_ref().ok_or_else(|| {
                "selected HLS variant lacked its shared audio resource".to_owned()
            })?;
        let (snapshot, plan) =
            wait_for_recovery_plan(snapshot_path, &fixture, &report, &high.id, &audio.id).await?;
        let mode = case
            .prewarm_mode()
            .ok_or_else(|| "prewarm diagnostic mode was unavailable".to_owned())?;
        let coverage_before =
            selected_range_coverage(&state.hls_cache, &session.id, high, audio, &plan).await?;
        let started = Instant::now();
        let mut execution =
            crate::media_prewarm_comparison_tests::PrewarmExecution::initial(coverage_before);
        publish_prewarm_progress(&report, mode, &snapshot, &plan, &mut execution, started)?;
        let prewarm_result = match mode {
            crate::media_prewarm_comparison_tests::PrewarmMode::Off => {
                execution.completed = true;
                Ok(())
            }
            crate::media_prewarm_comparison_tests::PrewarmMode::On => {
                prewarm_selected_ranges(
                    PrewarmRunContext {
                        cache: &state.hls_cache,
                        client: &state.hls_upstream_client,
                        session_id: &session.id,
                        high,
                        audio,
                        plan: &plan,
                        deadline: tokio::time::Instant::now() + Duration::from_secs(15),
                        report: &report,
                        mode,
                        snapshot: &snapshot,
                        started,
                    },
                    &mut execution,
                )
                .await
            }
        };
        let coverage_after = if prewarm_result.is_ok() {
            selected_range_coverage(&state.hls_cache, &session.id, high, audio, &plan).await
        } else {
            Err("coverage observation skipped after prewarm failure".to_owned())
        };
        if let Ok(coverage_after) = coverage_after {
            execution.coverage_after = Some(coverage_after);
        }
        let prewarm_result = if prewarm_result.is_ok()
            && execution.coverage_after.is_none_or(|coverage| {
                mode == crate::media_prewarm_comparison_tests::PrewarmMode::On
                    && coverage < plan.planned_bytes
            }) {
            execution.failure = prewarm_failure_without_range(
                crate::media_prewarm_comparison_tests::PrewarmFailureCategory::CacheCoverageIncomplete,
            );
            Err("successful prewarm did not produce a valid durable coverage observation or cover every selected range".to_owned())
        } else {
            prewarm_result
        };
        execution.completed = prewarm_result.is_ok();
        execution.elapsed_seconds = started.elapsed().as_secs_f64();
        publish_prewarm_progress(&report, mode, &snapshot, &plan, &mut execution, started)?;
        prewarm_result?;
    }

    let probe_timeout = Duration::from_secs(probe_deadline) + PROBE_EXIT_GRACE;
    let process = timeout(probe_timeout, probe.join()).await;
    let process = match process {
        Ok(result) => result?,
        Err(_) => {
            probe.abort();
            return Err("native probe exceeded its declared deadline".to_owned());
        }
    };
    let probe_value = parse_probe_result(process?, &report)?;
    validate_probe_result(case, probe_duration, &probe_value, &report)?;
    if case.uses_progressive_abr() {
        validate_abr_request_trace(&report, case)?;
        validate_abr_sample_timeline(case, &probe_value, &report)?;
        validate_recovery_observations(case, &probe_value, &report)?;
        if case.is_prewarm_diagnostic()
            && probe_value["video_size_expectations"]["entries"][2]["matched"].as_bool()
                == Some(true)
        {
            let data = report
                .lock()
                .map_err(|_| "acceptance report lock was poisoned".to_owned())?;
            if data.lan_ingress.count_by_phase_and_representation[2][0] == 0 {
                return Err(
                    "decoded recovery high lacked a corresponding LAN client high request"
                        .to_owned(),
                );
            }
        }
    }

    media_task.stop().await;
    origin_task.stop().await;
    Ok(())
}

async fn wait_for_recovery_plan(
    snapshot_path: &Path,
    fixture: &MediaFixture,
    report: &ReportStore,
    video_resource_id: &str,
    audio_resource_id: &str,
) -> Result<
    (
        crate::media_prewarm_comparison_tests::RecoverySnapshot,
        crate::media_prewarm_comparison_tests::PrewarmPlan,
    ),
    String,
> {
    let (recovery_start, recovery_end) = {
        let data = report
            .lock()
            .map_err(|_| "acceptance report lock was poisoned".to_owned())?;
        (
            data.fault_timeline
                .recovery_before_monotonic_seconds
                .ok_or_else(|| "recovery pre-transition clock was missing".to_owned())?,
            data.fault_timeline
                .recovery_after_monotonic_seconds
                .ok_or_else(|| "recovery post-transition clock was missing".to_owned())?,
        )
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let snapshot = loop {
        let now = monotonic_clock_seconds()?;
        match crate::media_prewarm_comparison_tests::read_recovery_snapshot(
            snapshot_path,
            recovery_start,
            now,
            f64::from(LONG_RECOVERY_MEDIA_DURATION_SECONDS),
        ) {
            Ok(snapshot) => break snapshot,
            Err(error) if Instant::now() < deadline => {
                if !error.contains("could not be inspected") {
                    return Err(error);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error),
        }
    };
    if snapshot.observed_monotonic_seconds < recovery_start
        || snapshot.observed_monotonic_seconds > recovery_end + 5.0
        || snapshot.observed_monotonic_seconds > monotonic_clock_seconds()?
    {
        return Err("recovery snapshot was outside the recovery transition bounds".to_owned());
    }
    let audio = fixture.audio.record.initialization_bytes;
    let plan = crate::media_prewarm_comparison_tests::plan_window(
        &snapshot,
        f64::from(LONG_RECOVERY_MEDIA_DURATION_SECONDS),
        [
            crate::media_prewarm_comparison_tests::PrewarmTrackInput {
                track_index: 0,
                resource_id: video_resource_id,
                actual_total_length: fixture.high_video.record.file_bytes,
                initialization_bytes: fixture.high_video.record.initialization_bytes,
                fragments: &fixture.high_video.fragments,
            },
            crate::media_prewarm_comparison_tests::PrewarmTrackInput {
                track_index: 1,
                resource_id: audio_resource_id,
                actual_total_length: fixture.audio.record.file_bytes,
                initialization_bytes: audio,
                fragments: &fixture.audio.fragments,
            },
        ],
    )?;
    Ok((snapshot, plan))
}

async fn selected_range_coverage(
    cache: &HlsCacheStore,
    session_id: &str,
    high: &crate::hls::HlsMediaResource,
    audio: &crate::hls::HlsMediaResource,
    plan: &crate::media_prewarm_comparison_tests::PrewarmPlan,
) -> Result<u64, String> {
    let mut covered = 0_u64;
    for range in &plan.ranges {
        let resource = match range.resource_id.as_str() {
            id if id == high.id => high,
            id if id == audio.id => audio,
            _ => return Err("prewarm plan referenced an unexpected HLS resource".to_owned()),
        };
        let bytes = cache
            .read_resource_range(session_id, resource, range.start..range.end)
            .await
            .map_err(|_| "selected durable HLS range could not be inspected".to_owned())?;
        if bytes.is_some() {
            covered = covered
                .checked_add(range.byte_len()?)
                .ok_or_else(|| "selected cache coverage count overflowed".to_owned())?;
        }
    }
    Ok(covered)
}

fn publish_prewarm_progress(
    report: &ReportStore,
    mode: crate::media_prewarm_comparison_tests::PrewarmMode,
    snapshot: &crate::media_prewarm_comparison_tests::RecoverySnapshot,
    plan: &crate::media_prewarm_comparison_tests::PrewarmPlan,
    execution: &mut crate::media_prewarm_comparison_tests::PrewarmExecution,
    started: Instant,
) -> Result<(), String> {
    execution.elapsed_seconds = started.elapsed().as_secs_f64();
    let comparison = crate::media_prewarm_comparison_tests::report_for(
        mode,
        snapshot.clone(),
        plan.clone(),
        execution.clone(),
    );
    report
        .lock()
        .map_err(|_| "acceptance report lock was poisoned".to_owned())?
        .prewarm_comparison = Some(comparison);
    Ok(())
}

fn prewarm_failure_without_range(
    category: crate::media_prewarm_comparison_tests::PrewarmFailureCategory,
) -> crate::media_prewarm_comparison_tests::PrewarmFailure {
    crate::media_prewarm_comparison_tests::PrewarmFailure {
        category: Some(category),
        ..Default::default()
    }
}

fn prewarm_failure_for_range(
    category: crate::media_prewarm_comparison_tests::PrewarmFailureCategory,
    range: &crate::media_prewarm_comparison_tests::SelectedRange,
    upstream_status: Option<u16>,
) -> crate::media_prewarm_comparison_tests::PrewarmFailure {
    crate::media_prewarm_comparison_tests::PrewarmFailure {
        category: Some(category),
        resource_id: Some(range.resource_id.clone()),
        start: Some(range.start),
        end: Some(range.end),
        upstream_status,
    }
}

fn prewarm_failure_message(
    failure: &crate::media_prewarm_comparison_tests::PrewarmFailure,
) -> String {
    let category = match failure.category {
        Some(crate::media_prewarm_comparison_tests::PrewarmFailureCategory::PlanInvalid) => {
            "plan_invalid"
        }
        Some(crate::media_prewarm_comparison_tests::PrewarmFailureCategory::Cancelled) => {
            "cancelled"
        }
        Some(crate::media_prewarm_comparison_tests::PrewarmFailureCategory::SessionRemoving) => {
            "session_removing"
        }
        Some(crate::media_prewarm_comparison_tests::PrewarmFailureCategory::IdentityChanged) => {
            "identity_changed"
        }
        Some(crate::media_prewarm_comparison_tests::PrewarmFailureCategory::QuotaExceeded) => {
            "quota_exceeded"
        }
        Some(crate::media_prewarm_comparison_tests::PrewarmFailureCategory::UpstreamStatus) => {
            "upstream_status"
        }
        Some(crate::media_prewarm_comparison_tests::PrewarmFailureCategory::Network) => "network",
        Some(crate::media_prewarm_comparison_tests::PrewarmFailureCategory::Io) => "io",
        Some(crate::media_prewarm_comparison_tests::PrewarmFailureCategory::RangeUnsupported) => {
            "range_unsupported"
        }
        Some(crate::media_prewarm_comparison_tests::PrewarmFailureCategory::InvalidResponse) => {
            "invalid_response"
        }
        Some(crate::media_prewarm_comparison_tests::PrewarmFailureCategory::DeadlineExceeded) => {
            "deadline_exceeded"
        }
        Some(crate::media_prewarm_comparison_tests::PrewarmFailureCategory::AttemptLimit) => {
            "attempt_limit"
        }
        Some(crate::media_prewarm_comparison_tests::PrewarmFailureCategory::ByteBudgetExceeded) => {
            "byte_budget_exceeded"
        }
        Some(
            crate::media_prewarm_comparison_tests::PrewarmFailureCategory::ResourceLengthChanged,
        ) => "resource_length_changed",
        Some(
            crate::media_prewarm_comparison_tests::PrewarmFailureCategory::CacheCoverageIncomplete,
        ) => "cache_coverage_incomplete",
        None => "unknown",
    };
    match (
        &failure.resource_id,
        failure.start,
        failure.end,
        failure.upstream_status,
    ) {
        (Some(resource), Some(start), Some(end), Some(status)) => format!(
            "bounded HLS prewarm failed: category={category} resource={resource} range={start}..{end} upstream_status={status}"
        ),
        (Some(resource), Some(start), Some(end), None) => format!(
            "bounded HLS prewarm failed: category={category} resource={resource} range={start}..{end}"
        ),
        _ => format!("bounded HLS prewarm failed: category={category}"),
    }
}

fn prewarm_range_error(
    error: &crate::hls_range_cache::HlsRangeError,
) -> (
    crate::media_prewarm_comparison_tests::PrewarmOutcome,
    Option<crate::media_prewarm_comparison_tests::PrewarmFailureCategory>,
    Option<u16>,
) {
    use crate::hls_range_cache::HlsRangeError;
    use crate::media_prewarm_comparison_tests::{
        PrewarmFailureCategory as Failure, PrewarmOutcome,
    };

    match error {
        HlsRangeError::Preempted => (PrewarmOutcome::Preempted, None, None),
        HlsRangeError::Cancelled => (PrewarmOutcome::Cancelled, Some(Failure::Cancelled), None),
        HlsRangeError::SessionRemoving => (
            PrewarmOutcome::SessionRemoving,
            Some(Failure::SessionRemoving),
            None,
        ),
        HlsRangeError::IdentityChanged => (
            PrewarmOutcome::IdentityChanged,
            Some(Failure::IdentityChanged),
            None,
        ),
        HlsRangeError::QuotaExceeded => (
            PrewarmOutcome::QuotaExceeded,
            Some(Failure::QuotaExceeded),
            None,
        ),
        HlsRangeError::UpstreamStatus(status) => (
            PrewarmOutcome::UpstreamStatus,
            Some(Failure::UpstreamStatus),
            Some(status.as_u16()),
        ),
        HlsRangeError::Network(_) => (PrewarmOutcome::Network, Some(Failure::Network), None),
        HlsRangeError::Io(_) => (PrewarmOutcome::Io, Some(Failure::Io), None),
        HlsRangeError::RangeUnsupported => (
            PrewarmOutcome::RangeUnsupported,
            Some(Failure::RangeUnsupported),
            None,
        ),
        HlsRangeError::InvalidResponse(_) => (
            PrewarmOutcome::InvalidResponse,
            Some(Failure::InvalidResponse),
            None,
        ),
    }
}

async fn wait_for_foreground_quiescence(
    cache: &HlsCacheStore,
    deadline: tokio::time::Instant,
) -> bool {
    const QUIET_INTERVAL: Duration = Duration::from_millis(100);
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    let mut quiet_since = None;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        if cache.foreground_activity_count_for_test() == 0 {
            let quiet_start = *quiet_since.get_or_insert(now);
            if now.duration_since(quiet_start) >= QUIET_INTERVAL {
                return true;
            }
        } else {
            quiet_since = None;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let sleep_for = remaining.min(POLL_INTERVAL);
        if tokio::time::timeout_at(deadline, tokio::time::sleep(sleep_for))
            .await
            .is_err()
        {
            return false;
        }
    }
}

struct PrewarmRunContext<'a> {
    cache: &'a HlsCacheStore,
    client: &'a reqwest::Client,
    session_id: &'a str,
    high: &'a crate::hls::HlsMediaResource,
    audio: &'a crate::hls::HlsMediaResource,
    plan: &'a crate::media_prewarm_comparison_tests::PrewarmPlan,
    deadline: tokio::time::Instant,
    report: &'a ReportStore,
    mode: crate::media_prewarm_comparison_tests::PrewarmMode,
    snapshot: &'a crate::media_prewarm_comparison_tests::RecoverySnapshot,
    started: Instant,
}

async fn prewarm_selected_ranges(
    context: PrewarmRunContext<'_>,
    execution: &mut crate::media_prewarm_comparison_tests::PrewarmExecution,
) -> Result<(), String> {
    use crate::media_prewarm_comparison_tests::{
        PrewarmAttempt, PrewarmFailureCategory as Failure, PrewarmOutcome,
    };

    let PrewarmRunContext {
        cache,
        client,
        session_id,
        high,
        audio,
        plan,
        deadline,
        report,
        mode,
        snapshot,
        started,
    } = context;

    let invalid_plan = plan.resource_ids.video != high.id
        || plan.resource_ids.audio != audio.id
        || high.id == audio.id
        || plan.transfer_ranges.is_empty()
        || plan.transfer_ranges.len() > 128
        || plan.ranges.is_empty()
        || plan.ranges.len() > 128;
    if invalid_plan {
        execution.failure = prewarm_failure_without_range(Failure::PlanInvalid);
        return Err(record_prewarm_failure(
            report, mode, snapshot, plan, execution, started,
        )?);
    }

    let selected_bytes_valid = plan
        .ranges
        .iter()
        .try_fold(0_u64, |sum, range| sum.checked_add(range.byte_len().ok()?))
        .is_some_and(|bytes| bytes == plan.planned_bytes && bytes <= 8 * 1024 * 1024);
    if !selected_bytes_valid {
        execution.failure = prewarm_failure_without_range(Failure::PlanInvalid);
        return Err(record_prewarm_failure(
            report, mode, snapshot, plan, execution, started,
        )?);
    }
    let transfer_bytes = match plan
        .transfer_ranges
        .iter()
        .try_fold(0_u64, |sum, range| sum.checked_add(range.byte_len().ok()?))
    {
        Some(bytes) if bytes == plan.planned_transfer_bytes => bytes,
        _ => {
            execution.failure = prewarm_failure_without_range(Failure::PlanInvalid);
            return Err(record_prewarm_failure(
                report, mode, snapshot, plan, execution, started,
            )?);
        }
    };
    if transfer_bytes > 16 * 1024 * 1024 {
        execution.failure = prewarm_failure_without_range(Failure::ByteBudgetExceeded);
        return Err(record_prewarm_failure(
            report, mode, snapshot, plan, execution, started,
        )?);
    }
    if plan
        .transfer_ranges
        .iter()
        .any(|range| range.resource_id != high.id && range.resource_id != audio.id)
        || plan.transfer_ranges.windows(2).any(|pair| {
            pair[0].resource_id > pair[1].resource_id
                || (pair[0].resource_id == pair[1].resource_id && pair[0].end > pair[1].start)
        })
        || plan.ranges.iter().any(|selected| {
            !plan.transfer_ranges.iter().any(|transfer| {
                transfer.resource_id == selected.resource_id
                    && transfer.start <= selected.start
                    && selected.end <= transfer.end
            })
        })
    {
        execution.failure = prewarm_failure_without_range(Failure::PlanInvalid);
        return Err(record_prewarm_failure(
            report, mode, snapshot, plan, execution, started,
        )?);
    }

    for range in &plan.transfer_ranges {
        let length = match range.byte_len() {
            Ok(length) => length,
            Err(_) => {
                execution.failure = prewarm_failure_for_range(Failure::PlanInvalid, range, None);
                return Err(record_prewarm_failure(
                    report, mode, snapshot, plan, execution, started,
                )?);
            }
        };
        let resource = if range.resource_id == high.id {
            high
        } else {
            audio
        };
        let expected_total = if range.resource_id == high.id {
            plan.video_total_length
        } else {
            plan.audio_total_length
        };
        loop {
            let range_count = execution.attempted_ranges.len();
            let next_requested_bytes = execution.operation_requested_bytes.saturating_add(length);
            if range_count >= 128 {
                execution.failure = prewarm_failure_for_range(Failure::AttemptLimit, range, None);
                return Err(record_prewarm_failure(
                    report, mode, snapshot, plan, execution, started,
                )?);
            }
            if next_requested_bytes > 16 * 1024 * 1024 {
                execution.failure =
                    prewarm_failure_for_range(Failure::ByteBudgetExceeded, range, None);
                return Err(record_prewarm_failure(
                    report, mode, snapshot, plan, execution, started,
                )?);
            }
            if tokio::time::Instant::now() >= deadline {
                execution.failure =
                    prewarm_failure_for_range(Failure::DeadlineExceeded, range, None);
                return Err(record_prewarm_failure(
                    report, mode, snapshot, plan, execution, started,
                )?);
            }

            let attempt_index = execution.attempted_ranges.len();
            execution.operation_requested_bytes = next_requested_bytes;
            execution.operation_count = u64::try_from(attempt_index + 1).unwrap_or(u64::MAX);
            execution.attempted_ranges.push(PrewarmAttempt {
                resource_id: range.resource_id.clone(),
                start: range.start,
                end: range.end,
                outcome: PrewarmOutcome::InProgress,
            });
            let control = || HlsCacheFillControl::Continue;
            let ensure = cache.ensure_resource_range(
                client,
                session_id,
                resource,
                range.start..range.end,
                HlsRangePriority::Background,
                &control,
            );
            publish_prewarm_progress(report, mode, snapshot, plan, execution, started)?;

            match tokio::time::timeout_at(deadline, ensure).await {
                Err(_) => {
                    execution.attempted_ranges[attempt_index].outcome =
                        PrewarmOutcome::DeadlineExceeded;
                    execution.failure =
                        prewarm_failure_for_range(Failure::DeadlineExceeded, range, None);
                    return Err(record_prewarm_failure(
                        report, mode, snapshot, plan, execution, started,
                    )?);
                }
                Ok(Err(error)) => {
                    let (outcome, category, upstream_status) = prewarm_range_error(&error);
                    execution.attempted_ranges[attempt_index].outcome = outcome;
                    if outcome == PrewarmOutcome::Preempted {
                        execution.preempted_operation_count += 1;
                        publish_prewarm_progress(report, mode, snapshot, plan, execution, started)?;
                        if execution.attempted_ranges.len() >= 128 {
                            execution.failure =
                                prewarm_failure_for_range(Failure::AttemptLimit, range, None);
                            return Err(record_prewarm_failure(
                                report, mode, snapshot, plan, execution, started,
                            )?);
                        }
                        if execution.operation_requested_bytes.saturating_add(length)
                            > 16 * 1024 * 1024
                        {
                            execution.failure =
                                prewarm_failure_for_range(Failure::ByteBudgetExceeded, range, None);
                            return Err(record_prewarm_failure(
                                report, mode, snapshot, plan, execution, started,
                            )?);
                        }
                        execution.foreground_defer_count += 1;
                        publish_prewarm_progress(report, mode, snapshot, plan, execution, started)?;
                        if !wait_for_foreground_quiescence(cache, deadline).await {
                            execution.failure =
                                prewarm_failure_for_range(Failure::DeadlineExceeded, range, None);
                            return Err(record_prewarm_failure(
                                report, mode, snapshot, plan, execution, started,
                            )?);
                        }
                        continue;
                    }
                    execution.failure = prewarm_failure_for_range(
                        category.expect("non-preempted HLS errors have typed categories"),
                        range,
                        upstream_status,
                    );
                    return Err(record_prewarm_failure(
                        report, mode, snapshot, plan, execution, started,
                    )?);
                }
                Ok(Ok(ready)) if ready.total_length != expected_total => {
                    execution.attempted_ranges[attempt_index].outcome =
                        PrewarmOutcome::ResourceLengthChanged;
                    execution.failure =
                        prewarm_failure_for_range(Failure::ResourceLengthChanged, range, None);
                    return Err(record_prewarm_failure(
                        report, mode, snapshot, plan, execution, started,
                    )?);
                }
                Ok(Ok(_)) => {
                    execution.attempted_ranges[attempt_index].outcome = PrewarmOutcome::Completed;
                    execution.completed_transfer_bytes =
                        execution.completed_transfer_bytes.saturating_add(length);
                    execution.completed_transfer_count += 1;
                    publish_prewarm_progress(report, mode, snapshot, plan, execution, started)?;
                    break;
                }
            }
        }
    }

    if execution.operation_count != execution.attempted_ranges.len() as u64
        || execution.completed_transfer_bytes != plan.planned_transfer_bytes
        || execution.completed_transfer_count != plan.transfer_ranges.len() as u64
    {
        execution.failure = prewarm_failure_without_range(Failure::PlanInvalid);
        return Err(record_prewarm_failure(
            report, mode, snapshot, plan, execution, started,
        )?);
    }
    execution.completed = true;
    publish_prewarm_progress(report, mode, snapshot, plan, execution, started)?;
    Ok(())
}

fn record_prewarm_failure(
    report: &ReportStore,
    mode: crate::media_prewarm_comparison_tests::PrewarmMode,
    snapshot: &crate::media_prewarm_comparison_tests::RecoverySnapshot,
    plan: &crate::media_prewarm_comparison_tests::PrewarmPlan,
    execution: &mut crate::media_prewarm_comparison_tests::PrewarmExecution,
    started: Instant,
) -> Result<String, String> {
    execution.completed = false;
    publish_prewarm_progress(report, mode, snapshot, plan, execution, started)?;
    Ok(prewarm_failure_message(&execution.failure))
}

struct CaseDirectories {
    credentials: PathBuf,
    tasks: PathBuf,
    cache: PathBuf,
    library: PathBuf,
    fixtures: PathBuf,
}

async fn create_case_directories(root: &Path) -> Result<CaseDirectories, String> {
    let directories = CaseDirectories {
        credentials: root.join("credentials"),
        tasks: root.join("tasks"),
        cache: root.join("cache"),
        library: root.join("library"),
        fixtures: root.join("fixtures"),
    };
    for path in [
        &directories.credentials,
        &directories.tasks,
        &directories.cache,
        &directories.library,
        &directories.fixtures,
    ] {
        tokio::fs::create_dir_all(path)
            .await
            .map_err(|_| "a fresh case-local media directory could not be created".to_owned())?;
    }
    Ok(directories)
}

fn create_app_state(
    temporary_root: &Path,
    directories: CaseDirectories,
    media_address: SocketAddr,
    ffmpeg: &Path,
    transcoding_enabled: bool,
    cache_limit_bytes: u64,
) -> Result<AppState, String> {
    let credentials_path = directories.credentials.join("credentials.json");
    let options = CacheServerOptions {
        root_path: directories.library,
        task_state_path: directories.tasks.join("tasks.json"),
        grpc_listen_url: "http://127.0.0.1:1".to_owned(),
        media_listen_url: format!("http://{media_address}"),
        bonjour_enabled: false,
        lan_transcoding_enabled: transcoding_enabled,
        lan_transcoding_ffmpeg_path: ffmpeg.to_path_buf(),
        lan_transcoding_max_concurrent_jobs: 1,
        bilibili_worker_enabled: false,
        allow_bilibili_login_sessions: false,
        bbdown_credential_path: Some(credentials_path),
        bbdown_credential_profile: None,
        hls_cache_max_bytes: cache_limit_bytes,
        bbdown_cdn_probe: false,
        bbdown_cdn_parallelism: 1,
        ..CacheServerOptions::default()
    };
    if !options.root_path.starts_with(temporary_root) {
        return Err("library path escaped its temporary test root".to_owned());
    }
    let state = AppState::new_with_playback_planner_and_hls_cache(
        options,
        Arc::new(RejectNetworkPlanner),
        HlsCacheStore::new(temporary_root.join("cache")),
    );
    Ok(state)
}

fn local_only_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(3))
        .read_timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| "local-only HTTP client could not be created".to_owned())
}

fn publish_playable_session(
    state: &AppState,
    session: &HlsPlaybackSession,
    task_id: &str,
    master_url: &str,
) -> Result<(), String> {
    ensure_loopback_url(master_url)?;
    state.hls_cache.save_session(session).map_err(|_| {
        "local HLS session could not be persisted in the temporary cache".to_owned()
    })?;
    state.register_hls_playback_session(session.clone());
    let playback_source = PlaybackSource {
        item_id: task_id.to_owned(),
        variant_id: session.variant.id.clone(),
        protocol: PlaybackProtocol::Hls.into(),
        uri: master_url.to_owned(),
        expires_at: None,
    };
    let playback_session = BilibiliPlaybackSession {
        id: task_id.to_owned(),
        title: session.title.clone(),
        content_id: "native-media-acceptance".to_owned(),
        selected_variant_id: session.variant.id.clone(),
        ..BilibiliPlaybackSession::default()
    };
    state
        .tasks
        .complete_playback_playable(
            task_id,
            session.title.clone(),
            playback_source,
            playback_session,
        )
        .map_err(|_| {
            "local playback task could not pass production publication checks".to_owned()
        })?;
    Ok(())
}

fn create_source_variants(
    case: NativeCase,
    fixture: &MediaFixture,
    origin_address: SocketAddr,
) -> Result<Vec<BilibiliPlaybackVariant>, String> {
    let high_url = origin_url(origin_address, "high.mp4")?;
    let audio_url = origin_url(origin_address, "audio.mp4")?;
    let abr_group_id = "native-acceptance-video-levels";
    let duration = case.source_duration_seconds();
    let high_video_bandwidth = fixture.high_video.record.peak_bandwidth_bits_per_second;
    let high = make_variant(VariantSpec {
        id: "high",
        width: 640,
        height: 360,
        bandwidth: expected_variant_peak_bandwidth(
            high_video_bandwidth,
            fixture.audio.record.peak_bandwidth_bits_per_second,
            false,
        )?,
        request_bandwidth: 2_000_000,
        duration,
        video_url: &high_url,
        audio_url: &audio_url,
        video_codec: if matches!(case, NativeCase::Transcoding) {
            "mp4v.20.9"
        } else {
            H264_BASELINE_LEVEL_3_CODEC
        },
        level: case
            .uses_progressive_abr()
            .then(|| BilibiliPlaybackAbrLevel {
                group_id: abr_group_id.to_owned(),
                level_index: 1,
                level_count: 2,
                switchable: true,
            }),
    });
    let Some(low_media) = fixture.low_video.as_ref() else {
        return Ok(vec![high]);
    };
    let low_url = origin_url(origin_address, "low.mp4")?;
    let low_video_bandwidth = low_media.record.peak_bandwidth_bits_per_second;
    let low = make_variant(VariantSpec {
        id: "low",
        width: 320,
        height: 180,
        bandwidth: expected_variant_peak_bandwidth(
            low_video_bandwidth,
            fixture.audio.record.peak_bandwidth_bits_per_second,
            false,
        )?,
        request_bandwidth: 500_000,
        duration,
        video_url: &low_url,
        audio_url: &audio_url,
        video_codec: H264_BASELINE_LEVEL_3_CODEC,
        level: Some(BilibiliPlaybackAbrLevel {
            group_id: abr_group_id.to_owned(),
            level_index: 0,
            level_count: 2,
            switchable: true,
        }),
    });
    let _ = low_media;
    Ok(vec![high, low])
}

fn make_abr_metadata(
    case: NativeCase,
    fixture: &MediaFixture,
) -> Result<BilibiliPlaybackAbrMetadata, String> {
    match case {
        NativeCase::BufferedContinuity
        | NativeCase::ProgressiveAbr
        | NativeCase::ExtendedDiagnostic
        | NativeCase::SinglePlayerPrewarmControl
        | NativeCase::SinglePlayerPrewarmTreatment => {
            let audio_peak = fixture.audio.record.peak_bandwidth_bits_per_second;
            let low_peak = aggregate_peak_bandwidth(
                fixture
                    .low_video
                    .as_ref()
                    .ok_or_else(|| "low ABR fixture was missing".to_owned())?
                    .record
                    .peak_bandwidth_bits_per_second,
                audio_peak,
            )?;
            let high_peak = aggregate_peak_bandwidth(
                fixture.high_video.record.peak_bandwidth_bits_per_second,
                audio_peak,
            )?;
            Ok(BilibiliPlaybackAbrMetadata {
                groups: vec![BilibiliPlaybackAbrGroup {
                    id: "native-acceptance-video-levels".to_owned(),
                    kind: BilibiliPlaybackAbrGroupKind::DashVideo,
                    variant_ids: vec!["high".to_owned(), "low".to_owned()],
                    level_count: 2,
                    min_bandwidth: Some(low_peak),
                    max_bandwidth: Some(high_peak),
                }],
            })
        }
        NativeCase::Transcoding => Ok(BilibiliPlaybackAbrMetadata { groups: Vec::new() }),
    }
}

struct VariantSpec<'a> {
    id: &'a str,
    width: u32,
    height: u32,
    bandwidth: u64,
    request_bandwidth: u64,
    duration: u32,
    video_url: &'a str,
    audio_url: &'a str,
    video_codec: &'a str,
    level: Option<BilibiliPlaybackAbrLevel>,
}

fn make_variant(spec: VariantSpec<'_>) -> BilibiliPlaybackVariant {
    let video_kind = BilibiliMediaRequestKind::Video;
    let audio_kind = BilibiliMediaRequestKind::Audio;
    BilibiliPlaybackVariant {
        id: spec.id.to_owned(),
        kind: BilibiliPlaybackVariantKind::Dash,
        content_id: "native-media-acceptance".to_owned(),
        bandwidth: Some(spec.bandwidth),
        codecs: vec![spec.video_codec.to_owned(), "mp4a.40.2".to_owned()],
        mime_types: vec!["video/mp4".to_owned(), "audio/mp4".to_owned()],
        width: Some(spec.width),
        height: Some(spec.height),
        frame_rate: Some("30".to_owned()),
        duration_seconds: Some(spec.duration),
        abr: spec.level,
        video: Some(make_request(
            video_kind,
            spec.video_url,
            spec.video_codec,
            Some(spec.width),
            Some(spec.height),
            Some(spec.request_bandwidth),
            spec.duration,
        )),
        audio: Some(make_request(
            audio_kind,
            spec.audio_url,
            "mp4a.40.2",
            None,
            None,
            Some(128_000),
            spec.duration,
        )),
        flv_segments: Vec::new(),
    }
}

fn make_request(
    kind: BilibiliMediaRequestKind,
    url: &str,
    codec: &str,
    width: Option<u32>,
    height: Option<u32>,
    bandwidth: Option<u64>,
    duration: u32,
) -> BilibiliMediaRequest {
    BilibiliMediaRequest {
        kind,
        stream_id: None,
        url: url.to_owned(),
        backup_urls: Vec::new(),
        headers: Vec::new(),
        mime_type: Some(match kind {
            BilibiliMediaRequestKind::Audio => "audio/mp4".to_owned(),
            _ => "video/mp4".to_owned(),
        }),
        codecs: Some(codec.to_owned()),
        bandwidth,
        width,
        height,
        frame_rate: Some("30".to_owned()),
        size: None,
        duration_seconds: Some(duration),
        cache_key: BilibiliMediaCacheKey {
            content_id: "native-media-acceptance".to_owned(),
            media_kind: kind,
            stream_id: None,
            codecs: Some(codec.to_owned()),
            source_hash: match kind {
                BilibiliMediaRequestKind::Video => format!(
                    "local-fixture-video-{}x{}-{}",
                    width.unwrap_or_default(),
                    height.unwrap_or_default(),
                    bandwidth.unwrap_or_default(),
                ),
                BilibiliMediaRequestKind::Audio => "local-fixture-audio".to_owned(),
                BilibiliMediaRequestKind::FlvSegment => "local-fixture-segment".to_owned(),
            },
        },
    }
}

async fn generate_and_inspect_fixture(
    case: NativeCase,
    ffmpeg: &Path,
    ffprobe: &Path,
    directory: &Path,
    duration: u32,
    report: &ReportStore,
) -> Result<MediaFixture, String> {
    let mut command = Command::new(ffmpeg);
    let video_input = format!("testsrc2=size=640x360:rate=30:duration={duration}");
    let audio_input = format!("sine=frequency=440:sample_rate=48000:duration={duration}");
    let mut arguments = vec![
        "-hide_banner".to_owned(),
        "-loglevel".to_owned(),
        "error".to_owned(),
        "-y".to_owned(),
        "-f".to_owned(),
        "lavfi".to_owned(),
        "-i".to_owned(),
        video_input,
        "-f".to_owned(),
        "lavfi".to_owned(),
        "-i".to_owned(),
        audio_input,
    ];

    let (high_limit, low_limit, audio_limit) = if case.uses_long_profile() {
        (
            MAX_LONG_HIGH_FIXTURE_BYTES,
            MAX_LONG_LOW_FIXTURE_BYTES,
            MAX_LONG_AUDIO_FIXTURE_BYTES,
        )
    } else {
        (
            MAX_SINGLE_HIGH_FIXTURE_BYTES,
            MAX_SINGLE_LOW_FIXTURE_BYTES,
            MAX_SINGLE_AUDIO_FIXTURE_BYTES,
        )
    };
    match case {
        NativeCase::BufferedContinuity
        | NativeCase::ProgressiveAbr
        | NativeCase::ExtendedDiagnostic
        | NativeCase::SinglePlayerPrewarmControl
        | NativeCase::SinglePlayerPrewarmTreatment => {
            push_video_output(
                &mut arguments,
                "high.mp4",
                "640:360",
                duration,
                "libx264",
                "2000k",
                high_limit,
            );
            push_video_output(
                &mut arguments,
                "low.mp4",
                "320:180",
                duration,
                "libx264",
                "500k",
                low_limit,
            );
        }
        NativeCase::Transcoding => {
            push_video_output(
                &mut arguments,
                "high.mp4",
                "640:360",
                duration,
                "mpeg4",
                "1200k",
                high_limit,
            );
        }
    }
    push_audio_output(&mut arguments, "audio.mp4", duration, audio_limit);
    command.current_dir(directory).args(arguments);
    let generated = run_process(
        command,
        if case.uses_long_profile() {
            LONG_RECOVERY_FFMPEG_TIMEOUT
        } else {
            FFMPEG_TIMEOUT
        },
        1024,
    )
    .await?;
    if !generated.success {
        return Err(format!(
            "FFmpeg fixture generation failed with exit code {}",
            generated
                .status_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        ));
    }

    let high_path = directory.join("high.mp4");
    let high = inspect_fixture(
        &high_path,
        FixtureExpectation {
            representation_code: 1,
            dimensions: Some((640, 360)),
            video_codec_code: if matches!(case, NativeCase::Transcoding) {
                2
            } else {
                1
            },
            audio_codec_code: 0,
            video_profile: source_h264_sidx_guard_applies(case)
                .then_some(("Constrained Baseline", 30)),
        },
        ffprobe,
        high_limit,
        case.uses_long_profile(),
    )
    .await?;
    let low = if case.uses_progressive_abr() {
        Some(
            inspect_fixture(
                &directory.join("low.mp4"),
                FixtureExpectation {
                    representation_code: 2,
                    dimensions: Some((320, 180)),
                    video_codec_code: 1,
                    audio_codec_code: 0,
                    video_profile: Some(("Constrained Baseline", 30)),
                },
                ffprobe,
                low_limit,
                case.uses_long_profile(),
            )
            .await?,
        )
    } else {
        None
    };
    let audio = inspect_fixture(
        &directory.join("audio.mp4"),
        FixtureExpectation {
            representation_code: 3,
            dimensions: None,
            video_codec_code: 0,
            audio_codec_code: 1,
            video_profile: None,
        },
        ffprobe,
        audio_limit,
        case.uses_long_profile(),
    )
    .await?;
    if let Ok(mut data) = report.lock() {
        data.fixture_media.clear();
        data.fixture_media.push(high.record.clone());
        if let Some(low) = low.as_ref() {
            data.fixture_media.push(low.record.clone());
        }
        data.fixture_media.push(audio.record.clone());
    }
    Ok(MediaFixture {
        high_video: high,
        low_video: low,
        audio,
    })
}

fn push_video_output(
    arguments: &mut Vec<String>,
    output_name: &str,
    size: &str,
    duration: u32,
    codec: &str,
    bitrate: &str,
    maximum_bytes: u64,
) {
    arguments.extend([
        "-map".to_owned(),
        "0:v:0".to_owned(),
        "-vf".to_owned(),
        format!("scale={size}"),
        "-c:v".to_owned(),
        codec.to_owned(),
        "-preset".to_owned(),
        "ultrafast".to_owned(),
        "-pix_fmt".to_owned(),
        "yuv420p".to_owned(),
    ]);
    if codec == "libx264" {
        arguments.extend([
            "-profile:v".to_owned(),
            "baseline".to_owned(),
            "-level:v".to_owned(),
            "3.0".to_owned(),
        ]);
    }
    arguments.extend([
        "-b:v".to_owned(),
        bitrate.to_owned(),
        "-maxrate".to_owned(),
        bitrate.to_owned(),
        "-bufsize".to_owned(),
        if codec == "mpeg4" { "2400k" } else { "5000k" }.to_owned(),
        "-g".to_owned(),
        "60".to_owned(),
        "-keyint_min".to_owned(),
        "60".to_owned(),
        "-sc_threshold".to_owned(),
        "0".to_owned(),
        "-force_key_frames".to_owned(),
        "expr:gte(t,n_forced*2)".to_owned(),
        "-an".to_owned(),
        "-movflags".to_owned(),
        "+dash+frag_keyframe+empty_moov+default_base_moof+global_sidx".to_owned(),
        "-frag_duration".to_owned(),
        "2000000".to_owned(),
        "-t".to_owned(),
        duration.to_string(),
        "-fs".to_owned(),
        maximum_bytes.to_string(),
        "-f".to_owned(),
        "mp4".to_owned(),
        output_name.to_owned(),
    ]);
}

fn push_audio_output(
    arguments: &mut Vec<String>,
    output_name: &str,
    duration: u32,
    maximum_bytes: u64,
) {
    arguments.extend([
        "-map".to_owned(),
        "1:a:0".to_owned(),
        "-vn".to_owned(),
        "-c:a".to_owned(),
        "aac".to_owned(),
        "-b:a".to_owned(),
        "128k".to_owned(),
        "-ar".to_owned(),
        "48000".to_owned(),
        "-movflags".to_owned(),
        "+dash+frag_keyframe+empty_moov+default_base_moof+global_sidx".to_owned(),
        "-frag_duration".to_owned(),
        "2000000".to_owned(),
        "-t".to_owned(),
        duration.to_string(),
        "-fs".to_owned(),
        maximum_bytes.to_string(),
        "-f".to_owned(),
        "mp4".to_owned(),
        output_name.to_owned(),
    ]);
}

async fn inspect_fixture(
    path: &Path,
    expected: FixtureExpectation,
    ffprobe: &Path,
    maximum_bytes: u64,
    long_profile: bool,
) -> Result<InspectedMedia, String> {
    let mut inspected = inspect_mp4(
        path,
        expected.representation_code,
        ffprobe,
        expected.video_codec_code != 2,
        maximum_bytes,
        if long_profile {
            LONG_RECOVERY_FFPROBE_TIMEOUT
        } else {
            Duration::from_secs(15)
        },
    )
    .await?;
    let metadata = ffprobe_media(
        ffprobe,
        path,
        if long_profile {
            LONG_RECOVERY_FFPROBE_TIMEOUT
        } else {
            Duration::from_secs(15)
        },
    )
    .await?;
    let duration = metadata
        .format
        .as_ref()
        .and_then(|format| format.duration.as_deref())
        .and_then(|duration| duration.parse::<f64>().ok());
    if !duration.is_some_and(|duration| duration.is_finite() && duration > 0.0) {
        return Err("FFmpeg fixture duration metadata was missing or invalid".to_owned());
    }
    let video = metadata
        .streams
        .iter()
        .find(|stream| stream.codec_type == "video");
    let audio = metadata
        .streams
        .iter()
        .find(|stream| stream.codec_type == "audio");
    if expected.dimensions.is_some_and(|dimensions| {
        video.and_then(|stream| stream.width.zip(stream.height)) != Some(dimensions)
    }) {
        return Err(
            "FFmpeg fixture dimensions did not match the requested representation".to_owned(),
        );
    }
    let video_codec_code = video
        .and_then(|stream| stream.codec_name.as_deref())
        .map(video_codec_code)
        .unwrap_or_default();
    let audio_codec_code = audio
        .and_then(|stream| stream.codec_name.as_deref())
        .map(audio_codec_code)
        .unwrap_or_default();
    if video_codec_code != expected.video_codec_code
        || audio_codec_code != expected.audio_codec_code
    {
        return Err(
            "FFmpeg fixture codec metadata did not match the expected video/audio tracks"
                .to_owned(),
        );
    }
    let profile = video.and_then(|stream| stream.profile.as_deref());
    let level = video.and_then(|stream| stream.level);
    if expected
        .video_profile
        .is_some_and(|(expected_profile, expected_level)| {
            !h264_baseline_level_3_is_coherent(
                H264_BASELINE_LEVEL_3_CODEC,
                profile,
                level,
                expected_profile,
                expected_level,
            )
        })
    {
        return Err("generated H.264 fixture profile or level was not Baseline 3.0".to_owned());
    }
    inspected.record.video_profile_code = profile.map(video_profile_code).unwrap_or_default();
    inspected.record.video_level = level;
    Ok(inspected)
}

async fn inspect_mp4(
    path: &Path,
    representation_code: u8,
    ffprobe: &Path,
    require_global_sidx: bool,
    maximum_bytes: u64,
    ffprobe_timeout: Duration,
) -> Result<InspectedMedia, String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|_| "generated media file is missing".to_owned())?;
    let total_length = metadata.len();
    if total_length == 0 || total_length > maximum_bytes {
        return Err("generated media file was empty or exceeded its source/cache bound".to_owned());
    }
    let initialization_length = tokio::task::spawn_blocking({
        let path = path.to_path_buf();
        move || {
            let mut file = File::open(path)?;
            let mut prefix = vec![0; usize::try_from(total_length.min(1024 * 1024)).unwrap_or(0)];
            file.read_exact(&mut prefix)?;
            mp4_initialization_length(&prefix)
                .ok_or_else(|| std::io::Error::other("MP4 initialization was not parseable"))
        }
    })
    .await
    .map_err(|_| "MP4 initialization parser task failed".to_owned())?
    .map_err(|_| "MP4 initialization could not be parsed".to_owned())?;
    let ranges = tokio::task::spawn_blocking({
        let path = path.to_path_buf();
        move || mp4_fragment_ranges(&path, initialization_length, total_length)
    })
    .await
    .map_err(|_| "MP4 fragment parser task failed".to_owned())?
    .map_err(|_| "MP4 fragment ranges could not be parsed".to_owned())?;
    validate_fragment_ranges(initialization_length, total_length, &ranges)?;
    let target_duration = fixture_target_duration_seconds(&ranges)
        .ok_or_else(|| "generated media target duration was invalid".to_owned())?;
    let peak_bandwidth = peak_bandwidth_bits_per_second(&ranges, target_duration)
        .ok_or_else(|| "generated media fragment peak bandwidth was invalid".to_owned())?;
    let legacy_single_fragment_peak =
        legacy_single_fragment_peak_bandwidth_bits_per_second(&ranges).ok_or_else(|| {
            "generated media single-fragment peak bandwidth was invalid".to_owned()
        })?;
    let has_sidx = tokio::task::spawn_blocking({
        let path = path.to_path_buf();
        move || top_level_box_present(&path, *b"sidx")
    })
    .await
    .map_err(|_| "MP4 global index parser task failed".to_owned())?
    .map_err(|_| "MP4 top-level boxes could not be scanned".to_owned())?;
    if require_global_sidx && !has_sidx {
        return Err("FFmpeg fixture did not contain a global top-level sidx box".to_owned());
    }
    let progressive_index_count = if require_global_sidx {
        let path = path.to_path_buf();
        Some(
            tokio::task::spawn_blocking(move || {
                inspect_progressive_index_reference_count(
                    &path,
                    initialization_length,
                    total_length,
                )
            })
            .await
            .map_err(|_| "production SIDX inspection task failed".to_owned())??,
        )
    } else {
        None
    };
    let parsed = ffprobe_media(ffprobe, path, ffprobe_timeout).await?;
    let video = parsed
        .streams
        .iter()
        .find(|stream| stream.codec_type == "video");
    let duration_ms = ranges.iter().fold(0_u64, |sum, range| {
        sum.saturating_add(range.duration_millis)
    });
    Ok(InspectedMedia {
        path: path.to_path_buf(),
        record: FixtureRecord {
            representation_code,
            width: video.and_then(|stream| stream.width),
            height: video.and_then(|stream| stream.height),
            file_bytes: total_length,
            initialization_bytes: initialization_length,
            fragment_count: ranges.len() as u64,
            fragment_duration_ms: duration_ms,
            target_duration_seconds: target_duration,
            peak_bandwidth_bits_per_second: peak_bandwidth,
            legacy_single_fragment_peak_bandwidth_bits_per_second: legacy_single_fragment_peak,
            global_sidx_present: has_sidx,
            progressive_index_count,
            video_codec_code: video
                .and_then(|stream| stream.codec_name.as_deref())
                .map(video_codec_code)
                .unwrap_or_default(),
            video_profile_code: video
                .and_then(|stream| stream.profile.as_deref())
                .map(video_profile_code)
                .unwrap_or_default(),
            video_level: video.and_then(|stream| stream.level),
            audio_codec_code: parsed
                .streams
                .iter()
                .find(|stream| stream.codec_type == "audio")
                .and_then(|stream| stream.codec_name.as_deref())
                .map(audio_codec_code)
                .unwrap_or_default(),
        },
        fragments: ranges,
    })
}

fn inspect_progressive_index_reference_count(
    path: &Path,
    initialization_length: u64,
    total_length: u64,
) -> Result<u64, String> {
    let prefix_length = usize::try_from(total_length.min(1024 * 1024))
        .map_err(|_| "SIDX prefix length was not representable".to_owned())?;
    let mut file =
        File::open(path).map_err(|_| "generated SIDX media could not be opened".to_owned())?;
    let mut prefix = vec![0; prefix_length];
    file.read_exact(&mut prefix)
        .map_err(|_| "generated SIDX prefix could not be read within its bound".to_owned())?;
    let index = mp4_sidx_index(&prefix, initialization_length, total_length)
        .ok_or_else(|| "production SIDX parser rejected generated indexed media".to_owned())?;
    if index.trailing_length > MAX_SIDX_TRAILER_BYTES {
        return Err("generated SIDX trailer exceeded the production parser bound".to_owned());
    }
    let trailer_length = usize::try_from(index.trailing_length)
        .map_err(|_| "SIDX trailer length was not representable".to_owned())?;
    let mut trailer = vec![0; trailer_length];
    file.seek(SeekFrom::Start(index.trailing_offset))
        .and_then(|_| file.read_exact(&mut trailer))
        .map_err(|_| "generated SIDX trailer could not be read within its bound".to_owned())?;
    let ranges = mp4_sidx_segment_ranges(index, &trailer)
        .ok_or_else(|| "production SIDX range validation rejected generated media".to_owned())?;
    if ranges.len() < 2 {
        return Err("generated SIDX index contained fewer than two references".to_owned());
    }
    u64::try_from(ranges.len()).map_err(|_| "SIDX reference count was not representable".to_owned())
}

fn validate_fragment_ranges(
    initialization_length: u64,
    total_length: u64,
    ranges: &[Mp4SegmentRange],
) -> Result<(), String> {
    if ranges.len() < 2 {
        return Err("FFmpeg media did not contain at least two parsed MP4 fragments".to_owned());
    }
    let mut expected_offset = initialization_length;
    for range in ranges {
        if range.offset != expected_offset || range.length == 0 {
            return Err("parsed MP4 byte ranges were not contiguous".to_owned());
        }
        expected_offset = expected_offset
            .checked_add(range.length)
            .ok_or_else(|| "parsed MP4 range length overflowed".to_owned())?;
    }
    if expected_offset != total_length {
        return Err("parsed MP4 byte ranges did not cover the media file tail".to_owned());
    }
    Ok(())
}

fn top_level_box_present(path: &Path, target: [u8; 4]) -> std::io::Result<bool> {
    let mut file = File::open(path)?;
    let total_length = file.metadata()?.len();
    let mut offset = 0_u64;
    while offset.saturating_add(8) <= total_length {
        file.seek(SeekFrom::Start(offset))?;
        let mut header = [0_u8; 16];
        file.read_exact(&mut header[..8])?;
        let size32 = u32::from_be_bytes(header[..4].try_into().unwrap_or_default());
        let kind: [u8; 4] = header[4..8].try_into().unwrap_or_default();
        let (header_size, box_size) = match size32 {
            0 => return Ok(kind == target),
            1 => {
                if offset.saturating_add(16) > total_length {
                    return Ok(false);
                }
                file.read_exact(&mut header[8..16])?;
                (
                    16_u64,
                    u64::from_be_bytes(header[8..16].try_into().unwrap_or_default()),
                )
            }
            size => (8_u64, u64::from(size)),
        };
        if kind == target {
            return Ok(true);
        }
        if box_size < header_size || offset.saturating_add(box_size) > total_length {
            return Ok(false);
        }
        offset += box_size;
    }
    Ok(false)
}

async fn ffprobe_media(
    ffprobe: &Path,
    path: &Path,
    timeout: Duration,
) -> Result<FfprobeOutput, String> {
    let mut command = Command::new(ffprobe);
    command.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration:stream=codec_type,codec_name,profile,level,width,height",
        "-of",
        "json",
    ]);
    command.arg(path);
    let output = run_process(command, timeout, 32 * 1024).await?;
    if !output.success || output.stdout_truncated {
        return Err("ffprobe metadata inspection failed or exceeded its 32 KiB bound".to_owned());
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| "ffprobe returned invalid bounded metadata JSON".to_owned())
}

fn video_codec_code(codec: &str) -> u8 {
    match codec {
        "h264" => 1,
        "mpeg4" => 2,
        _ => 0,
    }
}

fn video_profile_code(profile: &str) -> u8 {
    match profile {
        "Constrained Baseline" => 1,
        "Baseline" => 2,
        "Main" => 3,
        "High" => 4,
        _ => 0,
    }
}

fn h264_baseline_level_3_is_coherent(
    advertised_codec: &str,
    ffprobe_profile: Option<&str>,
    ffprobe_level: Option<i32>,
    expected_profile: &str,
    expected_level: i32,
) -> bool {
    advertised_codec == H264_BASELINE_LEVEL_3_CODEC
        && ffprobe_profile == Some(expected_profile)
        && ffprobe_level == Some(expected_level)
}

fn source_h264_sidx_guard_applies(case: NativeCase) -> bool {
    case.uses_progressive_abr()
}

fn legacy_single_fragment_peak_bandwidth_bits_per_second(
    ranges: &[Mp4SegmentRange],
) -> Option<u64> {
    if ranges.is_empty() {
        return None;
    }
    ranges.iter().try_fold(0_u64, |peak, range| {
        if range.length == 0 || range.duration_millis == 0 {
            return None;
        }
        let numerator = range
            .length
            .checked_mul(8)?
            .checked_mul(1_000)?
            .checked_add(range.duration_millis.checked_sub(1)?)?;
        Some(peak.max(numerator / range.duration_millis))
    })
}

fn aggregate_peak_bandwidth(video_peak: u64, audio_peak: u64) -> Result<u64, String> {
    video_peak
        .checked_add(audio_peak)
        .filter(|peak| *peak > 0)
        .ok_or_else(|| "video and shared-audio peak bandwidth overflowed".to_owned())
}

fn expected_variant_peak_bandwidth(
    video_peak: u64,
    audio_peak: u64,
    muxed_audio: bool,
) -> Result<u64, String> {
    if muxed_audio {
        Ok(video_peak)
    } else {
        aggregate_peak_bandwidth(video_peak, audio_peak)
    }
}

fn master_bandwidth_matches_peak(advertised: u64, measured_peak: u64) -> bool {
    advertised > 0 && measured_peak > 0 && advertised == measured_peak
}

fn audio_codec_code(codec: &str) -> u8 {
    if codec == "aac" { 1 } else { 0 }
}

fn enforce_fixture_budget(case: NativeCase, fixture: &MediaFixture) -> Result<(), String> {
    let total = fixture
        .high_video
        .record
        .file_bytes
        .saturating_add(
            fixture
                .low_video
                .as_ref()
                .map(|media| media.record.file_bytes)
                .unwrap_or_default(),
        )
        .saturating_add(fixture.audio.record.file_bytes);
    let maximum = if case.uses_long_profile() {
        MAX_LONG_SOURCE_FIXTURE_BYTES
    } else {
        MAX_SOURCE_FIXTURE_BYTES
    };
    if total > maximum {
        return Err("generated source fixtures exceeded their profile aggregate bound".to_owned());
    }
    Ok(())
}

fn make_origin_state(
    case: NativeCase,
    fixture: &MediaFixture,
    address: SocketAddr,
    report: ReportStore,
) -> Result<Arc<OriginState>, String> {
    let mut media = HashMap::new();
    media.insert(
        "high.mp4".to_owned(),
        OriginMedia {
            path: fixture.high_video.path.clone(),
            representation_code: 1,
            initialization_length: fixture.high_video.record.initialization_bytes,
            fragments: fixture.high_video.fragments.clone(),
            content_type: "video/mp4",
        },
    );
    if let Some(low) = fixture.low_video.as_ref() {
        media.insert(
            "low.mp4".to_owned(),
            OriginMedia {
                path: low.path.clone(),
                representation_code: 2,
                initialization_length: low.record.initialization_bytes,
                fragments: low.fragments.clone(),
                content_type: "video/mp4",
            },
        );
    }
    media.insert(
        "audio.mp4".to_owned(),
        OriginMedia {
            path: fixture.audio.path.clone(),
            representation_code: 3,
            initialization_length: fixture.audio.record.initialization_bytes,
            fragments: fixture.audio.fragments.clone(),
            content_type: "audio/mp4",
        },
    );
    if !is_strict_loopback(address) {
        return Err("fixture origin was not bound to IPv4 loopback".to_owned());
    }
    if let Ok(mut data) = report.lock() {
        data.fault_timeline.enabled = case.uses_progressive_abr();
        if case.uses_progressive_abr() {
            data.fault_timeline.bad_high_status_code = Some(503);
        }
    }
    Ok(Arc::new(OriginState {
        media,
        case,
        phase: Arc::new(AtomicU8::new(PHASE_BASELINE)),
        epoch: Arc::new(Mutex::new(Instant::now())),
        report,
        pacers: std::array::from_fn(|_| RepresentationPacer::default()),
    }))
}

async fn origin_get(
    State(origin): State<Arc<OriginState>>,
    AxumPath(file_name): AxumPath<String>,
    headers: HeaderMap,
) -> Response<Body> {
    origin_response(origin, file_name, headers, false).await
}

async fn origin_head(
    State(origin): State<Arc<OriginState>>,
    AxumPath(file_name): AxumPath<String>,
    headers: HeaderMap,
) -> Response<Body> {
    origin_response(origin, file_name, headers, true).await
}

async fn origin_response(
    origin: Arc<OriginState>,
    file_name: String,
    headers: HeaderMap,
    head_only: bool,
) -> Response<Body> {
    let Some(media) = origin.media.get(&file_name) else {
        return empty_response(StatusCode::NOT_FOUND);
    };
    let total_length = match tokio::fs::metadata(&media.path).await {
        Ok(metadata) => metadata.len(),
        Err(_) => return empty_response(StatusCode::INTERNAL_SERVER_ERROR),
    };
    let range = match parse_byte_range(headers.get(RANGE), total_length) {
        Ok(range) => range,
        Err(()) => {
            let mut response = empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
            response.headers_mut().insert(
                CONTENT_RANGE,
                format!("bytes */{total_length}")
                    .parse()
                    .unwrap_or_else(|_| axum::http::HeaderValue::from_static("bytes */0")),
            );
            return response;
        }
    };
    let phase_code = origin.phase.load(Ordering::Acquire).min(PHASE_RECOVERY);
    let fault_applied = media.representation_code == 1 && phase_code == PHASE_BAD_HIGH;
    let status = if fault_applied {
        StatusCode::SERVICE_UNAVAILABLE
    } else if range.partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let touches_media_payload =
        range.start.saturating_add(range.length) > media.initialization_length;
    let media_window = media_window_for_byte_range(
        range,
        media.initialization_length,
        total_length,
        &media.fragments,
    );
    record_origin_request(
        &origin,
        media.representation_code,
        phase_code,
        range,
        status,
        !head_only && !fault_applied && touches_media_payload,
        media_window,
    );
    if fault_applied {
        return empty_response(StatusCode::SERVICE_UNAVAILABLE);
    }

    let mut builder = Response::builder()
        .status(status)
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_TYPE, media.content_type)
        .header(CONTENT_LENGTH, range.length.to_string())
        .header("cache-control", "no-store");
    if range.partial {
        let end = range.start.saturating_add(range.length).saturating_sub(1);
        builder = builder.header(
            CONTENT_RANGE,
            format!("bytes {}-{end}/{total_length}", range.start),
        );
    }
    if head_only {
        return builder
            .body(Body::empty())
            .unwrap_or_else(|_| empty_response(StatusCode::INTERNAL_SERVER_ERROR));
    }
    let mut file = match tokio::fs::File::open(&media.path).await {
        Ok(file) => file,
        Err(_) => return empty_response(StatusCode::INTERNAL_SERVER_ERROR),
    };
    if file.seek(SeekFrom::Start(range.start)).await.is_err() {
        return empty_response(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let stream = ReaderStream::new(file.take(range.length));
    let body = if pacing_rate_bits_per_second(origin.case, phase_code, media.representation_code)
        .is_some()
    {
        let pacer = origin.pacers[usize::from(media.representation_code - 1)].clone();
        let origin_for_stream = Arc::clone(&origin);
        let representation_code = media.representation_code;
        let stream = stream.then(move |chunk| {
            let origin = Arc::clone(&origin_for_stream);
            let pacer = pacer.clone();
            async move {
                if let Ok(bytes) = &chunk {
                    pacer
                        .pace_chunk(&origin, representation_code, bytes.len())
                        .await;
                }
                chunk
            }
        });
        Body::from_stream(stream)
    } else {
        Body::from_stream(stream)
    };
    builder
        .body(body)
        .unwrap_or_else(|_| empty_response(StatusCode::INTERNAL_SERVER_ERROR))
}

fn pacing_rate_bits_per_second(
    case: NativeCase,
    phase_code: u8,
    representation_code: u8,
) -> Option<u64> {
    if !case.uses_progressive_abr() || !matches!(representation_code, 1 | 2) {
        return None;
    }
    Some(match phase_code.min(PHASE_RECOVERY) {
        PHASE_BASELINE => ABR_BASELINE_RATE_BITS_PER_SECOND,
        PHASE_BAD_HIGH => ABR_BAD_RATE_BITS_PER_SECOND,
        _ => ABR_RECOVERY_RATE_BITS_PER_SECOND,
    })
}

fn chunk_transmission_duration(bytes: usize, bits_per_second: u64) -> Duration {
    let numerator = (bytes as u128).saturating_mul(8_000_000_000);
    let nanoseconds = numerator.div_ceil(u128::from(bits_per_second.max(1)));
    Duration::from_nanos(nanoseconds.min(u128::from(u64::MAX)) as u64)
}

impl PacingState {
    fn wait_time(&self, now: Instant) -> Duration {
        self.next_available_at
            .map(|available_at| available_at.saturating_duration_since(now))
            .unwrap_or_default()
    }

    fn try_reserve(&mut self, now: Instant, bytes: usize, bits_per_second: u64) -> bool {
        if self
            .next_available_at
            .is_some_and(|available_at| available_at > now)
        {
            return false;
        }
        self.next_available_at = Some(
            now.checked_add(chunk_transmission_duration(bytes, bits_per_second))
                .unwrap_or(now),
        );
        true
    }
}

impl RepresentationPacer {
    async fn pace_chunk(&self, origin: &OriginState, representation_code: u8, bytes: usize) {
        loop {
            let wait = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.wait_time(Instant::now())
            };
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
                continue;
            }

            let now = Instant::now();
            let reserved = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let phase_code = origin.phase.load(Ordering::Acquire).min(PHASE_RECOVERY);
                let Some(rate) =
                    pacing_rate_bits_per_second(origin.case, phase_code, representation_code)
                else {
                    return;
                };
                state.try_reserve(now, bytes, rate)
            };
            if reserved {
                return;
            }
        }
    }
}

fn parse_byte_range(value: Option<&axum::http::HeaderValue>, size: u64) -> Result<ByteRange, ()> {
    let Some(value) = value else {
        return Ok(ByteRange {
            start: 0,
            length: size,
            partial: false,
        });
    };
    let value = value.to_str().map_err(|_| ())?;
    let value = value.strip_prefix("bytes=").ok_or(())?;
    if value.contains(',') {
        return Err(());
    }
    let (start, end) = value.split_once('-').ok_or(())?;
    let (start, end) = if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| ())?.min(size);
        (
            size.checked_sub(suffix).ok_or(())?,
            size.checked_sub(1).ok_or(())?,
        )
    } else {
        let start = start.parse::<u64>().map_err(|_| ())?;
        let end = if end.is_empty() {
            size.checked_sub(1).ok_or(())?
        } else {
            end.parse::<u64>()
                .map_err(|_| ())?
                .min(size.saturating_sub(1))
        };
        (start, end)
    };
    if start >= size || end < start {
        return Err(());
    }
    Ok(ByteRange {
        start,
        length: end
            .checked_sub(start)
            .and_then(|n| n.checked_add(1))
            .ok_or(())?,
        partial: true,
    })
}

fn empty_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn record_origin_request(
    origin: &OriginState,
    representation_code: u8,
    phase_code: u8,
    range: ByteRange,
    status: StatusCode,
    media_payload: bool,
    media_window: Option<MediaWindow>,
) {
    let Ok(epoch) = origin.epoch.lock() else {
        return;
    };
    let elapsed_ms = Instant::now()
        .saturating_duration_since(*epoch)
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    drop(epoch);
    let Ok(mut report) = origin.report.lock() else {
        return;
    };
    let rep = usize::from(representation_code.saturating_sub(1).min(2));
    let phase = usize::from(phase_code.min(2));
    report.requests.total = report.requests.total.saturating_add(1);
    report.requests.count_by_representation[rep] =
        report.requests.count_by_representation[rep].saturating_add(1);
    report.requests.count_by_phase_and_representation[phase][rep] =
        report.requests.count_by_phase_and_representation[phase][rep].saturating_add(1);
    report
        .requests
        .last_request_elapsed_ms_by_phase_and_representation[phase][rep] = Some(elapsed_ms);
    if status.is_success() {
        report.requests.successful_by_representation[rep] =
            report.requests.successful_by_representation[rep].saturating_add(1);
        if media_payload {
            report.requests.media_payload_by_phase_and_representation[phase][rep] =
                report.requests.media_payload_by_phase_and_representation[phase][rep]
                    .saturating_add(1);
        }
    }
    if representation_code == 1
        && phase_code == PHASE_BAD_HIGH
        && status == StatusCode::SERVICE_UNAVAILABLE
    {
        report.requests.high_fault_status_count =
            report.requests.high_fault_status_count.saturating_add(1);
    }
    let event = RequestEvent {
        elapsed_ms,
        representation_code,
        phase_code,
        range_start: range.start,
        range_length: range.length,
        status_code: status.as_u16(),
    };
    if representation_code == 1
        && phase_code == PHASE_RECOVERY
        && report.requests.first_recovery_high_range_request.is_none()
    {
        report.requests.first_recovery_high_range_request = Some(RecoveryHighRangeRequest {
            elapsed_ms,
            monotonic_seconds: monotonic_clock_seconds().ok(),
            range_start: range.start,
            range_length: range.length,
            status_code: status.as_u16(),
            media_window,
        });
    }
    if representation_code == 1
        && phase_code == PHASE_RECOVERY
        && status == StatusCode::PARTIAL_CONTENT
        && media_payload
        && media_window.is_some()
        && report.requests.first_recovery_high_media_request.is_none()
    {
        report.requests.first_recovery_high_media_request = Some(RecoveryHighRangeRequest {
            elapsed_ms,
            monotonic_seconds: monotonic_clock_seconds().ok(),
            range_start: range.start,
            range_length: range.length,
            status_code: status.as_u16(),
            media_window,
        });
    }
    if report.requests.events.len() < 32 {
        report.requests.events.push(event);
    }
}

fn media_window_for_byte_range(
    requested: ByteRange,
    initialization_length: u64,
    total_length: u64,
    fragments: &[Mp4SegmentRange],
) -> Option<MediaWindow> {
    validate_fragment_ranges(initialization_length, total_length, fragments).ok()?;
    let request_end = requested.start.checked_add(requested.length)?;
    if requested.length == 0 || request_end > total_length {
        return None;
    }

    let mut elapsed_millis = 0_u64;
    let mut first_fragment = None;
    let mut last_fragment = None;
    let mut window_start_millis = 0_u64;
    let mut window_end_millis = 0_u64;
    for (index, fragment) in fragments.iter().enumerate() {
        let fragment_end = fragment.offset.checked_add(fragment.length)?;
        let fragment_index = u64::try_from(index).ok()?;
        let fragment_start_millis = elapsed_millis;
        elapsed_millis = elapsed_millis.checked_add(fragment.duration_millis)?;
        if requested.start < fragment_end && fragment.offset < request_end {
            if first_fragment.is_none() {
                first_fragment = Some(fragment_index);
                window_start_millis = fragment_start_millis;
            }
            last_fragment = Some(fragment_index);
            window_end_millis = elapsed_millis;
        }
    }

    Some(MediaWindow {
        start_seconds: window_start_millis as f64 / 1000.0,
        end_seconds: window_end_millis as f64 / 1000.0,
        first_fragment_index: first_fragment?,
        last_fragment_index: last_fragment?,
    })
}

impl OriginState {
    fn reset_probe_epoch(&self) {
        if let Ok(mut epoch) = self.epoch.lock() {
            *epoch = Instant::now();
        }
    }

    fn set_phase(&self, phase: u8) -> Result<(), String> {
        let elapsed_ms = self
            .epoch
            .lock()
            .map(|epoch| {
                Instant::now()
                    .saturating_duration_since(*epoch)
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64
            })
            .unwrap_or_default();
        let before_monotonic = match phase {
            PHASE_BAD_HIGH | PHASE_RECOVERY => Some(monotonic_clock_seconds()?),
            _ => None,
        };
        self.phase.store(phase, Ordering::Release);
        let after_monotonic = match phase {
            PHASE_BAD_HIGH | PHASE_RECOVERY => Some(monotonic_clock_seconds()?),
            _ => None,
        };
        if let Ok(mut report) = self.report.lock() {
            match phase {
                PHASE_BAD_HIGH => {
                    report.fault_timeline.bad_high_start_elapsed_ms = Some(elapsed_ms);
                    report.fault_timeline.bad_high_before_monotonic_seconds = before_monotonic;
                    report.fault_timeline.bad_high_after_monotonic_seconds = after_monotonic;
                }
                PHASE_RECOVERY => {
                    report.fault_timeline.bad_high_end_elapsed_ms = Some(elapsed_ms);
                    report.fault_timeline.recovery_start_elapsed_ms = Some(elapsed_ms);
                    report.fault_timeline.recovery_before_monotonic_seconds = before_monotonic;
                    report.fault_timeline.recovery_after_monotonic_seconds = after_monotonic;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

async fn run_abr_fault_timeline(
    origin: &Arc<OriginState>,
    probe: &mut AbortTask<Result<ProcessCapture, String>>,
    marker_path: &Path,
    report: &ReportStore,
    recovery_observation_path: &Path,
) -> Result<(), String> {
    let ready_marker = wait_for_decoder_ready_marker(marker_path, probe).await?;
    let baseline_start = monotonic_clock_seconds()?;
    if ready_marker.observed_monotonic_seconds > baseline_start {
        return Err("decoder readiness marker timestamp was after the baseline clock".to_owned());
    }
    let baseline_elapsed_ms = origin
        .epoch
        .lock()
        .map_err(|_| "origin probe epoch lock was poisoned".to_owned())?
        .elapsed()
        .as_millis();
    let baseline_elapsed_ms = u64::try_from(baseline_elapsed_ms).unwrap_or(u64::MAX);
    let high_payloads = report
        .lock()
        .map_err(|_| "acceptance report lock was poisoned".to_owned())?
        .requests
        .media_payload_by_phase_and_representation[0][0];
    if high_payloads == 0 {
        return Err("decoder readiness marker lacked a healthy high media payload".to_owned());
    }
    {
        let mut report_data = report
            .lock()
            .map_err(|_| "acceptance report lock was poisoned".to_owned())?;
        report_data.fault_timeline.decoder_ready_monotonic_seconds =
            Some(ready_marker.observed_monotonic_seconds);
        report_data.fault_timeline.baseline_start_monotonic_seconds = Some(baseline_start);
        report_data.fault_timeline.baseline_start_elapsed_ms = Some(baseline_elapsed_ms);
    }
    wait_while_probe_runs(origin, probe, BASELINE_WINDOW).await?;
    origin.set_phase(PHASE_BAD_HIGH)?;
    wait_while_probe_runs(origin, probe, BAD_HIGH_WINDOW).await?;
    origin.set_phase(PHASE_RECOVERY)?;
    create_empty_private_marker(recovery_observation_path)?;
    Ok(())
}

fn create_empty_private_marker(path: &Path) -> Result<(), String> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "fresh recovery observation marker could not be created".to_owned())?;
    let metadata = file
        .metadata()
        .map_err(|_| "recovery observation marker metadata could not be read".to_owned())?;
    if !metadata.is_file() || metadata.len() != 0 || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err("recovery observation marker was not empty, regular, and private".to_owned());
    }
    Ok(())
}

#[derive(Deserialize)]
struct DecoderReadyMarker {
    schema_version: u32,
    expected_width: u32,
    expected_height: u32,
    distinct_frame_count: u32,
    observed_monotonic_seconds: f64,
}

async fn wait_for_decoder_ready_marker(
    path: &Path,
    probe: &AbortTask<Result<ProcessCapture, String>>,
) -> Result<DecoderReadyMarker, String> {
    let started = Instant::now();
    loop {
        if probe.is_finished() {
            return Err("native probe exited before decoder readiness was marked".to_owned());
        }
        if let Some(marker) = read_decoder_ready_marker(path)? {
            if marker.schema_version != 1
                || marker.expected_width != 640
                || marker.expected_height != 360
                || marker.distinct_frame_count != 3
                || !marker.observed_monotonic_seconds.is_finite()
                || marker.observed_monotonic_seconds <= 0.0
            {
                return Err(
                    "decoder readiness marker fields did not match the fixture contract".to_owned(),
                );
            }
            return Ok(marker);
        }
        if started.elapsed() >= DECODER_READY_TIMEOUT {
            return Err(
                "native probe did not publish decoder readiness within 45 seconds".to_owned(),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn read_decoder_ready_marker(path: &Path) -> Result<Option<DecoderReadyMarker>, String> {
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(
                "decoder readiness marker could not be opened without following links".to_owned(),
            );
        }
    };
    let metadata = file
        .metadata()
        .map_err(|_| "decoder readiness marker metadata could not be read".to_owned())?;
    if !metadata.is_file()
        || metadata.len() > 2 * 1024
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err("decoder readiness marker was not a bounded private regular file".to_owned());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(2 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "decoder readiness marker could not be read within its bound".to_owned())?;
    if bytes.len() > 2 * 1024 {
        return Err("decoder readiness marker exceeded its 2 KiB read bound".to_owned());
    }
    match serde_json::from_slice(&bytes) {
        Ok(marker) => Ok(Some(marker)),
        Err(_) => Ok(None),
    }
}

async fn wait_while_probe_runs(
    origin: &OriginState,
    probe: &AbortTask<Result<ProcessCapture, String>>,
    duration: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + duration;
    loop {
        if probe.is_finished() {
            return Err("native probe ended before the planned recovery phase".to_owned());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let _ = origin;
            return Ok(());
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(100))).await;
    }
}

async fn run_native_probe(
    probe_path: PathBuf,
    master_url: String,
    duration: u64,
    deadline: u64,
    expected_sizes: String,
    marker_paths: ProbeMarkerPaths,
    minimum_steady_play_seconds: Option<u64>,
) -> Result<ProcessCapture, String> {
    ensure_loopback_url(&master_url)?;
    let mut command = Command::new(probe_path);
    let (arguments, recovery_snapshot) = native_probe_arguments(
        &master_url,
        duration,
        deadline,
        &expected_sizes,
        &marker_paths,
        minimum_steady_play_seconds,
    );
    command.args(arguments);
    if let Some(path) = recovery_snapshot {
        command.env(
            "LAN_PLAYBACK_RECOVERY_SNAPSHOT_PATH",
            path.to_string_lossy().as_ref(),
        );
    }
    run_process(
        command,
        Duration::from_secs(deadline) + PROBE_EXIT_GRACE,
        MAX_PROBE_JSON_BYTES,
    )
    .await
}

fn native_probe_arguments(
    master_url: &str,
    duration: u64,
    deadline: u64,
    expected_sizes: &str,
    marker_paths: &ProbeMarkerPaths,
    minimum_steady_play_seconds: Option<u64>,
) -> (Vec<String>, Option<PathBuf>) {
    let mut arguments = vec![
        "--url".to_owned(),
        master_url.to_owned(),
        "--duration".to_owned(),
        duration.to_string(),
        "--deadline".to_owned(),
        deadline.to_string(),
        "--expect-video-sizes".to_owned(),
        expected_sizes.to_owned(),
    ];
    if let Some(path) = &marker_paths.ready {
        arguments.extend([
            "--ready-marker".to_owned(),
            path.to_string_lossy().into_owned(),
            "--controls-after-video-size-expectations".to_owned(),
        ]);
    }
    if let Some(path) = &marker_paths.recovery_observation {
        arguments.extend([
            "--recovery-observation-trigger".to_owned(),
            path.to_string_lossy().into_owned(),
        ]);
    }
    if let Some(seconds) = minimum_steady_play_seconds {
        arguments.extend([
            "--minimum-steady-play-seconds".to_owned(),
            seconds.to_string(),
        ]);
    }
    (arguments, marker_paths.recovery_snapshot.clone())
}

#[cfg(test)]
mod prewarm_probe_contract_tests {
    use super::*;

    #[test]
    fn prewarm_probe_command_keeps_recovery_snapshot_without_steady_minimum() {
        let markers = ProbeMarkerPaths {
            ready: Some(PathBuf::from("/tmp/ready.json")),
            recovery_observation: Some(PathBuf::from("/tmp/recovery.trigger")),
            recovery_snapshot: Some(PathBuf::from("/tmp/recovery.json")),
        };
        let (arguments, snapshot) = native_probe_arguments(
            "http://127.0.0.1:12345/master.m3u8",
            600,
            720,
            "640x360,320x180,640x360",
            &markers,
            None,
        );
        assert_eq!(
            arguments,
            vec![
                "--url",
                "http://127.0.0.1:12345/master.m3u8",
                "--duration",
                "600",
                "--deadline",
                "720",
                "--expect-video-sizes",
                "640x360,320x180,640x360",
                "--ready-marker",
                "/tmp/ready.json",
                "--controls-after-video-size-expectations",
                "--recovery-observation-trigger",
                "/tmp/recovery.trigger",
            ]
        );
        assert_eq!(snapshot, Some(PathBuf::from("/tmp/recovery.json")));
        assert!(
            !arguments
                .iter()
                .any(|argument| { argument == "--minimum-steady-play-seconds" })
        );
    }
}

fn parse_probe_result(process: ProcessCapture, report: &ReportStore) -> Result<Value, String> {
    if process.stdout_truncated || process.stdout.len() > MAX_PROBE_JSON_BYTES {
        return Err("native probe JSON exceeded its 64 KiB bound".to_owned());
    }
    let mut value: Value = serde_json::from_slice(&process.stdout)
        .map_err(|_| "native probe did not return bounded JSON".to_owned())?;
    redact_probe_json(&mut value);
    if let Ok(mut data) = report.lock() {
        data.probe = Some(value.clone());
    }
    if !process.success {
        return Err(format!(
            "native probe exited with status {}; bounded_stderr_bytes={}; stderr_truncated={}",
            process
                .status_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_owned()),
            process.stderr_tail.len(),
            process.stderr_truncated
        ));
    }
    Ok(value)
}

fn validate_probe_result(
    case: NativeCase,
    requested_duration: u64,
    probe: &Value,
    report: &ReportStore,
) -> Result<(), String> {
    let report_data = report
        .lock()
        .map_err(|_| "acceptance report lock was poisoned".to_owned())?;
    validate_report_identity(
        case,
        &report_data.case_full_name,
        &report_data.scenario,
        report_data.source_duration_seconds,
    )?;
    drop(report_data);
    if let Ok(mut data) = report.lock() {
        data.probe = Some(probe.clone());
    }
    if probe.get("status").and_then(Value::as_str) != Some("completed") {
        return Err("native probe did not complete all AVPlayer acceptance checks".to_owned());
    }
    let asset_duration = probe
        .get("item_duration_seconds")
        .and_then(Value::as_f64)
        .filter(|duration| duration.is_finite() && *duration > 0.0)
        .ok_or_else(|| "native probe did not report finite positive asset duration".to_owned())?;
    if asset_duration < requested_duration as f64 {
        return Err("native HLS asset duration was shorter than the probe duration".to_owned());
    }
    if (asset_duration - f64::from(case.source_duration_seconds())).abs() > 0.25 {
        return Err("native HLS asset duration did not match its named source profile".to_owned());
    }
    if !all_probe_controls_completed(probe) {
        return Err("native probe did not complete every playback control".to_owned());
    }
    if matches!(case, NativeCase::Transcoding)
        && (probe.get("audio_track_present").and_then(Value::as_bool) != Some(true)
            || probe
                .get("enabled_audio_track_count")
                .and_then(Value::as_u64)
                .unwrap_or_default()
                == 0)
    {
        return Err("native playback did not provide AVPlayer audio-track evidence".to_owned());
    }
    let expected = match case {
        NativeCase::BufferedContinuity => &[(640, 360), (320, 180)][..],
        NativeCase::ProgressiveAbr | NativeCase::ExtendedDiagnostic => {
            &[(640, 360), (320, 180), (640, 360)][..]
        }
        NativeCase::SinglePlayerPrewarmControl | NativeCase::SinglePlayerPrewarmTreatment => {
            &[(640, 360), (320, 180), (640, 360)][..]
        }
        NativeCase::Transcoding => &[(640, 360)][..],
    };
    validate_video_size_expectations(probe, expected, case.is_prewarm_diagnostic())
}

fn validate_video_size_expectations(
    probe: &Value,
    expected: &[(u32, u32)],
    allow_optional_recovery_high: bool,
) -> Result<(), String> {
    let contract = probe
        .get("video_size_expectations")
        .ok_or_else(|| "native probe omitted ordered decoded-size expectations".to_owned())?;
    if contract.get("enabled").and_then(Value::as_bool) != Some(true)
        || (!allow_optional_recovery_high
            && contract.get("complete").and_then(Value::as_bool) != Some(true))
        || (allow_optional_recovery_high
            && contract.get("complete").and_then(Value::as_bool).is_none())
        || contract.get("semantics").and_then(Value::as_str)
            != Some("ordered_decoded_size_subsequence")
        || contract
            .get("minimum_distinct_display_timestamps_per_size")
            .and_then(Value::as_u64)
            != Some(3)
    {
        return Err("native probe ordered decoded-size contract was incomplete".to_owned());
    }
    let entries = contract
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| "native probe size expectation entries were missing".to_owned())?;
    if entries.len() != expected.len() {
        return Err("native probe returned the wrong number of decoded-size entries".to_owned());
    }
    for (index, (entry, (width, height))) in entries.iter().zip(expected).enumerate() {
        if allow_optional_recovery_high && index == 2 {
            let matched = entry.get("matched").and_then(Value::as_bool);
            let optional = entry
                .get("diagnostic_optional_recovery_high")
                .and_then(Value::as_bool)
                == Some(true);
            let samples = entry.get("samples").and_then(Value::as_array);
            if entry.get("expected_width").and_then(Value::as_u64) != Some(u64::from(*width))
                || entry.get("expected_height").and_then(Value::as_u64) != Some(u64::from(*height))
                || !optional
                || !matches!(samples, Some(samples) if samples.is_empty() || samples.len() == 3)
                || (matched == Some(false)
                    && (samples.is_none_or(|samples| !samples.is_empty())
                        || contract.get("complete").and_then(Value::as_bool) != Some(false)))
                || (matched == Some(true)
                    && (samples.is_none_or(|samples| samples.len() != 3)
                        || contract.get("complete").and_then(Value::as_bool) != Some(true)))
                || matched.is_none()
            {
                return Err(
                    "optional recovered-high evidence was neither valid nor explicitly censored"
                        .to_owned(),
                );
            }
            if matched == Some(false) {
                continue;
            }
        }
        if entry.get("expected_width").and_then(Value::as_u64) != Some(u64::from(*width))
            || entry.get("expected_height").and_then(Value::as_u64) != Some(u64::from(*height))
            || entry.get("matched").and_then(Value::as_bool) != Some(true)
            || entry
                .get("distinct_frame_count")
                .and_then(Value::as_u64)
                .unwrap_or_default()
                < 3
            || entry
                .get("required_distinct_frame_count")
                .and_then(Value::as_u64)
                != Some(3)
            || entry
                .get("samples")
                .and_then(Value::as_array)
                .is_none_or(|samples| {
                    samples.len() != 3
                        || samples.iter().take(3).any(|sample| {
                            sample
                                .get("observed_elapsed_seconds")
                                .and_then(Value::as_f64)
                                .is_none_or(|elapsed| !elapsed.is_finite() || elapsed < 0.0)
                                || sample
                                    .get("observed_monotonic_seconds")
                                    .and_then(Value::as_f64)
                                    .is_none_or(|timestamp| {
                                        !timestamp.is_finite() || timestamp <= 0.0
                                    })
                        })
                        || samples
                            .iter()
                            .take(3)
                            .filter_map(|sample| {
                                sample
                                    .get("observed_monotonic_seconds")
                                    .and_then(Value::as_f64)
                            })
                            .collect::<Vec<_>>()
                            .windows(2)
                            .any(|pair| pair[0] > pair[1])
                })
        {
            return Err(format!(
                "native decoded pixels did not match expected size {width}x{height}"
            ));
        }
    }
    Ok(())
}

fn validate_report_identity(
    case: NativeCase,
    case_full_name: &str,
    scenario: &str,
    source_duration_seconds: u32,
) -> Result<(), String> {
    if case_full_name != case.full_name()
        || scenario != case.scenario()
        || source_duration_seconds != case.source_duration_seconds()
    {
        return Err("acceptance report identity did not match its named scenario".to_owned());
    }
    Ok(())
}

fn validate_recovery_observations(
    case: NativeCase,
    probe: &Value,
    report: &ReportStore,
) -> Result<(), String> {
    let report_data = report
        .lock()
        .map_err(|_| "acceptance report lock was poisoned".to_owned())?;
    validate_report_identity(
        case,
        &report_data.case_full_name,
        &report_data.scenario,
        report_data.source_duration_seconds,
    )?;
    let source_duration = f64::from(case.source_duration_seconds());
    let probe_duration = probe
        .get("item_duration_seconds")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| "probe source duration was missing or invalid".to_owned())?;
    if (probe_duration - source_duration).abs() > 0.25 {
        return Err("probe source duration did not match the named case".to_owned());
    }
    let recovery_after = required_clock_bound(
        report_data.fault_timeline.recovery_after_monotonic_seconds,
        "recovery post-transition clock was not recorded",
    )?;
    drop(report_data);

    let recovery_observation = probe
        .get("recovery_observation")
        .ok_or_else(|| "probe recovery observation was missing".to_owned())?;
    let recovery_state = validate_player_observation(recovery_observation, source_duration)?;
    if recovery_state.observed_monotonic_seconds < recovery_after
        || recovery_state.observed_monotonic_seconds > recovery_after + 5.0
    {
        return Err("recovery observation was outside the controller phase window".to_owned());
    }
    match case {
        NativeCase::ProgressiveAbr
        | NativeCase::ExtendedDiagnostic
        | NativeCase::SinglePlayerPrewarmControl
        | NativeCase::SinglePlayerPrewarmTreatment => {
            if recovery_state.range_end > source_duration - 20.0 {
                return Err(
                    "recovery observation did not prove 20 seconds of future unbuffered media"
                        .to_owned(),
                );
            }
        }
        NativeCase::BufferedContinuity => {
            let steady_play_elapsed = probe
                .get("steps")
                .and_then(Value::as_array)
                .and_then(|steps| {
                    steps.iter().find(|step| {
                        step.get("name").and_then(Value::as_str) == Some("play")
                            && step.get("status").and_then(Value::as_str) == Some("completed")
                    })
                })
                .and_then(|step| step.get("elapsed_seconds"))
                .and_then(Value::as_f64)
                .filter(|seconds| {
                    seconds.is_finite() && *seconds >= MINIMUM_STEADY_PLAY_SECONDS as f64
                })
                .ok_or_else(|| {
                    "buffered case did not prove 110 seconds of steady playback".to_owned()
                })?;
            let _ = steady_play_elapsed;
            let steady_end = probe
                .get("steady_play_end_observation")
                .ok_or_else(|| "steady-play end observation was missing".to_owned())?;
            let steady_state = validate_player_observation(steady_end, source_duration)?;
            if steady_state.observed_monotonic_seconds < recovery_state.observed_monotonic_seconds
                || steady_state.observed_monotonic_seconds < recovery_after
                || steady_state.range_end < source_duration - 0.25
            {
                return Err(
                    "steady playback did not retain loaded coverage through the source end"
                        .to_owned(),
                );
            }
        }
        NativeCase::Transcoding => {
            return Err(
                "transcoding case does not use progressive recovery observations".to_owned(),
            );
        }
    }
    Ok(())
}

struct PlayerObservationEvidence {
    observed_monotonic_seconds: f64,
    range_end: f64,
}

fn validate_player_observation(
    observation: &Value,
    source_duration: f64,
) -> Result<PlayerObservationEvidence, String> {
    let observed_monotonic_seconds = observation
        .get("observed_monotonic_seconds")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| "passive observation timestamp was missing or invalid".to_owned())?;
    let player_state = observation
        .get("player_state")
        .ok_or_else(|| "passive observation player state was missing".to_owned())?;
    let playhead = player_state
        .get("playhead_seconds")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0 && *value <= source_duration)
        .ok_or_else(|| "passive observation playhead was outside the source".to_owned())?;
    player_state
        .get("preferred_forward_buffer_duration_seconds")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value == 0.0)
        .ok_or_else(|| {
            "passive observation did not retain default AVPlayer buffering".to_owned()
        })?;
    let ranges = player_state
        .get("loaded_time_ranges")
        .filter(|value| value.is_object())
        .ok_or_else(|| "passive observation loaded ranges were missing".to_owned())?;
    let observations = ranges
        .get("observations")
        .and_then(Value::as_array)
        .ok_or_else(|| "passive observation loaded ranges were incomplete".to_owned())?;
    let source_count = ranges.get("source_count").and_then(Value::as_u64);
    let valid_count = ranges.get("valid_count").and_then(Value::as_u64);
    let invalid_count = ranges.get("invalid_count").and_then(Value::as_u64);
    if ranges.get("truncated").and_then(Value::as_bool) != Some(false)
        || observations.is_empty()
        || observations.len() > 8
        || source_count != Some(observations.len() as u64)
        || valid_count != source_count
        || invalid_count != Some(0)
    {
        return Err("passive observation loaded ranges were truncated or invalid".to_owned());
    }

    let mut containing_range_end = None::<f64>;
    for range in observations {
        let start = finite_nonnegative(range.get("start_seconds"))
            .ok_or_else(|| "loaded range start was missing or invalid".to_owned())?;
        let duration = finite_nonnegative(range.get("duration_seconds"))
            .ok_or_else(|| "loaded range duration was missing or invalid".to_owned())?;
        let end = finite_nonnegative(range.get("end_seconds"))
            .ok_or_else(|| "loaded range end was missing or invalid".to_owned())?;
        if (start + duration - end).abs() > 0.001
            || end > source_duration
            || start > source_duration
        {
            return Err("loaded range was inconsistent or extended beyond the source".to_owned());
        }
        if start <= playhead && playhead <= end {
            containing_range_end = Some(containing_range_end.unwrap_or_default().max(end));
        }
    }
    let range_end = containing_range_end
        .ok_or_else(|| "no loaded range containing the playhead was observed".to_owned())?;
    Ok(PlayerObservationEvidence {
        observed_monotonic_seconds,
        range_end,
    })
}

fn finite_nonnegative(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite() && *number >= 0.0)
}

fn all_probe_controls_completed(probe: &Value) -> bool {
    let Some(steps) = probe.get("steps").and_then(Value::as_array) else {
        return false;
    };
    let required = [
        "play",
        "pause",
        "resume",
        "seek_forward",
        "seek_forward_play",
        "seek_back",
        "seek_back_play",
        "rate_1_25",
        "duration_remainder",
    ];
    required.iter().all(|name| {
        steps.iter().any(|step| {
            step.get("name").and_then(Value::as_str) == Some(name)
                && step.get("status").and_then(Value::as_str) == Some("completed")
        })
    })
}

fn validate_abr_request_trace(report: &ReportStore, case: NativeCase) -> Result<(), String> {
    let data = report
        .lock()
        .map_err(|_| "acceptance report lock was poisoned".to_owned())?;
    let requests = &data.requests;
    if requests.media_payload_by_phase_and_representation[0][0] == 0
        || requests.count_by_phase_and_representation[1][0] == 0
        || requests.high_fault_status_count == 0
        || requests.count_by_phase_and_representation[1][1] == 0
        || requests.successful_by_representation[1] == 0
        || (matches!(
            case,
            NativeCase::ProgressiveAbr | NativeCase::ExtendedDiagnostic
        ) && requests.media_payload_by_phase_and_representation[2][0] == 0)
    {
        return Err(
            "origin trace did not prove healthy high, bad-high with healthy low, and recovered high requests"
                .to_owned(),
        );
    }
    Ok(())
}

fn validate_abr_sample_timeline(
    case: NativeCase,
    probe: &Value,
    report: &ReportStore,
) -> Result<(), String> {
    let timeline = report
        .lock()
        .map_err(|_| "acceptance report lock was poisoned".to_owned())?
        .fault_timeline
        .clone();
    let decoder_ready = required_clock_bound(
        timeline.decoder_ready_monotonic_seconds,
        "decoder-ready marker clock was not recorded",
    )?;
    let baseline_start = required_clock_bound(
        timeline.baseline_start_monotonic_seconds,
        "post-decoder healthy-baseline clock was not recorded",
    )?;
    let bad_high_before = required_clock_bound(
        timeline.bad_high_before_monotonic_seconds,
        "bad-high pre-transition clock bound was not recorded",
    )?;
    let bad_high_after = required_clock_bound(
        timeline.bad_high_after_monotonic_seconds,
        "bad-high post-transition clock bound was not recorded",
    )?;
    let recovery_before = required_clock_bound(
        timeline.recovery_before_monotonic_seconds,
        "recovery pre-transition clock bound was not recorded",
    )?;
    let recovery_after = required_clock_bound(
        timeline.recovery_after_monotonic_seconds,
        "recovery post-transition clock bound was not recorded",
    )?;
    if decoder_ready > baseline_start
        || baseline_start + BASELINE_WINDOW.as_secs_f64() > bad_high_before
    {
        return Err(
            "healthy high baseline did not start after decoder readiness and last six seconds"
                .to_owned(),
        );
    }
    let entries = probe["video_size_expectations"]["entries"]
        .as_array()
        .ok_or_else(|| "decoded-size sample entries were missing".to_owned())?;
    let samples = |index: usize| -> Result<Vec<f64>, String> {
        if case.is_prewarm_diagnostic()
            && index == 2
            && entries[index]["matched"].as_bool() == Some(false)
        {
            return Ok(Vec::new());
        }
        entries[index]["samples"]
            .as_array()
            .filter(|samples| samples.len() == 3)
            .ok_or_else(|| "decoded-size monotonic samples were missing".to_owned())?
            .iter()
            .map(|sample| {
                sample["observed_monotonic_seconds"]
                    .as_f64()
                    .filter(|timestamp| timestamp.is_finite() && *timestamp > 0.0)
                    .ok_or_else(|| "decoded-size monotonic sample was not numeric".to_owned())
            })
            .collect()
    };
    let initial_high = samples(0)?;
    let low = samples(1)?;
    let recovered_high = match entries.len() {
        2 => Vec::new(),
        3 => samples(2)?,
        _ => return Err("unexpected number of progressive decoded-size stages".to_owned()),
    };
    validate_abr_sample_timestamps(
        &initial_high,
        &low,
        &recovered_high,
        bad_high_before,
        bad_high_after,
        recovery_before,
        recovery_after,
    )
}

fn required_clock_bound(value: Option<f64>, error: &str) -> Result<f64, String> {
    value
        .filter(|timestamp| timestamp.is_finite() && *timestamp > 0.0)
        .ok_or_else(|| error.to_owned())
}

fn monotonic_clock_seconds() -> Result<f64, String> {
    let mut timestamp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // CLOCK_MONOTONIC is shared across processes on this host.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut timestamp) } != 0
        || timestamp.tv_sec < 0
        || !(0..1_000_000_000).contains(&timestamp.tv_nsec)
    {
        return Err("native monotonic clock could not be read".to_owned());
    }
    let seconds = timestamp.tv_sec as f64 + timestamp.tv_nsec as f64 / 1_000_000_000.0;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err("native monotonic clock returned an invalid timestamp".to_owned());
    }
    Ok(seconds)
}

fn validate_abr_sample_timestamps(
    initial_high: &[f64],
    low: &[f64],
    recovered_high: &[f64],
    bad_high_before: f64,
    bad_high_after: f64,
    recovery_before: f64,
    recovery_after: f64,
) -> Result<(), String> {
    let timestamps_are_valid = |values: &[f64]| {
        values.len() == 3
            && values.iter().all(|value| value.is_finite() && *value > 0.0)
            && values.windows(2).all(|pair| pair[0] <= pair[1])
    };
    if !(timestamps_are_valid(initial_high)
        && timestamps_are_valid(low)
        && (recovered_high.is_empty() || timestamps_are_valid(recovered_high))
        && [
            bad_high_before,
            bad_high_after,
            recovery_before,
            recovery_after,
        ]
        .into_iter()
        .all(|value| value.is_finite() && value > 0.0)
        && bad_high_before <= bad_high_after
        && bad_high_after <= recovery_before
        && recovery_before <= recovery_after)
    {
        return Err("decoded-size monotonic timestamps or fault bounds were invalid".to_owned());
    }
    if initial_high
        .iter()
        .copied()
        .any(|timestamp| timestamp >= bad_high_before)
    {
        return Err(
            "initial high decoded frames were not observed before the bad-high gate".to_owned(),
        );
    }
    if low
        .iter()
        .copied()
        .any(|timestamp| timestamp < bad_high_after || timestamp >= recovery_before)
    {
        return Err("low decoded frames were not observed inside the bad-high interval".to_owned());
    }
    if !recovered_high.is_empty()
        && recovered_high
            .iter()
            .copied()
            .any(|timestamp| timestamp < recovery_after)
    {
        return Err("final high decoded frames were not observed after high recovery".to_owned());
    }
    Ok(())
}

fn redact_probe_json(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                let key_lower = key.to_ascii_lowercase();
                if ["url", "path", "cookie", "token", "header"]
                    .iter()
                    .any(|marker| key_lower.contains(marker))
                {
                    *value = Value::String("[redacted]".to_owned());
                } else {
                    redact_probe_json(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_probe_json(value);
            }
        }
        Value::String(string)
            if string.starts_with("http://") || string.starts_with("https://") =>
        {
            *string = "[redacted]".to_owned();
        }
        _ => {}
    }
}

async fn validate_master_playlist(
    client: &reqwest::Client,
    url: &str,
    case: NativeCase,
    expected: PlaylistExpectations,
    long_profile: bool,
) -> Result<HlsPlaylistPreflight, String> {
    ensure_loopback_url(url)?;
    let master =
        url::Url::parse(url).map_err(|_| "production HLS master URL was invalid".to_owned())?;
    let playlist_limit = playlist_response_limit(long_profile);
    let body = fetch_playlist(client, &master, playlist_limit).await?;
    if !body.starts_with("#EXTM3U") || !body.contains("#EXT-X-STREAM-INF:") {
        return Err("production media route returned an invalid HLS master playlist".to_owned());
    }
    if case.uses_progressive_abr()
        && (!body.contains("RESOLUTION=640x360") || !body.contains("RESOLUTION=320x180"))
    {
        return Err(
            "production master playlist did not advertise both real ABR representations".to_owned(),
        );
    }
    let SelectedHlsPlaylists {
        video_uri,
        audio_uri,
        bandwidth: advertised_bandwidth,
    } = selected_high_playlist_uris(&body)?;
    if source_h264_sidx_guard_applies(case) && audio_uri.is_none() {
        return Err("ABR source master omitted its separate audio playlist".to_owned());
    }
    let expected_variant_peak = expected_variant_peak_bandwidth(
        expected.video_peak_bandwidth,
        expected.audio_peak_bandwidth,
        audio_uri.is_none(),
    )?;
    if !master_bandwidth_matches_peak(advertised_bandwidth, expected_variant_peak) {
        return Err(format!(
            "production HLS master bandwidth mismatch: advertised={} canonical={expected_variant_peak}",
            advertised_bandwidth
        ));
    }
    let video_url = resolve_same_origin_playlist(&master, &video_uri)?;
    let (video_fragment_count, video_target_duration_seconds) = fetch_and_validate_media_playlist(
        client,
        &video_url,
        expected.video_fragments,
        playlist_limit,
    )
    .await?;
    if video_target_duration_seconds != expected.video_target_duration {
        return Err(
            "production video playlist target duration did not match its fixture".to_owned(),
        );
    }
    let (audio_fragment_count, audio_target_duration_seconds, external_audio_peak) =
        if let Some(audio_uri) = audio_uri {
            let audio_url = resolve_same_origin_playlist(&master, &audio_uri)?;
            let (count, target) = fetch_and_validate_media_playlist(
                client,
                &audio_url,
                expected.audio_fragments,
                playlist_limit,
            )
            .await?;
            if target != expected.audio_target_duration {
                return Err(
                    "production audio playlist target duration did not match its fixture"
                        .to_owned(),
                );
            }
            (count, target, expected.audio_peak_bandwidth)
        } else {
            (0, 0, 0)
        };
    Ok(HlsPlaylistPreflight {
        bandwidth_semantics: "rfc8216_contiguous_duration_window",
        high_video_fragment_count: video_fragment_count,
        high_video_target_duration_seconds: video_target_duration_seconds,
        audio_fragment_count,
        audio_target_duration_seconds,
        high_video_peak_bandwidth_bits_per_second: expected.video_peak_bandwidth,
        audio_peak_bandwidth_bits_per_second: external_audio_peak,
        high_variant_peak_bandwidth_bits_per_second: expected_variant_peak,
        advertised_high_bandwidth_bits_per_second: advertised_bandwidth,
    })
}

fn playlist_response_limit(long_profile: bool) -> usize {
    if long_profile { 512 * 1024 } else { 64 * 1024 }
}

async fn fetch_playlist(
    client: &reqwest::Client,
    url: &url::Url,
    maximum_bytes: usize,
) -> Result<String, String> {
    ensure_loopback_url(url.as_str())?;
    let mut response = timeout(Duration::from_secs(5), client.get(url.clone()).send())
        .await
        .map_err(|_| "production HLS playlist request timed out".to_owned())?
        .map_err(|_| "production HLS playlist request failed".to_owned())?;
    if response.status() != StatusCode::OK {
        return Err("production media router did not serve the selected HLS playlist".to_owned());
    }
    if response
        .content_length()
        .is_some_and(|length| length > maximum_bytes as u64)
    {
        return Err("production HLS playlist exceeded its response bound".to_owned());
    }
    let bytes = timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        loop {
            let chunk = response.chunk().await.map_err(|_| ())?;
            let Some(chunk) = chunk else {
                break;
            };
            if bytes.len().saturating_add(chunk.len()) > maximum_bytes {
                return Err(());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok::<_, ()>(bytes)
    })
    .await
    .map_err(|_| "production HLS playlist body timed out".to_owned())?
    .map_err(|_| "production HLS playlist body exceeded its response bound".to_owned())?;
    String::from_utf8(bytes).map_err(|_| "production HLS playlist was not valid UTF-8".to_owned())
}

struct SelectedHlsPlaylists {
    video_uri: String,
    audio_uri: Option<String>,
    bandwidth: u64,
}

fn selected_high_playlist_uris(master: &str) -> Result<SelectedHlsPlaylists, String> {
    let lines = master.lines().map(str::trim).collect::<Vec<_>>();
    let mut selected: Option<SelectedHlsPlaylists> = None;
    for (index, line) in lines.iter().enumerate() {
        let Some(attributes) = line.strip_prefix("#EXT-X-STREAM-INF:") else {
            continue;
        };
        let attributes = parse_hls_attributes(attributes)
            .ok_or_else(|| "production HLS master attributes were invalid".to_owned())?;
        if attributes.get("RESOLUTION").map(String::as_str) != Some("640x360") {
            continue;
        }
        let bandwidth = attributes
            .get("BANDWIDTH")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| "selected high HLS variant bandwidth was invalid".to_owned())?;
        let video_uri = lines[index + 1..]
            .iter()
            .find(|entry| !entry.is_empty() && !entry.starts_with('#'))
            .ok_or_else(|| "production HLS master omitted the selected video URI".to_owned())?;
        let audio_group = attributes.get("AUDIO");
        let mut audio_uris = Vec::new();
        for line in &lines {
            let Some(attributes) = line.strip_prefix("#EXT-X-MEDIA:") else {
                continue;
            };
            let attributes = parse_hls_attributes(attributes)
                .ok_or_else(|| "production HLS rendition attributes were invalid".to_owned())?;
            if attributes.get("TYPE").map(String::as_str) == Some("AUDIO")
                && audio_group.is_some()
                && attributes.get("GROUP-ID") == audio_group
                && let Some(uri) = attributes.get("URI")
            {
                audio_uris.push(uri.clone());
            }
        }
        if (audio_group.is_some() && audio_uris.len() != 1) || selected.is_some() {
            return Err(
                "production HLS master did not identify one high video/audio pair".to_owned(),
            );
        }
        selected = Some(SelectedHlsPlaylists {
            video_uri: (*video_uri).to_owned(),
            audio_uri: audio_uris.pop(),
            bandwidth,
        });
    }
    selected
        .ok_or_else(|| "production HLS master omitted the selected high representation".to_owned())
}

fn parse_hls_attributes(value: &str) -> Option<HashMap<String, String>> {
    let mut entries = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (index, character) in value.char_indices() {
        if character == '"' {
            quoted = !quoted;
        } else if character == ',' && !quoted {
            entries.push(&value[start..index]);
            start = index + character.len_utf8();
        }
    }
    if quoted {
        return None;
    }
    entries.push(&value[start..]);
    let mut attributes = HashMap::new();
    for entry in entries {
        let (key, raw_value) = entry.split_once('=')?;
        let key = key.trim();
        let raw_value = raw_value.trim();
        if key.is_empty() || raw_value.is_empty() || attributes.contains_key(key) {
            return None;
        }
        let value = if raw_value.starts_with('"') && raw_value.ends_with('"') {
            raw_value.get(1..raw_value.len().checked_sub(1)?)?
        } else if raw_value.contains('"') {
            return None;
        } else {
            raw_value
        };
        attributes.insert(key.to_owned(), value.to_owned());
    }
    Some(attributes)
}

fn resolve_same_origin_playlist(master: &url::Url, uri: &str) -> Result<url::Url, String> {
    let resolved = master
        .join(uri)
        .map_err(|_| "selected HLS playlist URI could not be resolved".to_owned())?;
    if resolved.origin() != master.origin()
        || resolved.username() != ""
        || resolved.password().is_some()
        || resolved.fragment().is_some()
        || !resolved.path().starts_with('/')
    {
        return Err("selected HLS playlist URI was not same-origin".to_owned());
    }
    ensure_loopback_url(resolved.as_str())?;
    Ok(resolved)
}

async fn fetch_and_validate_media_playlist(
    client: &reqwest::Client,
    url: &url::Url,
    expected_fragments: u64,
    maximum_bytes: usize,
) -> Result<(u64, u64), String> {
    let body = fetch_playlist(client, url, maximum_bytes).await?;
    let target_duration = validate_media_playlist_body(url, &body, expected_fragments)?;
    Ok((expected_fragments, target_duration))
}

fn validate_media_playlist_body(
    playlist_url: &url::Url,
    body: &str,
    expected_fragments: u64,
) -> Result<u64, String> {
    let lines = body.lines().map(str::trim).collect::<Vec<_>>();
    if lines.first().copied() != Some("#EXTM3U") {
        return Err("selected production media playlist was invalid".to_owned());
    }
    let target_durations = lines
        .iter()
        .filter_map(|line| line.strip_prefix("#EXT-X-TARGETDURATION:"))
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "selected production target duration was invalid".to_owned())?;
    if target_durations.len() != 1 || target_durations[0] == 0 {
        return Err("selected production media playlist omitted its target duration".to_owned());
    }
    let map_lines = lines
        .iter()
        .filter_map(|line| line.strip_prefix("#EXT-X-MAP:"))
        .collect::<Vec<_>>();
    let map_attributes = map_lines
        .first()
        .and_then(|line| parse_hls_attributes(line));
    let map_uri = map_attributes
        .as_ref()
        .and_then(|attributes| attributes.get("URI"));
    let map_range = map_attributes
        .as_ref()
        .and_then(|attributes| attributes.get("BYTERANGE"))
        .and_then(|range| parse_hls_byte_range(range));
    let map_resource = map_uri
        .zip(map_range)
        .and_then(|(uri, _range)| resolve_playlist_resource(playlist_url, uri, None, None).ok());
    if map_lines.len() != 1 || map_resource.is_none() {
        return Err(
            "selected production media playlist omitted a valid initialization map".to_owned(),
        );
    }
    let map_resource = map_resource.expect("validated map resource is present");
    let mut fragment_count = 0_u64;
    let mut index = 0;
    while index < lines.len() {
        let Some(duration) = lines[index].strip_prefix("#EXTINF:") else {
            index += 1;
            continue;
        };
        let (duration, _) = duration
            .split_once(',')
            .ok_or_else(|| "selected production segment duration was malformed".to_owned())?;
        let duration = duration
            .parse::<f64>()
            .map_err(|_| "selected production segment duration was invalid".to_owned())?;
        if !duration.is_finite() || duration <= 0.0 {
            return Err("selected production segment duration was invalid".to_owned());
        }
        let range = lines
            .get(index + 1)
            .and_then(|line| line.strip_prefix("#EXT-X-BYTERANGE:"))
            .ok_or_else(|| "selected production segment omitted its byte range".to_owned())?;
        let Some(byte_range) = parse_hls_byte_range(range) else {
            return Err(
                "selected production media playlist declared an invalid byte range".to_owned(),
            );
        };
        let segment_uri = lines.get(index + 2).copied().unwrap_or_default();
        if segment_uri.is_empty()
            || segment_uri.starts_with('#')
            || resolve_playlist_resource(
                playlist_url,
                segment_uri,
                Some(map_resource.path()),
                Some(byte_range),
            )
            .is_err()
        {
            return Err("selected production segment byte range or URI was invalid".to_owned());
        }
        fragment_count = fragment_count.saturating_add(1);
        index += 3;
    }
    if fragment_count < 2 || fragment_count != expected_fragments {
        return Err(
            "selected production playlist fragment count did not match its source".to_owned(),
        );
    }
    if lines
        .iter()
        .filter(|line| line.starts_with("#EXT-X-BYTERANGE:"))
        .count()
        != usize::try_from(fragment_count).unwrap_or(usize::MAX)
        || lines
            .iter()
            .filter(|line| **line == "#EXT-X-ENDLIST")
            .count()
            != 1
    {
        return Err("selected production media playlist was incomplete".to_owned());
    }
    Ok(target_durations[0])
}

fn valid_hls_byte_range(value: &str) -> bool {
    parse_hls_byte_range(value).is_some()
}

fn parse_hls_byte_range(value: &str) -> Option<(u64, u64)> {
    let (length, offset) = value.split_once('@').unwrap_or((value, "0"));
    let length = length.parse::<u64>().ok().filter(|length| *length > 0)?;
    let offset = offset.parse::<u64>().ok()?;
    Some((offset, length))
}

fn resolve_playlist_resource(
    playlist_url: &url::Url,
    uri: &str,
    authorized_path: Option<&str>,
    expected_range: Option<(u64, u64)>,
) -> Result<url::Url, String> {
    let resolved = playlist_url
        .join(uri)
        .map_err(|_| "selected HLS resource URI could not be resolved".to_owned())?;
    if resolved.origin() != playlist_url.origin()
        || resolved.username() != ""
        || resolved.password().is_some()
        || resolved.fragment().is_some()
        || !resolved.path().starts_with('/')
        || authorized_path.is_some_and(|path| path != resolved.path())
    {
        return Err("selected HLS resource URI was not authorized".to_owned());
    }
    if let Some(query) = resolved.query() {
        let expected = expected_range
            .map(|(offset, length)| format!("segment={offset}-{length}"))
            .ok_or_else(|| "initialization resource URI contained a query".to_owned())?;
        let mut pairs = resolved.query_pairs();
        let matches_expected = pairs
            .next()
            .is_some_and(|(name, value)| format!("{name}={value}") == expected);
        if !matches_expected || pairs.next().is_some() || query.is_empty() {
            return Err("selected HLS segment query did not match its byte range".to_owned());
        }
    }
    ensure_loopback_url(resolved.as_str())?;
    Ok(resolved)
}

async fn run_process(
    mut command: Command,
    deadline: Duration,
    max_stdout_bytes: usize,
) -> Result<ProcessCapture, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| "required local acceptance child process could not be started".to_owned())?;
    capture_child(&mut child, deadline, max_stdout_bytes).await
}

async fn capture_child(
    child: &mut Child,
    deadline: Duration,
    max_stdout_bytes: usize,
) -> Result<ProcessCapture, String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "acceptance child stdout pipe was unavailable".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "acceptance child stderr pipe was unavailable".to_owned())?;
    let stdout_task = tokio::spawn(capture_stream(stdout, max_stdout_bytes, false));
    let stderr_task = tokio::spawn(capture_stream(stderr, MAX_CHILD_STDERR_BYTES, true));
    let wait_result = timeout(deadline, child.wait()).await;
    let timed_out = wait_result.is_err();
    let status = match wait_result {
        Ok(Ok(status)) => Some(status),
        Ok(Err(_)) => None,
        Err(_) => {
            let _ = child.start_kill();
            timeout(Duration::from_secs(3), child.wait())
                .await
                .ok()
                .and_then(Result::ok)
        }
    };
    let stdout = finish_capture_task(stdout_task).await?;
    let stderr = finish_capture_task(stderr_task).await?;
    if timed_out {
        return Err("acceptance child process exceeded its finite deadline".to_owned());
    }
    if status.is_none() {
        return Err("acceptance child process could not be reaped".to_owned());
    }
    Ok(ProcessCapture {
        status_code: status.as_ref().and_then(|status| status.code()),
        success: status.as_ref().is_some_and(|status| status.success()),
        stdout: stdout.bytes,
        stdout_truncated: stdout.truncated,
        stderr_tail: stderr.bytes,
        stderr_truncated: stderr.truncated,
    })
}

async fn capture_stream<R>(
    mut reader: R,
    maximum_bytes: usize,
    keep_tail: bool,
) -> std::io::Result<CapturedStream>
where
    R: AsyncRead + Unpin,
{
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(CapturedStream {
                bytes: captured,
                truncated,
            });
        }
        let chunk = &buffer[..read];
        if keep_tail {
            if chunk.len() >= maximum_bytes {
                captured.clear();
                captured.extend_from_slice(&chunk[chunk.len() - maximum_bytes..]);
                truncated = true;
                continue;
            }
            let overflow = captured
                .len()
                .saturating_add(chunk.len())
                .saturating_sub(maximum_bytes);
            if overflow > 0 {
                captured.drain(..overflow);
                truncated = true;
            }
            captured.extend_from_slice(chunk);
        } else if captured.len() < maximum_bytes {
            let copied = (maximum_bytes - captured.len()).min(chunk.len());
            captured.extend_from_slice(&chunk[..copied]);
            truncated |= copied < chunk.len();
        } else {
            truncated = true;
        }
    }
}

async fn finish_capture_task(
    task: JoinHandle<std::io::Result<CapturedStream>>,
) -> Result<CapturedStream, String> {
    timeout(Duration::from_secs(3), task)
        .await
        .map_err(|_| "acceptance child output reader exceeded its deadline".to_owned())?
        .map_err(|_| "acceptance child output reader task failed".to_owned())?
        .map_err(|_| "acceptance child output stream failed".to_owned())
}

fn required_executable(name: &str) -> Result<PathBuf, String> {
    let path = env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{name} is required"))?;
    if !path.is_absolute() {
        return Err(format!("{name} must name an absolute executable path"));
    }
    let path = path
        .canonicalize()
        .map_err(|_| format!("{name} did not resolve to an existing executable"))?;
    let metadata = std::fs::metadata(&path)
        .map_err(|_| format!("{name} executable metadata could not be read"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(format!("{name} is not an executable file"));
    }
    Ok(path)
}

fn find_executable(name: &str) -> Result<PathBuf, String> {
    let path =
        env::var_os("PATH").ok_or_else(|| "PATH is required to locate FFmpeg tools".to_owned())?;
    for directory in env::split_paths(&path) {
        let candidate = directory.join(name);
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            return candidate
                .canonicalize()
                .map_err(|_| format!("{name} executable path could not be resolved"));
        }
    }
    Err(format!("required local tool {name} was not found on PATH"))
}

async fn bind_loopback_listener() -> Result<TcpListener, String> {
    TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .map_err(|_| "local IPv4 loopback listener could not be bound".to_owned())
}

fn origin_url(address: SocketAddr, file_name: &str) -> Result<String, String> {
    if !is_strict_loopback(address) || !matches!(file_name, "high.mp4" | "low.mp4" | "audio.mp4") {
        return Err("fixture source URL did not use the strict local allowlist".to_owned());
    }
    let url = format!("http://{address}/{file_name}");
    ensure_loopback_url(&url)?;
    Ok(url)
}

fn ensure_loopback_url(value: &str) -> Result<(), String> {
    let parsed =
        url::Url::parse(value).map_err(|_| "acceptance source URL was invalid".to_owned())?;
    if parsed.scheme() != "http"
        || parsed.host_str() != Some("127.0.0.1")
        || parsed.username() != ""
        || parsed.password().is_some()
    {
        return Err("acceptance source URL was not strict IPv4 loopback HTTP".to_owned());
    }
    Ok(())
}

fn is_strict_loopback(address: SocketAddr) -> bool {
    address.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST)
}

async fn write_report(path: &Path, report: &ReportStore) -> Result<(), String> {
    let mut snapshot = report
        .lock()
        .map_err(|_| "acceptance report lock was poisoned".to_owned())?
        .clone();
    let mut bytes = serde_json::to_vec(&snapshot)
        .map_err(|_| "acceptance report could not be encoded as JSON".to_owned())?;
    if bytes.len() > MAX_REPORT_BYTES {
        snapshot.requests.events.truncate(8);
        snapshot.probe = None;
        snapshot.error = Some("report payload reduced to enforce the 64 KiB limit".to_owned());
        bytes = serde_json::to_vec(&snapshot)
            .map_err(|_| "bounded acceptance report could not be encoded".to_owned())?;
    }
    if bytes.len() > MAX_REPORT_BYTES {
        return Err("acceptance report exceeded its 64 KiB bound".to_owned());
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|_| "acceptance report directory could not be created".to_owned())?;
    let temporary = path.with_extension("json.tmp");
    tokio::fs::write(&temporary, bytes)
        .await
        .map_err(|_| "bounded acceptance report could not be written".to_owned())?;
    tokio::fs::rename(&temporary, path)
        .await
        .map_err(|_| "bounded acceptance report could not be installed".to_owned())
}

fn bounded_error(error: &str) -> String {
    let mut value = error.chars().take(900).collect::<String>();
    if error.chars().count() > 900 {
        value.push_str("...");
    }
    value
}

impl ServerTask {
    async fn stop(mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for ServerTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

impl<T> AbortTask<T> {
    fn is_finished(&self) -> bool {
        self.0.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn abort(&mut self) {
        if let Some(task) = self.0.as_ref() {
            task.abort();
        }
    }

    async fn join(&mut self) -> Result<T, String> {
        let task = self
            .0
            .take()
            .ok_or_else(|| "native probe task was already joined".to_owned())?;
        task.await
            .map_err(|_| "native probe task ended before returning a result".to_owned())
    }
}

impl<T> Drop for AbortTask<T> {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod origin_request_diagnostic_tests {
    use super::*;

    fn sample_fragments() -> [Mp4SegmentRange; 3] {
        [
            Mp4SegmentRange {
                offset: 100,
                length: 100,
                duration_millis: 1_000,
            },
            Mp4SegmentRange {
                offset: 200,
                length: 100,
                duration_millis: 2_500,
            },
            Mp4SegmentRange {
                offset: 300,
                length: 200,
                duration_millis: 500,
            },
        ]
    }

    #[tokio::test]
    async fn recovery_head_probe_does_not_capture_first_media_request() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("high.mp4");
        tokio::fs::write(&path, vec![0_u8; 500]).await.unwrap();
        let report = Arc::new(Mutex::new(AcceptanceReport::new(
            NativeCase::ProgressiveAbr,
        )));
        let origin = Arc::new(OriginState {
            media: HashMap::from([(
                "high.mp4".to_owned(),
                OriginMedia {
                    path,
                    representation_code: 1,
                    initialization_length: 100,
                    fragments: sample_fragments().to_vec(),
                    content_type: "video/mp4",
                },
            )]),
            case: NativeCase::ProgressiveAbr,
            phase: Arc::new(AtomicU8::new(PHASE_RECOVERY)),
            epoch: Arc::new(Mutex::new(Instant::now())),
            report: Arc::clone(&report),
            pacers: std::array::from_fn(|_| RepresentationPacer::default()),
        });
        let headers = |range: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(RANGE, range.parse().unwrap());
            headers
        };
        let head = origin_response(
            Arc::clone(&origin),
            "high.mp4".to_owned(),
            headers("bytes=150-199"),
            true,
        )
        .await;
        assert_eq!(head.status(), StatusCode::PARTIAL_CONTENT);
        assert!(
            axum::body::to_bytes(head.into_body(), 50)
                .await
                .unwrap()
                .is_empty()
        );
        {
            let snapshot = report.lock().unwrap();
            assert_eq!(snapshot.requests.total, 1);
            assert_eq!(
                snapshot.requests.media_payload_by_phase_and_representation[2][0],
                0
            );
            assert!(
                snapshot
                    .requests
                    .first_recovery_high_media_request
                    .is_none()
            );
            assert_eq!(
                snapshot
                    .requests
                    .first_recovery_high_range_request
                    .as_ref()
                    .unwrap()
                    .range_start,
                150
            );
        }
        let get = origin_response(
            origin,
            "high.mp4".to_owned(),
            headers("bytes=250-299"),
            false,
        )
        .await;
        assert_eq!(get.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            axum::body::to_bytes(get.into_body(), 50)
                .await
                .unwrap()
                .len(),
            50
        );
        let snapshot = report.lock().unwrap();
        assert_eq!(snapshot.requests.total, 2);
        assert_eq!(
            snapshot.requests.media_payload_by_phase_and_representation[2][0],
            1
        );
        assert_eq!(
            snapshot
                .requests
                .first_recovery_high_range_request
                .as_ref()
                .unwrap()
                .range_start,
            150
        );
        let first_media = snapshot
            .requests
            .first_recovery_high_media_request
            .as_ref()
            .unwrap();
        assert_eq!(first_media.range_start, 250);
        assert_eq!(first_media.media_window.unwrap().first_fragment_index, 1);
    }

    #[test]
    fn media_window_uses_half_open_fragment_boundaries_and_parsed_durations() {
        let fragments = sample_fragments();
        let window = media_window_for_byte_range(
            ByteRange {
                start: 200,
                length: 100,
                partial: true,
            },
            100,
            500,
            &fragments,
        )
        .unwrap();

        assert_eq!(window.start_seconds, 1.0);
        assert_eq!(window.end_seconds, 3.5);
        assert_eq!(window.first_fragment_index, 1);
        assert_eq!(window.last_fragment_index, 1);

        let initialization_and_media = media_window_for_byte_range(
            ByteRange {
                start: 50,
                length: 110,
                partial: true,
            },
            100,
            500,
            &fragments,
        )
        .unwrap();
        assert_eq!(initialization_and_media.start_seconds, 0.0);
        assert_eq!(initialization_and_media.end_seconds, 1.0);
        assert_eq!(initialization_and_media.first_fragment_index, 0);
        assert_eq!(initialization_and_media.last_fragment_index, 0);
    }

    #[test]
    fn media_window_is_null_for_initialization_only_range() {
        assert_eq!(
            media_window_for_byte_range(
                ByteRange {
                    start: 0,
                    length: 100,
                    partial: true,
                },
                100,
                500,
                &sample_fragments(),
            ),
            None,
        );
    }

    #[test]
    fn media_window_spanning_fragments_uses_first_start_and_last_end() {
        let window = media_window_for_byte_range(
            ByteRange {
                start: 150,
                length: 160,
                partial: true,
            },
            100,
            500,
            &sample_fragments(),
        )
        .unwrap();

        assert_eq!(window.start_seconds, 0.0);
        assert_eq!(window.end_seconds, 4.0);
        assert_eq!(window.first_fragment_index, 0);
        assert_eq!(window.last_fragment_index, 2);
    }

    #[test]
    fn media_window_is_null_for_overflowing_or_truncated_mapping() {
        let fragments = sample_fragments();
        assert_eq!(
            media_window_for_byte_range(
                ByteRange {
                    start: u64::MAX,
                    length: 1,
                    partial: true,
                },
                100,
                500,
                &fragments,
            ),
            None,
        );
        assert_eq!(
            media_window_for_byte_range(
                ByteRange {
                    start: 150,
                    length: 10,
                    partial: true,
                },
                100,
                501,
                &fragments,
            ),
            None,
        );
        let overflowing_durations = [
            Mp4SegmentRange {
                offset: 100,
                length: 200,
                duration_millis: u64::MAX,
            },
            Mp4SegmentRange {
                offset: 300,
                length: 200,
                duration_millis: 1,
            },
        ];
        assert_eq!(
            media_window_for_byte_range(
                ByteRange {
                    start: 150,
                    length: 10,
                    partial: true,
                },
                100,
                500,
                &overflowing_durations,
            ),
            None,
        );
        let overflowing_fragments = [
            Mp4SegmentRange {
                offset: 100,
                length: u64::MAX,
                duration_millis: 1,
            },
            Mp4SegmentRange {
                offset: 0,
                length: 1,
                duration_millis: 1,
            },
        ];
        assert_eq!(
            media_window_for_byte_range(
                ByteRange {
                    start: 150,
                    length: 10,
                    partial: true,
                },
                100,
                500,
                &overflowing_fragments,
            ),
            None,
        );
    }

    #[test]
    fn first_recovery_range_and_first_successful_media_request_are_distinct() {
        let report = Arc::new(Mutex::new(AcceptanceReport::new(
            NativeCase::ExtendedDiagnostic,
        )));
        let origin = OriginState {
            media: HashMap::new(),
            case: NativeCase::ExtendedDiagnostic,
            phase: Arc::new(AtomicU8::new(PHASE_RECOVERY)),
            epoch: Arc::new(Mutex::new(Instant::now())),
            report: Arc::clone(&report),
            pacers: std::array::from_fn(|_| RepresentationPacer::default()),
        };
        let init_range = ByteRange {
            start: 0,
            length: 100,
            partial: true,
        };
        let media_range = ByteRange {
            start: 150,
            length: 50,
            partial: true,
        };
        let media_window = media_window_for_byte_range(media_range, 100, 500, &sample_fragments());

        record_origin_request(
            &origin,
            1,
            PHASE_RECOVERY,
            init_range,
            StatusCode::SERVICE_UNAVAILABLE,
            false,
            None,
        );
        record_origin_request(
            &origin,
            1,
            PHASE_RECOVERY,
            media_range,
            StatusCode::SERVICE_UNAVAILABLE,
            false,
            media_window,
        );
        record_origin_request(
            &origin,
            1,
            PHASE_RECOVERY,
            media_range,
            StatusCode::PARTIAL_CONTENT,
            true,
            media_window,
        );

        let report = report.lock().unwrap();
        assert_eq!(report.requests.total, 3);
        assert_eq!(report.requests.events.len(), 3);
        let first_range = report
            .requests
            .first_recovery_high_range_request
            .as_ref()
            .unwrap();
        assert_eq!(first_range.range_start, 0);
        assert_eq!(first_range.media_window, None);
        assert_eq!(
            serde_json::to_value(first_range).unwrap()["media_window"],
            serde_json::Value::Null
        );
        let first_media = report
            .requests
            .first_recovery_high_media_request
            .as_ref()
            .unwrap();
        assert_eq!(first_media.range_start, 150);
        assert_eq!(
            first_media.status_code,
            StatusCode::PARTIAL_CONTENT.as_u16()
        );
        assert_eq!(first_media.media_window, media_window);
        let serialized = serde_json::to_value(first_media).unwrap();
        assert_eq!(
            serialized["media_window"],
            serde_json::json!({
                "start_seconds": 0.0,
                "end_seconds": 1.0,
                "first_fragment_index": 0,
                "last_fragment_index": 0,
            })
        );
    }

    #[test]
    fn first_recovery_high_range_survives_full_early_event_window() {
        let report = Arc::new(Mutex::new(AcceptanceReport::new(
            NativeCase::ExtendedDiagnostic,
        )));
        let origin = OriginState {
            media: HashMap::new(),
            case: NativeCase::ExtendedDiagnostic,
            phase: Arc::new(AtomicU8::new(PHASE_BASELINE)),
            epoch: Arc::new(Mutex::new(Instant::now())),
            report: Arc::clone(&report),
            pacers: std::array::from_fn(|_| RepresentationPacer::default()),
        };
        let range = ByteRange {
            start: 100,
            length: 50,
            partial: true,
        };
        for _ in 0..40 {
            record_origin_request(
                &origin,
                1,
                PHASE_BASELINE,
                range,
                StatusCode::PARTIAL_CONTENT,
                true,
                None,
            );
        }
        record_origin_request(
            &origin,
            1,
            PHASE_RECOVERY,
            range,
            StatusCode::PARTIAL_CONTENT,
            true,
            None,
        );

        let report = report.lock().unwrap();
        assert_eq!(report.requests.events.len(), 32);
        let event = report
            .requests
            .first_recovery_high_range_request
            .as_ref()
            .unwrap();
        assert!(
            event
                .monotonic_seconds
                .is_some_and(|value| value.is_finite())
        );
        assert_eq!(event.range_start, 100);
        assert_eq!(event.range_length, 50);
        assert_eq!(event.status_code, StatusCode::PARTIAL_CONTENT.as_u16());
    }

    #[test]
    fn latest_request_times_start_missing_and_survive_event_cap() {
        let report = Arc::new(Mutex::new(AcceptanceReport::new(
            NativeCase::ExtendedDiagnostic,
        )));
        let origin = OriginState {
            media: HashMap::new(),
            case: NativeCase::ExtendedDiagnostic,
            phase: Arc::new(AtomicU8::new(PHASE_BASELINE)),
            epoch: Arc::new(Mutex::new(Instant::now())),
            report: Arc::clone(&report),
            pacers: std::array::from_fn(|_| RepresentationPacer::default()),
        };
        let range = ByteRange {
            start: 100,
            length: 50,
            partial: true,
        };
        {
            let report = report.lock().unwrap();
            assert!(
                report
                    .requests
                    .last_request_elapsed_ms_by_phase_and_representation
                    .iter()
                    .flatten()
                    .all(Option::is_none)
            );
        }

        for _ in 0..40 {
            record_origin_request(
                &origin,
                1,
                PHASE_BASELINE,
                range,
                StatusCode::PARTIAL_CONTENT,
                true,
                None,
            );
        }
        let latest = report
            .lock()
            .unwrap()
            .requests
            .last_request_elapsed_ms_by_phase_and_representation[0][0];
        std::thread::sleep(Duration::from_millis(2));
        record_origin_request(
            &origin,
            1,
            PHASE_BASELINE,
            range,
            StatusCode::PARTIAL_CONTENT,
            true,
            None,
        );

        let report = report.lock().unwrap();
        assert_eq!(report.requests.events.len(), 32);
        assert!(latest.is_some());
        assert!(
            report
                .requests
                .last_request_elapsed_ms_by_phase_and_representation[0][0]
                > latest
        );
    }

    #[test]
    fn latest_request_times_are_isolated_by_phase_and_representation() {
        let report = Arc::new(Mutex::new(AcceptanceReport::new(
            NativeCase::ExtendedDiagnostic,
        )));
        let origin = OriginState {
            media: HashMap::new(),
            case: NativeCase::ExtendedDiagnostic,
            phase: Arc::new(AtomicU8::new(PHASE_BASELINE)),
            epoch: Arc::new(Mutex::new(Instant::now())),
            report: Arc::clone(&report),
            pacers: std::array::from_fn(|_| RepresentationPacer::default()),
        };
        let range = ByteRange {
            start: 100,
            length: 50,
            partial: true,
        };
        for (phase, representation) in [
            (PHASE_BASELINE, 1),
            (PHASE_BAD_HIGH, 2),
            (PHASE_RECOVERY, 3),
        ] {
            record_origin_request(
                &origin,
                representation,
                phase,
                range,
                StatusCode::PARTIAL_CONTENT,
                true,
                None,
            );
        }

        let report = report.lock().unwrap();
        let times = &report
            .requests
            .last_request_elapsed_ms_by_phase_and_representation;
        assert!(times[0][0].is_some());
        assert!(times[1][1].is_some());
        assert!(times[2][2].is_some());
        for (phase, representation) in [(0, 1), (0, 2), (1, 0), (1, 2), (2, 0), (2, 1)] {
            assert_eq!(times[phase][representation], None);
        }
    }
}

mod origin_pacing_tests {
    use super::*;

    #[test]
    fn long_recovery_profile_is_shared_without_conflating_case_identity() {
        let diagnostic = NativeCase::ExtendedDiagnostic;
        let required = NativeCase::ProgressiveAbr;
        let buffered = NativeCase::BufferedContinuity;
        let transcoding = NativeCase::Transcoding;
        assert_eq!(required.full_name(), ABR_CASE_FULL_NAME);
        assert_eq!(required.report_name(), "abr.json");
        assert_eq!(required.scenario(), "unbuffered_recovery");
        assert_eq!(required.source_duration_seconds(), 3600);
        assert_eq!(required.deadline(), Duration::from_secs(840));
        assert_eq!(required.probe_duration_seconds(), 600);
        assert_eq!(required.probe_deadline_seconds(), 720);
        assert!(!AcceptanceReport::new(required).diagnostic_only);
        assert_eq!(diagnostic.full_name(), EXTENDED_DIAGNOSTIC_CASE_FULL_NAME);
        assert_ne!(required.full_name(), diagnostic.full_name());
        assert_eq!(diagnostic.report_name(), "abr-extended-diagnostic.json");
        assert_eq!(diagnostic.scenario(), "extended_recovery_diagnostic");
        assert_eq!(diagnostic.source_duration_seconds(), 3600);
        assert_eq!(diagnostic.deadline(), Duration::from_secs(840));
        assert_eq!(diagnostic.probe_duration_seconds(), 600);
        assert_eq!(diagnostic.probe_deadline_seconds(), 720);
        assert!(AcceptanceReport::new(diagnostic).diagnostic_only);
        assert!(diagnostic.is_diagnostic());
        assert!(!required.is_diagnostic());
        assert!(required.uses_long_profile());
        assert!(diagnostic.uses_long_profile());
        assert!(!buffered.uses_long_profile());
        assert!(!transcoding.uses_long_profile());
        assert_eq!(diagnostic.cache_limit_bytes(), 2 * 1024 * 1024 * 1024);
        assert_eq!(required.cache_limit_bytes(), 2 * 1024 * 1024 * 1024);
        assert_eq!(buffered.cache_limit_bytes(), MAX_FIXTURE_BYTES);
        assert_eq!(transcoding.cache_limit_bytes(), MAX_FIXTURE_BYTES);
        assert_eq!(MAX_LONG_SOURCE_FIXTURE_BYTES, 1280 * 1024 * 1024);
        assert_eq!(MAX_LONG_HIGH_FIXTURE_BYTES, 960 * 1024 * 1024);
        assert_eq!(MAX_LONG_LOW_FIXTURE_BYTES, 256 * 1024 * 1024);
        assert_eq!(MAX_LONG_AUDIO_FIXTURE_BYTES, 64 * 1024 * 1024);
        assert_eq!(LONG_RECOVERY_PROBE_DURATION_SECONDS, 600);
        const { assert!(LONG_RECOVERY_PROBE_DURATION_SECONDS <= 600) };
        assert_eq!(LONG_RECOVERY_PROBE_DEADLINE_SECONDS, 720);
        const { assert!(LONG_RECOVERY_PROBE_DEADLINE_SECONDS <= 900) };
        assert_eq!(ABR_PROBE_DURATION_SECONDS, 130);
        assert_eq!(ABR_PROBE_DEADLINE_SECONDS, 175);
        assert_eq!(buffered.source_duration_seconds(), 160);
        assert_eq!(buffered.deadline(), Duration::from_secs(225));
        assert_eq!(buffered.probe_duration_seconds(), 130);
        assert_eq!(buffered.probe_deadline_seconds(), 175);
        assert_eq!(MINIMUM_STEADY_PLAY_SECONDS, 110);
        assert_eq!(transcoding.source_duration_seconds(), 45);
        assert_eq!(transcoding.deadline(), Duration::from_secs(225));
        assert_eq!(TRANSCODE_PROBE_DURATION_SECONDS, 30);
        assert_eq!(transcoding.probe_duration_seconds(), 30);
        assert_eq!(transcoding.probe_deadline_seconds(), 45);
        assert_eq!(LONG_RECOVERY_FFMPEG_TIMEOUT, Duration::from_secs(120));
        assert_eq!(LONG_RECOVERY_FFPROBE_TIMEOUT, Duration::from_secs(30));
        assert_eq!(FFMPEG_TIMEOUT, Duration::from_secs(100));
        assert_eq!(playlist_response_limit(true), 512 * 1024);
        assert_eq!(playlist_response_limit(false), 64 * 1024);
    }

    #[test]
    fn phase_rates_apply_only_to_progressive_video_representations() {
        for representation_code in [1, 2] {
            assert_eq!(
                pacing_rate_bits_per_second(
                    NativeCase::ProgressiveAbr,
                    PHASE_BASELINE,
                    representation_code,
                ),
                Some(ABR_BASELINE_RATE_BITS_PER_SECOND),
            );
            assert_eq!(
                pacing_rate_bits_per_second(
                    NativeCase::ProgressiveAbr,
                    PHASE_BAD_HIGH,
                    representation_code,
                ),
                Some(ABR_BAD_RATE_BITS_PER_SECOND),
            );
            assert_eq!(
                pacing_rate_bits_per_second(
                    NativeCase::ProgressiveAbr,
                    PHASE_RECOVERY,
                    representation_code,
                ),
                Some(ABR_RECOVERY_RATE_BITS_PER_SECOND),
            );
        }
        assert_eq!(
            pacing_rate_bits_per_second(NativeCase::Transcoding, PHASE_BASELINE, 1),
            None,
        );
        assert_eq!(
            pacing_rate_bits_per_second(NativeCase::ProgressiveAbr, PHASE_BASELINE, 3),
            None,
        );
    }

    #[test]
    fn chunk_duration_accounts_for_bytes_at_the_selected_bit_rate() {
        assert_eq!(
            chunk_transmission_duration(524_288, ABR_BASELINE_RATE_BITS_PER_SECOND),
            Duration::from_micros(1_048_576),
        );
        assert_eq!(
            chunk_transmission_duration(4_000, ABR_BASELINE_RATE_BITS_PER_SECOND),
            Duration::from_millis(8),
        );
        assert_eq!(
            chunk_transmission_duration(0, ABR_BASELINE_RATE_BITS_PER_SECOND),
            Duration::ZERO,
        );
    }

    #[test]
    fn abr_timeline_uses_shared_clock_independent_of_process_elapsed_origins() {
        let report = Arc::new(Mutex::new(AcceptanceReport::new(
            NativeCase::ProgressiveAbr,
        )));
        {
            let mut report = report.lock().unwrap();
            report.fault_timeline.bad_high_before_monotonic_seconds = Some(100.0);
            report.fault_timeline.bad_high_after_monotonic_seconds = Some(101.0);
            report.fault_timeline.recovery_before_monotonic_seconds = Some(200.0);
            report.fault_timeline.recovery_after_monotonic_seconds = Some(201.0);
            report.fault_timeline.bad_high_start_elapsed_ms = Some(5_000);
            report.fault_timeline.recovery_start_elapsed_ms = Some(15_000);
            report.fault_timeline.decoder_ready_monotonic_seconds = Some(90.0);
            report.fault_timeline.baseline_start_monotonic_seconds = Some(94.0);
        }
        let make_samples = |values: [f64; 3], elapsed_origin: f64| {
            values
                .into_iter()
                .enumerate()
                .map(|(index, timestamp)| {
                    serde_json::json!({
                        "observed_monotonic_seconds": timestamp,
                        "observed_elapsed_seconds": elapsed_origin + index as f64,
                    })
                })
                .collect::<Vec<_>>()
        };
        let probe = serde_json::json!({
            "video_size_expectations": {
                "entries": [
                    {"samples": make_samples([90.0, 95.0, 99.0], 50_000.0)},
                    {"samples": make_samples([101.0, 150.0, 199.0], 0.5)},
                    {"samples": make_samples([201.0, 202.0, 203.0], 8.0)},
                ]
            }
        });

        assert!(validate_abr_sample_timeline(NativeCase::ProgressiveAbr, &probe, &report).is_ok());
    }

    #[test]
    fn abr_timeline_requires_exact_finite_ordered_samples_inside_strict_bounds() {
        let validate = |initial, low, recovered| {
            validate_abr_sample_timestamps(initial, low, recovered, 100.0, 101.0, 200.0, 201.0)
        };
        assert!(
            validate(
                &[90.0, 95.0, 99.0],
                &[101.0, 150.0, 199.0],
                &[201.0, 202.0, 203.0]
            )
            .is_ok()
        );
        assert!(
            validate(
                &[90.0, 100.0, 99.0],
                &[101.0, 150.0, 199.0],
                &[201.0, 202.0, 203.0]
            )
            .is_err()
        );
        assert!(
            validate(
                &[90.0, 95.0, 99.0],
                &[100.0, 150.0, 199.0],
                &[201.0, 202.0, 203.0]
            )
            .is_err()
        );
        assert!(
            validate(
                &[90.0, 95.0, 99.0],
                &[101.0, 200.0, 199.0],
                &[201.0, 202.0, 203.0]
            )
            .is_err()
        );
        assert!(
            validate(
                &[90.0, 95.0, 99.0],
                &[101.0, f64::NAN, 199.0],
                &[201.0, 202.0, 203.0]
            )
            .is_err()
        );
        assert!(
            validate(
                &[90.0, 95.0],
                &[101.0, 150.0, 199.0],
                &[201.0, 202.0, 203.0]
            )
            .is_err()
        );
        assert!(
            validate_abr_sample_timestamps(
                &[90.0, 95.0, 99.0],
                &[101.0, 150.0, 199.0],
                &[201.0, 202.0, 203.0],
                100.0,
                201.0,
                200.0,
                202.0,
            )
            .is_err()
        );
    }

    #[test]
    fn native_monotonic_clock_returns_finite_positive_seconds() {
        let timestamps = [
            monotonic_clock_seconds().unwrap(),
            monotonic_clock_seconds().unwrap(),
        ];
        assert!(
            timestamps
                .iter()
                .all(|timestamp| timestamp.is_finite() && *timestamp > 0.0)
        );
        assert!(timestamps[0] <= timestamps[1]);
    }

    #[test]
    fn shared_pacer_serializes_chunks_without_reserving_waiting_bytes() {
        let first_request = RepresentationPacer::default();
        let concurrent_request = first_request.clone();
        let start = Instant::now();
        let chunk_bytes = 4_000;
        let chunk_duration = Duration::from_millis(8);

        assert!(first_request.state.lock().unwrap().try_reserve(
            start,
            chunk_bytes,
            ABR_BASELINE_RATE_BITS_PER_SECOND
        ));

        let mut state = concurrent_request.state.lock().unwrap();
        assert_eq!(state.wait_time(start), chunk_duration);
        assert!(!state.try_reserve(start, chunk_bytes, ABR_BASELINE_RATE_BITS_PER_SECOND,));
        let available_at = start + chunk_duration;
        assert_eq!(state.next_available_at, Some(available_at));
        assert_eq!(
            state.wait_time(start + Duration::from_millis(2)),
            Duration::from_millis(6),
        );
        assert_eq!(state.next_available_at, Some(available_at));
        assert!(state.try_reserve(available_at, chunk_bytes, ABR_BASELINE_RATE_BITS_PER_SECOND,));
        assert_eq!(state.next_available_at, Some(available_at + chunk_duration));
    }
}

#[cfg(test)]
mod protocol_contract_tests {
    use super::*;

    #[test]
    fn h264_advertisement_matches_constrained_baseline_level_3_metadata() {
        assert!(h264_baseline_level_3_is_coherent(
            "avc1.42c01e",
            Some("Constrained Baseline"),
            Some(30),
            "Constrained Baseline",
            30,
        ));
        assert!(!h264_baseline_level_3_is_coherent(
            "avc1.640028",
            Some("Constrained Baseline"),
            Some(30),
            "Constrained Baseline",
            30,
        ));
        assert!(!h264_baseline_level_3_is_coherent(
            "avc1.42c01e",
            Some("High"),
            Some(30),
            "Constrained Baseline",
            30,
        ));
        assert!(!h264_baseline_level_3_is_coherent(
            "avc1.42c01e",
            Some("Constrained Baseline"),
            Some(40),
            "Constrained Baseline",
            30,
        ));
    }

    #[test]
    fn selected_high_variant_resolves_only_its_same_group_audio_playlist() {
        let master = concat!(
            "#EXTM3U\n",
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-high\",NAME=\"Default\",URI=\"segments/audio-high.m3u8\"\n",
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-low\",NAME=\"Default\",URI=\"segments/audio-low.m3u8\"\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=640x360,CODECS=\"avc1.42c01e,mp4a.40.2\",AUDIO=\"audio-high\"\n",
            "segments/high.m3u8\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=500000,RESOLUTION=320x180,CODECS=\"avc1.42c01e,mp4a.40.2\",AUDIO=\"audio-low\"\n",
            "segments/low.m3u8\n",
        );
        let SelectedHlsPlaylists {
            video_uri,
            audio_uri,
            bandwidth,
        } = selected_high_playlist_uris(master).unwrap();
        let base = url::Url::parse("http://127.0.0.1:4321/session/master.m3u8").unwrap();
        assert_eq!(video_uri, "segments/high.m3u8");
        assert_eq!(audio_uri.as_deref(), Some("segments/audio-high.m3u8"));
        assert_eq!(bandwidth, 2_000_000);
        assert_eq!(
            resolve_same_origin_playlist(&base, &video_uri)
                .unwrap()
                .path(),
            "/session/segments/high.m3u8",
        );
        assert!(resolve_same_origin_playlist(&base, "http://example.com/audio.m3u8").is_err());
    }

    #[test]
    fn transcoding_master_guard_accepts_muxed_output_without_external_audio_group() {
        let master = concat!(
            "#EXTM3U\n",
            "#EXT-X-STREAM-INF:BANDWIDTH=2128000,RESOLUTION=640x360,CODECS=\"avc1.640028,mp4a.40.2\"\n",
            "transcoded.m3u8\n",
        );
        assert!(master.starts_with("#EXTM3U\n") && master.contains("#EXT-X-STREAM-INF:"));
        assert!(!source_h264_sidx_guard_applies(NativeCase::Transcoding));
        let SelectedHlsPlaylists {
            video_uri,
            audio_uri,
            bandwidth,
        } = selected_high_playlist_uris(master).unwrap();
        assert_eq!(video_uri, "transcoded.m3u8");
        assert_eq!(audio_uri, None);
        assert_eq!(bandwidth, 2_128_000);
        assert!(!master_bandwidth_matches_peak(bandwidth, 2_000_001));
    }

    #[test]
    fn rfc_peak_bandwidth_uses_contiguous_target_duration_windows() {
        let short_tail = [
            Mp4SegmentRange {
                offset: 0,
                length: 32_000,
                duration_millis: 2_000,
            },
            Mp4SegmentRange {
                offset: 32_000,
                length: 2_000,
                duration_millis: 100,
            },
        ];
        assert_eq!(fixture_target_duration_seconds(&short_tail), Some(2));
        assert_eq!(
            peak_bandwidth_bits_per_second(&short_tail, 2),
            Some(129_524)
        );
        assert_eq!(
            legacy_single_fragment_peak_bandwidth_bits_per_second(&short_tail),
            Some(160_000),
        );

        let boundary_windows = [
            Mp4SegmentRange {
                offset: 0,
                length: 1_000,
                duration_millis: 1_000,
            },
            Mp4SegmentRange {
                offset: 1_000,
                length: 500,
                duration_millis: 500,
            },
            Mp4SegmentRange {
                offset: 1_500,
                length: 3_000,
                duration_millis: 1_500,
            },
        ];
        assert_eq!(
            peak_bandwidth_bits_per_second(&boundary_windows, 2),
            Some(16_000)
        );

        let three_second_boundary = [
            Mp4SegmentRange {
                offset: 0,
                length: 1_000,
                duration_millis: 1_000,
            },
            Mp4SegmentRange {
                offset: 1_000,
                length: 1_000,
                duration_millis: 1_000,
            },
            Mp4SegmentRange {
                offset: 2_000,
                length: 1_000,
                duration_millis: 1_000,
            },
        ];
        assert_eq!(
            peak_bandwidth_bits_per_second(&three_second_boundary, 2),
            Some(8_000)
        );
        assert_eq!(
            peak_bandwidth_bits_per_second(&three_second_boundary[..1], 2),
            Some(8_000)
        );
    }

    #[test]
    fn rfc_peak_bandwidth_rejects_empty_invalid_ineligible_and_overflowing_ranges() {
        assert_eq!(peak_bandwidth_bits_per_second(&[], 2), None);
        assert_eq!(fixture_target_duration_seconds(&[]), None);
        assert_eq!(
            peak_bandwidth_bits_per_second(
                &[Mp4SegmentRange {
                    offset: 0,
                    length: 1,
                    duration_millis: 0
                }],
                2
            ),
            None,
        );
        assert_eq!(
            peak_bandwidth_bits_per_second(
                &[Mp4SegmentRange {
                    offset: 0,
                    length: 0,
                    duration_millis: 1_000
                }],
                2
            ),
            None,
        );
        assert_eq!(
            peak_bandwidth_bits_per_second(
                &[Mp4SegmentRange {
                    offset: 0,
                    length: 500,
                    duration_millis: 500
                }],
                2
            ),
            None,
        );
        assert_eq!(
            peak_bandwidth_bits_per_second(
                &[Mp4SegmentRange {
                    offset: 0,
                    length: u64::MAX,
                    duration_millis: 1_000
                }],
                2
            ),
            None,
        );
        assert_eq!(
            peak_bandwidth_bits_per_second(
                &[
                    Mp4SegmentRange {
                        offset: 0,
                        length: u64::MAX,
                        duration_millis: 500
                    },
                    Mp4SegmentRange {
                        offset: 0,
                        length: 1,
                        duration_millis: 500
                    },
                ],
                2,
            ),
            None,
        );
        assert_eq!(
            legacy_single_fragment_peak_bandwidth_bits_per_second(&[Mp4SegmentRange {
                offset: 0,
                length: 1,
                duration_millis: 0,
            }]),
            None,
        );
    }

    #[test]
    fn aggregate_peak_bandwidth_includes_audio_and_checks_overflow() {
        let shared_audio_peak = 740_733;
        let high_video_peak = 2_462_920;
        let high_variant_peak =
            aggregate_peak_bandwidth(high_video_peak, shared_audio_peak).unwrap();
        assert_eq!(high_variant_peak, 3_203_653);
        assert_eq!(
            aggregate_peak_bandwidth(2_390_920, 238_183).unwrap(),
            2_629_103
        );
        assert_eq!(
            aggregate_peak_bandwidth(500_000, shared_audio_peak).unwrap(),
            1_240_733
        );
        assert!(aggregate_peak_bandwidth(u64::MAX, 1).is_err());
        assert!(master_bandwidth_matches_peak(
            high_variant_peak,
            high_variant_peak
        ));
        assert!(!master_bandwidth_matches_peak(
            high_variant_peak - 1,
            high_variant_peak
        ));
        assert!(!master_bandwidth_matches_peak(
            high_variant_peak + 1,
            high_variant_peak
        ));
        assert!(!master_bandwidth_matches_peak(0, high_variant_peak));
        let separate_audio_peak =
            expected_variant_peak_bandwidth(2_128_000, 128_000, false).unwrap();
        assert_eq!(separate_audio_peak, 2_256_000);
        let muxed_transcode_peak =
            expected_variant_peak_bandwidth(2_128_000, 128_000, true).unwrap();
        assert_eq!(muxed_transcode_peak, 2_128_000);
        assert!(master_bandwidth_matches_peak(
            muxed_transcode_peak,
            muxed_transcode_peak
        ));
    }

    #[test]
    fn same_codec_fixture_rungs_have_distinct_video_keys_and_shared_audio_key() {
        let high = make_request(
            BilibiliMediaRequestKind::Video,
            "http://127.0.0.1/high.mp4",
            "avc1.42c01e",
            Some(640),
            Some(360),
            Some(2_000_000),
            160,
        );
        let low = make_request(
            BilibiliMediaRequestKind::Video,
            "http://127.0.0.1/low.mp4",
            "avc1.42c01e",
            Some(320),
            Some(180),
            Some(500_000),
            160,
        );
        let high_audio = make_request(
            BilibiliMediaRequestKind::Audio,
            "http://127.0.0.1/audio.mp4",
            "mp4a.40.2",
            None,
            None,
            Some(128_000),
            160,
        );
        let low_audio = make_request(
            BilibiliMediaRequestKind::Audio,
            "http://127.0.0.1/audio.mp4",
            "mp4a.40.2",
            None,
            None,
            Some(128_000),
            160,
        );
        assert_eq!(high.cache_key.content_id, low.cache_key.content_id);
        assert_eq!(high.cache_key.codecs, low.cache_key.codecs);
        assert_ne!(high.cache_key, low.cache_key);
        assert_eq!(high_audio.cache_key, low_audio.cache_key);
    }

    #[test]
    fn production_media_playlist_requires_matching_multiple_ranges_and_map() {
        let playlist_url = url::Url::parse("http://127.0.0.1/hls/session/video.m3u8").unwrap();
        let playlist = concat!(
            "#EXTM3U\n",
            "#EXT-X-TARGETDURATION:2\n",
            "#EXT-X-MAP:URI=\"video.m4s\",BYTERANGE=\"100@0\"\n",
            "#EXTINF:2.000,\n#EXT-X-BYTERANGE:200@100\nvideo.m4s\n",
            "#EXTINF:2.000,\n#EXT-X-BYTERANGE:200@300\nvideo.m4s\n",
            "#EXT-X-ENDLIST\n",
        );
        assert_eq!(
            validate_media_playlist_body(&playlist_url, playlist, 2).unwrap(),
            2
        );
        assert!(validate_media_playlist_body(&playlist_url, playlist, 3).is_err());
        assert!(validate_media_playlist_body(&playlist_url, playlist, 1).is_err());
        assert!(
            validate_media_playlist_body(
                &playlist_url,
                &playlist.replace("#EXT-X-MAP:", "#EXT-X-MAP-INVALID:"),
                2,
            )
            .is_err()
        );
        assert!(
            validate_media_playlist_body(
                &playlist_url,
                &playlist.replace("#EXT-X-ENDLIST\n", ""),
                2,
            )
            .is_err()
        );
    }

    #[test]
    fn production_media_playlist_accepts_only_matching_segment_queries() {
        let playlist_url = url::Url::parse("http://127.0.0.1/hls/session/video.m3u8").unwrap();
        assert!(valid_hls_byte_range("200@100"));
        assert!(!valid_hls_byte_range("0@100"));
        let prefix = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MAP:URI=\"video.m4s\",BYTERANGE=\"100@0\"\n#EXTINF:2.000,\n#EXT-X-BYTERANGE:200@100\n";
        let suffix = concat!(
            "\n#EXTINF:2.000,\n#EXT-X-BYTERANGE:200@300\n",
            "video.m4s?segment=300-200\n#EXT-X-ENDLIST\n",
        );
        for uri in ["video.m4s", "video.m4s?segment=100-200"] {
            let playlist = format!("{prefix}{uri}{suffix}");
            assert!(validate_media_playlist_body(&playlist_url, &playlist, 2).is_ok());
        }
        for uri in [
            "video.m4s?segment=101-200",
            "video.m4s?segment=100-200&extra=1",
            "other.m4s?segment=100-200",
            "http://example.com/video.m4s?segment=100-200",
            "video.m4s?segment=100-200#fragment",
        ] {
            let playlist = format!("{prefix}{uri}{suffix}");
            assert!(validate_media_playlist_body(&playlist_url, &playlist, 2).is_err());
        }
        let map_with_query = concat!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n",
            "#EXT-X-MAP:URI=\"video.m4s?segment=0-100\",BYTERANGE=\"100@0\"\n",
            "#EXTINF:2.000,\n#EXT-X-BYTERANGE:200@100\n",
            "video.m4s?segment=100-200\n",
            "#EXTINF:2.000,\n#EXT-X-BYTERANGE:200@300\n",
            "video.m4s?segment=300-200\n#EXT-X-ENDLIST\n",
        );
        assert!(validate_media_playlist_body(&playlist_url, map_with_query, 2).is_err());
    }
}

#[cfg(test)]
mod lan_ingress_tests {
    use super::*;

    fn telemetry() -> Arc<LanIngressTelemetry> {
        Arc::new(LanIngressTelemetry {
            session_id: "session-test".to_owned(),
            resource_representations: HashMap::from([
                ("video.m4s".to_owned(), 1),
                ("low-video.m4s".to_owned(), 2),
                ("audio.m4s".to_owned(), 3),
            ]),
            phase: Arc::new(AtomicU8::new(PHASE_BASELINE)),
            epoch: Arc::new(Mutex::new(Instant::now())),
            report: Arc::new(Mutex::new(AcceptanceReport::new(
                NativeCase::ProgressiveAbr,
            ))),
        })
    }

    #[test]
    fn ingress_classification_is_known_id_only_and_range_is_numeric() {
        let telemetry = telemetry();
        let request = Request::builder()
            .uri("/hls/session-test/segments/low-video.m4s?segment=40-12")
            .header(RANGE, "bytes=9-20")
            .body(Body::empty())
            .unwrap();
        assert_eq!(telemetry.classify(&request), Some((2, true, Some((9, 20)))));

        let unknown = Request::builder()
            .uri("/hls/session-test/segments/unknown.m4s?probe=private")
            .header(RANGE, "bytes=9-20")
            .body(Body::empty())
            .unwrap();
        assert_eq!(telemetry.classify(&unknown), None);

        for value in [
            "bytes=9-20,30-40",
            "bytes=9-",
            "bytes=secret-20",
            "bytes=-20",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(RANGE, value.parse().unwrap());
            assert_eq!(numeric_range_header(&headers), None);
        }
    }

    #[test]
    fn ingress_header_samples_are_capped_per_phase() {
        let telemetry = telemetry();
        for _ in 0..12 {
            telemetry.record(1, true, Some((0, 99)), StatusCode::PARTIAL_CONTENT.as_u16());
        }
        let report = telemetry.report.lock().unwrap();
        assert_eq!(
            report.lan_ingress.count_by_phase_and_representation[0][0],
            12
        );
        assert_eq!(
            report
                .lan_ingress
                .segment_query_present_by_phase_and_representation[0][0],
            12
        );
        assert_eq!(report.lan_ingress.header_samples.len(), 8);
        assert!(
            report
                .lan_ingress
                .header_samples
                .iter()
                .all(|sample| { sample.range_start == Some(0) && sample.range_end == Some(99) })
        );
    }

    #[tokio::test]
    async fn body_outcomes_distinguish_eof_error_and_drop() {
        let telemetry = telemetry();
        let context =
            telemetry.record(1, false, Some((0, 3)), StatusCode::PARTIAL_CONTENT.as_u16());
        let mut complete = LanBodyOutcomeStream::new(
            futures_util::stream::iter([
                Ok::<_, axum::Error>(axum::body::Bytes::from_static(b"ab")),
                Ok(axum::body::Bytes::from_static(b"cd")),
            ]),
            context,
            Arc::clone(&telemetry.report),
            Arc::clone(&telemetry.epoch),
        );
        assert_eq!(
            b"ab".as_slice(),
            complete.next().await.unwrap().unwrap().as_ref()
        );
        assert_eq!(
            b"cd".as_slice(),
            complete.next().await.unwrap().unwrap().as_ref()
        );
        assert!(complete.next().await.is_none());

        let context =
            telemetry.record(2, false, Some((0, 3)), StatusCode::PARTIAL_CONTENT.as_u16());
        let error = axum::Error::new(std::io::Error::other("fixture body failure"));
        let mut errored = LanBodyOutcomeStream::new(
            futures_util::stream::iter([Err::<axum::body::Bytes, _>(error)]),
            context,
            Arc::clone(&telemetry.report),
            Arc::clone(&telemetry.epoch),
        );
        assert!(errored.next().await.unwrap().is_err());

        let context =
            telemetry.record(3, false, Some((0, 3)), StatusCode::PARTIAL_CONTENT.as_u16());
        let dropped = LanBodyOutcomeStream::new(
            futures_util::stream::pending::<Result<axum::body::Bytes, axum::Error>>(),
            context,
            Arc::clone(&telemetry.report),
            Arc::clone(&telemetry.epoch),
        );
        drop(dropped);

        let report = telemetry.report.lock().unwrap();
        let outcomes = &report.lan_body_outcomes;
        assert_eq!(outcomes.complete_by_representation, [1, 0, 0]);
        assert_eq!(outcomes.error_by_representation, [0, 1, 0]);
        assert_eq!(outcomes.dropped_by_representation, [0, 0, 1]);
        assert_eq!(outcomes.events.len(), 3);
        assert_eq!(outcomes.events[0].outcome, "complete");
        assert_eq!(outcomes.events[0].bytes_observed, 4);
        assert_eq!(outcomes.events[1].outcome, "error");
        assert_eq!(outcomes.events[2].outcome, "dropped");
        assert!(outcomes.events.iter().all(|event| {
            event.outcome_elapsed_ms >= event.header_elapsed_ms
                && event.header_phase_code == PHASE_BASELINE
        }));
    }

    #[tokio::test]
    async fn empty_error_body_preserves_end_stream_and_exact_zero_size_hint() {
        let telemetry = telemetry();
        let context = telemetry.record(1, false, None, StatusCode::SERVICE_UNAVAILABLE.as_u16());
        let response = empty_response(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(CONTENT_LENGTH), None);

        let original_end_stream = response.body().is_end_stream();
        let original_size_hint = response.body().size_hint().exact();
        assert!(original_end_stream);
        assert_eq!(original_size_hint, Some(0));
        record_empty_lan_body_outcome(
            context,
            Arc::clone(&telemetry.report),
            Arc::clone(&telemetry.epoch),
        );
        assert!(response.body().is_end_stream());
        assert_eq!(response.body().size_hint().exact(), Some(0));
        assert_eq!(response.headers().get(CONTENT_LENGTH), None);

        let report = telemetry.report.lock().unwrap();
        assert_eq!(
            report.lan_body_outcomes.complete_by_representation,
            [1, 0, 0]
        );
        assert_eq!(report.lan_body_outcomes.events[0].bytes_observed, 0);
        assert_eq!(report.lan_body_outcomes.events[0].outcome, "complete");
    }

    #[tokio::test]
    async fn body_outcome_event_cap_reserves_rare_error_and_drop_samples() {
        let telemetry = telemetry();
        for _ in 0..40 {
            let context = telemetry.record(1, false, None, StatusCode::PARTIAL_CONTENT.as_u16());
            let mut complete = LanBodyOutcomeStream::new(
                futures_util::stream::once(async {
                    Ok::<_, axum::Error>(axum::body::Bytes::from_static(b"healthy"))
                }),
                context,
                Arc::clone(&telemetry.report),
                Arc::clone(&telemetry.epoch),
            );
            while complete.next().await.is_some() {}
        }
        let context = telemetry.record(2, false, None, StatusCode::BAD_GATEWAY.as_u16());
        let error = axum::Error::new(std::io::Error::other("fixture body failure"));
        let mut errored = LanBodyOutcomeStream::new(
            futures_util::stream::iter([Err::<axum::body::Bytes, _>(error)]),
            context,
            Arc::clone(&telemetry.report),
            Arc::clone(&telemetry.epoch),
        );
        assert!(errored.next().await.unwrap().is_err());

        let context = telemetry.record(3, false, None, StatusCode::SERVICE_UNAVAILABLE.as_u16());
        drop(LanBodyOutcomeStream::new(
            futures_util::stream::pending::<Result<axum::body::Bytes, axum::Error>>(),
            context,
            Arc::clone(&telemetry.report),
            Arc::clone(&telemetry.epoch),
        ));

        let report = telemetry.report.lock().unwrap();
        let outcomes = &report.lan_body_outcomes;
        assert_eq!(outcomes.events.len(), 32);
        assert_eq!(outcomes.complete_by_representation, [40, 0, 0]);
        assert_eq!(outcomes.error_by_representation, [0, 1, 0]);
        assert_eq!(outcomes.dropped_by_representation, [0, 0, 1]);
        assert!(outcomes.events.iter().any(|event| event.outcome == "error"));
        assert!(
            outcomes
                .events
                .iter()
                .any(|event| event.outcome == "dropped")
        );
        assert_eq!(
            outcomes
                .events
                .iter()
                .filter(|event| event.outcome == "complete")
                .count(),
            30
        );
    }

    #[test]
    fn latest_header_times_are_isolated_and_survive_sample_cap() {
        let telemetry = telemetry();
        {
            let report = telemetry.report.lock().unwrap();
            assert!(
                report
                    .lan_ingress
                    .last_header_elapsed_ms_by_phase_and_representation
                    .iter()
                    .flatten()
                    .all(Option::is_none)
            );
        }
        for _ in 0..12 {
            telemetry.record(1, false, None, StatusCode::PARTIAL_CONTENT.as_u16());
        }
        let latest = telemetry
            .report
            .lock()
            .unwrap()
            .lan_ingress
            .last_header_elapsed_ms_by_phase_and_representation[0][0];
        std::thread::sleep(Duration::from_millis(2));
        telemetry.record(1, false, None, StatusCode::PARTIAL_CONTENT.as_u16());
        telemetry.phase.store(PHASE_BAD_HIGH, Ordering::Release);
        telemetry.record(2, false, None, StatusCode::PARTIAL_CONTENT.as_u16());
        telemetry.phase.store(PHASE_RECOVERY, Ordering::Release);
        telemetry.record(3, false, None, StatusCode::PARTIAL_CONTENT.as_u16());

        let report = telemetry.report.lock().unwrap();
        let times = &report
            .lan_ingress
            .last_header_elapsed_ms_by_phase_and_representation;
        assert!(times[0][0].is_some());
        assert!(times[0][0] > latest);
        assert!(times[1][1].is_some());
        assert!(times[2][2].is_some());
        for (phase, representation) in [(0, 1), (0, 2), (1, 0), (1, 2), (2, 0), (2, 1)] {
            assert_eq!(times[phase][representation], None);
        }
        assert_eq!(report.lan_ingress.header_samples.len(), 10);
    }
}

#[cfg(test)]
mod recovery_observation_tests {
    use super::*;
    use serde_json::json;

    fn observation(timestamp: f64, range_end: f64) -> Value {
        json!({
            "observed_monotonic_seconds": timestamp,
            "player_state": {
                "playhead_seconds": 10.0,
                "preferred_forward_buffer_duration_seconds": 0.0,
                "loaded_time_ranges": {
                    "observations": [{
                        "start_seconds": 0.0,
                        "duration_seconds": range_end,
                        "end_seconds": range_end,
                    }],
                    "source_count": 1,
                    "valid_count": 1,
                    "invalid_count": 0,
                    "truncated": false,
                }
            }
        })
    }

    fn progressive_report(case: NativeCase, recovery_after: f64) -> ReportStore {
        let mut report = AcceptanceReport::new(case);
        report.fault_timeline.recovery_after_monotonic_seconds = Some(recovery_after);
        Arc::new(Mutex::new(report))
    }

    #[test]
    fn report_identity_rejects_wrong_case_scenario_and_source_duration() {
        assert!(
            validate_report_identity(
                NativeCase::ProgressiveAbr,
                ABR_CASE_FULL_NAME,
                "unbuffered_recovery",
                LONG_RECOVERY_MEDIA_DURATION_SECONDS,
            )
            .is_ok()
        );
        assert!(
            validate_report_identity(
                NativeCase::ProgressiveAbr,
                BUFFERED_CASE_FULL_NAME,
                "unbuffered_recovery",
                LONG_RECOVERY_MEDIA_DURATION_SECONDS,
            )
            .is_err()
        );
        assert!(
            validate_report_identity(
                NativeCase::ProgressiveAbr,
                ABR_CASE_FULL_NAME,
                "buffered_continuity",
                LONG_RECOVERY_MEDIA_DURATION_SECONDS,
            )
            .is_err()
        );
        assert!(
            validate_report_identity(
                NativeCase::ProgressiveAbr,
                ABR_CASE_FULL_NAME,
                "unbuffered_recovery",
                BUFFERED_MEDIA_DURATION_SECONDS,
            )
            .is_err()
        );
    }

    #[test]
    fn buffered_end_coverage_accepts_a_finite_250ms_aac_tail() {
        let evidence = validate_player_observation(&observation(20.0, 159.8), 160.0).unwrap();
        assert!(evidence.range_end >= 160.0 - 0.25);
        assert!(validate_player_observation(&observation(20.0, 159.75), 160.0).is_ok());
    }

    #[test]
    fn passive_observation_rejects_playhead_or_ranges_beyond_source_duration() {
        assert!(validate_player_observation(&observation(20.0, 160.01), 160.0).is_err());

        let mut beyond_playhead = observation(20.0, 160.0);
        beyond_playhead["player_state"]["playhead_seconds"] = json!(160.01);
        assert!(validate_player_observation(&beyond_playhead, 160.0).is_err());
    }

    #[test]
    fn long_recovery_requires_twenty_seconds_of_unbuffered_source() {
        let report = progressive_report(NativeCase::ProgressiveAbr, 100.0);
        let probe = json!({
            "item_duration_seconds": 3600.0,
            "recovery_observation": observation(101.0, 3580.0),
        });
        assert!(
            validate_recovery_observations(NativeCase::ProgressiveAbr, &probe, &report).is_ok()
        );

        let insufficient = json!({
            "item_duration_seconds": 3600.0,
            "recovery_observation": observation(101.0, 3580.01),
        });
        assert!(
            validate_recovery_observations(NativeCase::ProgressiveAbr, &insufficient, &report,)
                .is_err()
        );
    }

    #[test]
    fn buffered_admission_requires_recovery_window_and_steady_end_coverage() {
        let report = progressive_report(NativeCase::BufferedContinuity, 100.0);
        let probe = json!({
            "item_duration_seconds": 160.0,
            "steps": [{"name": "play", "status": "completed", "elapsed_seconds": 110.0}],
            "recovery_observation": observation(101.0, 159.8),
            "steady_play_end_observation": observation(211.0, 159.75),
        });
        assert!(
            validate_recovery_observations(NativeCase::BufferedContinuity, &probe, &report,)
                .is_ok()
        );

        let stale = json!({
            "item_duration_seconds": 160.0,
            "steps": [{"name": "play", "status": "completed", "elapsed_seconds": 110.0}],
            "recovery_observation": observation(105.01, 159.8),
            "steady_play_end_observation": observation(211.0, 159.8),
        });
        assert!(
            validate_recovery_observations(NativeCase::BufferedContinuity, &stale, &report,)
                .is_err()
        );

        let incomplete = json!({
            "item_duration_seconds": 160.0,
            "steps": [{"name": "play", "status": "completed", "elapsed_seconds": 109.99}],
            "recovery_observation": observation(101.0, 159.8),
            "steady_play_end_observation": observation(211.0, 159.8),
        });
        assert!(
            validate_recovery_observations(NativeCase::BufferedContinuity, &incomplete, &report,)
                .is_err()
        );
    }

    #[test]
    fn recovery_marker_is_fresh_empty_regular_and_mode_600() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("recovery.trigger");
        create_empty_private_marker(&marker).unwrap();
        let metadata = std::fs::symlink_metadata(&marker).unwrap();
        assert!(metadata.file_type().is_file());
        assert_eq!(metadata.len(), 0);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert!(create_empty_private_marker(&marker).is_err());
    }

    #[test]
    fn loaded_ranges_are_not_joined_across_gaps() {
        let mut gapped = observation(211.0, 10.0);
        gapped["player_state"]["playhead_seconds"] = json!(5.0);
        gapped["player_state"]["loaded_time_ranges"]["observations"] = json!([
            {"start_seconds": 0.0, "duration_seconds": 10.0, "end_seconds": 10.0},
            {"start_seconds": 100.0, "duration_seconds": 60.0, "end_seconds": 160.0},
        ]);
        gapped["player_state"]["loaded_time_ranges"]["source_count"] = json!(2);
        gapped["player_state"]["loaded_time_ranges"]["valid_count"] = json!(2);

        let evidence = validate_player_observation(&gapped, 160.0).unwrap();
        assert_eq!(evidence.range_end, 10.0);
    }
}

#[cfg(test)]
mod prewarm_retry_tests {
    use super::*;
    use crate::{
        bbdown_adapter::{
            BilibiliHttpHeader, BilibiliMediaCacheKey, BilibiliMediaRequest,
            BilibiliMediaRequestKind,
        },
        hls::{HlsMediaResource, HlsVariant},
        media_prewarm_comparison_tests::{
            PrewarmExecution, PrewarmFailureCategory, PrewarmMode, PrewarmOutcome, PrewarmPlan,
            PrewarmResourceIds, RecoverySnapshot, SelectedRange,
        },
    };
    use std::{convert::Infallible, sync::atomic::AtomicUsize};
    use tokio::sync::Notify;

    struct LoopbackUpstream {
        bytes: Arc<Vec<u8>>,
        foreground_started: Notify,
        foreground_release: Notify,
        background_requests: AtomicUsize,
        fail_background: bool,
    }

    async fn start_upstream(
        total: u64,
        fail_background: bool,
    ) -> (String, Arc<LoopbackUpstream>, JoinHandle<()>) {
        let state = Arc::new(LoopbackUpstream {
            bytes: Arc::new(vec![0x5a; total as usize]),
            foreground_started: Notify::new(),
            foreground_release: Notify::new(),
            background_requests: AtomicUsize::new(0),
            fail_background,
        });
        let app = Router::new()
            .route("/video.m4s", get(held_foreground_range))
            .route("/audio.m4s", get(background_range))
            .with_state(Arc::clone(&state));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener should bind");
        let address = listener
            .local_addr()
            .expect("loopback listener should expose its address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("loopback server should run");
        });
        (format!("http://{address}"), state, server)
    }

    fn parse_range(headers: &HeaderMap, total: usize) -> Option<(usize, usize)> {
        let value = headers.get(RANGE)?.to_str().ok()?.strip_prefix("bytes=")?;
        let (start, inclusive_end) = value.split_once('-')?;
        let start = start.parse::<usize>().ok()?;
        let end = inclusive_end.parse::<usize>().ok()?.checked_add(1)?;
        (start < end && end <= total).then_some((start, end))
    }

    fn partial_response(start: usize, end: usize, total: usize, body: Body) -> Response<Body> {
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_LENGTH, (end - start).to_string())
            .header(CONTENT_RANGE, format!("bytes {start}-{}/{total}", end - 1))
            .header(reqwest::header::ETAG, "\"prewarm-retry-v1\"")
            .body(body)
            .expect("loopback range response should build")
    }

    async fn held_foreground_range(
        State(state): State<Arc<LoopbackUpstream>>,
        headers: HeaderMap,
    ) -> Response<Body> {
        let Some((start, end)) = parse_range(&headers, state.bytes.len()) else {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .body(Body::empty())
                .expect("invalid range response should build");
        };
        let payload = state.bytes[start..end].to_vec();
        let total = state.bytes.len();
        let release = Arc::clone(&state);
        let body = Body::from_stream(futures_util::stream::once(async move {
            release.foreground_started.notify_one();
            release.foreground_release.notified().await;
            Ok::<_, Infallible>(payload)
        }));
        partial_response(start, end, total, body)
    }

    async fn background_range(
        State(state): State<Arc<LoopbackUpstream>>,
        headers: HeaderMap,
    ) -> Response<Body> {
        state.background_requests.fetch_add(1, Ordering::SeqCst);
        if state.fail_background {
            return Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::empty())
                .expect("failure response should build");
        }
        let Some((start, end)) = parse_range(&headers, state.bytes.len()) else {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .body(Body::empty())
                .expect("invalid range response should build");
        };
        partial_response(
            start,
            end,
            state.bytes.len(),
            Body::from(state.bytes[start..end].to_vec()),
        )
    }

    fn resource(
        id: &str,
        url: String,
        kind: BilibiliMediaRequestKind,
        total: u64,
        source_hash: &str,
    ) -> HlsMediaResource {
        HlsMediaResource {
            id: id.to_owned(),
            request: BilibiliMediaRequest {
                kind,
                stream_id: None,
                url,
                backup_urls: Vec::new(),
                headers: vec![BilibiliHttpHeader {
                    name: "referer".to_owned(),
                    value: "https://www.bilibili.com".to_owned(),
                }],
                mime_type: Some("video/mp4".to_owned()),
                codecs: Some(match kind {
                    BilibiliMediaRequestKind::Audio => "mp4a.40.2".to_owned(),
                    _ => "avc1.640028".to_owned(),
                }),
                bandwidth: Some(1_000_000),
                width: Some(1920),
                height: Some(1080),
                frame_rate: Some("30".to_owned()),
                size: Some(total),
                duration_seconds: Some(60),
                cache_key: BilibiliMediaCacheKey {
                    content_id: "prewarm-retry-test".to_owned(),
                    media_kind: kind,
                    stream_id: None,
                    codecs: Some(match kind {
                        BilibiliMediaRequestKind::Audio => "mp4a.40.2".to_owned(),
                        _ => "avc1.640028".to_owned(),
                    }),
                    source_hash: source_hash.to_owned(),
                },
            },
        }
    }

    fn session(base_url: &str, total: u64) -> HlsPlaybackSession {
        HlsPlaybackSession {
            id: "prewarm-retry-session".to_owned(),
            title: "prewarm retry fixture".to_owned(),
            accepted_identity: None,
            variant: HlsVariant {
                id: "h264".to_owned(),
                bandwidth: 1_000_000,
                codecs: vec!["avc1.640028".to_owned(), "mp4a.40.2".to_owned()],
                width: Some(1920),
                height: Some(1080),
                duration_seconds: 60,
                video: resource(
                    "video.m4s",
                    format!("{base_url}/video.m4s"),
                    BilibiliMediaRequestKind::Video,
                    total,
                    "prewarm-video-source",
                ),
                audio: Some(resource(
                    "audio.m4s",
                    format!("{base_url}/audio.m4s"),
                    BilibiliMediaRequestKind::Audio,
                    total,
                    "prewarm-audio-source",
                )),
            },
            alternate_variants: Vec::new(),
            advertise_alternate_variants: true,
            abr: Default::default(),
            variants: Vec::new(),
            transcoding: Default::default(),
            effective_policy: PlaybackPolicy::default(),
        }
    }

    fn plan(selected_end: u64, transfer_end: u64, total: u64) -> PrewarmPlan {
        PrewarmPlan {
            start_seconds: 60.0,
            end_seconds: 80.0,
            planned_bytes: selected_end,
            ranges: vec![SelectedRange {
                resource_id: "audio.m4s".to_owned(),
                start: 0,
                end: selected_end,
            }],
            resource_ids: PrewarmResourceIds {
                video: "video.m4s".to_owned(),
                audio: "audio.m4s".to_owned(),
            },
            transfer_ranges: vec![SelectedRange {
                resource_id: "audio.m4s".to_owned(),
                start: 0,
                end: transfer_end,
            }],
            planned_transfer_bytes: transfer_end,
            video_total_length: total,
            audio_total_length: total,
        }
    }

    fn report_store() -> ReportStore {
        Arc::new(Mutex::new(AcceptanceReport::new(
            NativeCase::SinglePlayerPrewarmTreatment,
        )))
    }

    fn recovery_snapshot() -> RecoverySnapshot {
        RecoverySnapshot {
            schema_version: 1,
            observed_monotonic_seconds: 100.0,
            playhead_seconds: 60.0,
            loaded_end_seconds: 80.0,
        }
    }

    async fn run_prewarm(
        cache: HlsCacheStore,
        client: reqwest::Client,
        session: HlsPlaybackSession,
        plan: PrewarmPlan,
        report: ReportStore,
        deadline: Duration,
    ) -> (Result<(), String>, PrewarmExecution) {
        let mut execution = PrewarmExecution::initial(0);
        let started = Instant::now();
        let snapshot = recovery_snapshot();
        let result = prewarm_selected_ranges(
            PrewarmRunContext {
                cache: &cache,
                client: &client,
                session_id: &session.id,
                high: &session.variant.video,
                audio: session
                    .variant
                    .audio
                    .as_ref()
                    .expect("fixture audio should exist"),
                plan: &plan,
                deadline: tokio::time::Instant::now() + deadline,
                report: &report,
                mode: PrewarmMode::On,
                snapshot: &snapshot,
                started,
            },
            &mut execution,
        )
        .await;
        (result, execution)
    }

    fn spawn_foreground(
        cache: HlsCacheStore,
        client: reqwest::Client,
        session: HlsPlaybackSession,
    ) -> JoinHandle<
        Result<crate::hls_range_cache::HlsReadyRange, crate::hls_range_cache::HlsRangeError>,
    > {
        tokio::spawn(async move {
            cache
                .ensure_resource_range(
                    &client,
                    &session.id,
                    &session.variant.video,
                    0..64 * 1024,
                    HlsRangePriority::Foreground,
                    &|| HlsCacheFillControl::Continue,
                )
                .await
        })
    }

    async fn wait_for_preempted_attempt(report: &ReportStore) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let preempted = report
                    .lock()
                    .expect("report lock should be available")
                    .prewarm_comparison
                    .as_ref()
                    .is_some_and(|comparison| comparison.preempted_operation_count > 0);
                if preempted {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("prewarm should publish its preempted attempt");
    }

    fn fixture_cache(session: &HlsPlaybackSession) -> (tempfile::TempDir, HlsCacheStore) {
        let temp = tempfile::tempdir().expect("cache temp dir should be created");
        let root = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let cache = HlsCacheStore::new(root).with_range_budget(32 * 1024 * 1024);
        cache
            .save_session(session)
            .expect("HLS fixture session should be saved");
        (temp, cache)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prewarm_caller_waits_for_quiescence_before_retrying_large_span() {
        const TOTAL: u64 = 6 * 1024 * 1024;
        const SPAN: u64 = 5 * 1024 * 1024 + 512 * 1024;
        const _: () = {
            assert!(SPAN * 2 <= 16 * 1024 * 1024);
            assert!(SPAN * 3 > 16 * 1024 * 1024);
        };

        let (base_url, upstream, server) = start_upstream(TOTAL, false).await;
        let session = session(&base_url, TOTAL);
        let (_temp, cache) = fixture_cache(&session);
        let client = reqwest::Client::new();
        let foreground = spawn_foreground(cache.clone(), client.clone(), session.clone());
        tokio::time::timeout(
            Duration::from_secs(2),
            upstream.foreground_started.notified(),
        )
        .await
        .expect("foreground loopback request should be held");
        assert_eq!(cache.foreground_activity_count_for_test(), 1);

        let report = report_store();
        let prewarm = tokio::spawn(run_prewarm(
            cache.clone(),
            client,
            session.clone(),
            plan(SPAN, SPAN, TOTAL),
            Arc::clone(&report),
            Duration::from_secs(5),
        ));
        wait_for_preempted_attempt(&report).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let held = report
            .lock()
            .expect("report lock should be available")
            .prewarm_comparison
            .clone()
            .expect("prewarm progress should be published");
        assert_eq!(held.attempted_ranges.len(), 1);
        assert_eq!(held.operation_count, 1);
        assert_eq!(held.operation_requested_bytes, SPAN);
        assert_eq!(held.preempted_operation_count, 1);
        assert_eq!(held.foreground_defer_count, 1);
        assert_eq!(upstream.background_requests.load(Ordering::SeqCst), 0);
        assert_eq!(cache.foreground_activity_count_for_test(), 1);

        upstream.foreground_release.notify_one();
        let (result, execution) = tokio::time::timeout(Duration::from_secs(8), prewarm)
            .await
            .expect("prewarm should complete after foreground release")
            .expect("prewarm task should join");
        result.expect("prewarm caller should succeed after quiescence");
        tokio::time::timeout(Duration::from_secs(2), foreground)
            .await
            .expect("foreground request should finish after release")
            .expect("foreground task should join")
            .expect("foreground range should complete");

        assert_eq!(execution.attempted_ranges.len(), 2);
        assert_eq!(execution.operation_count, 2);
        assert_eq!(execution.operation_requested_bytes, SPAN * 2);
        assert!(execution.operation_requested_bytes <= 16 * 1024 * 1024);
        assert_eq!(
            execution.attempted_ranges[0].outcome,
            PrewarmOutcome::Preempted
        );
        assert_eq!(
            execution.attempted_ranges[1].outcome,
            PrewarmOutcome::Completed
        );
        assert_eq!(execution.completed_transfer_bytes, SPAN);
        assert_eq!(execution.completed_transfer_count, 1);
        assert_eq!(execution.preempted_operation_count, 1);
        assert_eq!(execution.foreground_defer_count, 1);
        assert!(execution.completed);
        assert!(execution.failure.category.is_none());
        assert!(upstream.background_requests.load(Ordering::SeqCst) > 0);
        assert_eq!(cache.foreground_activity_count_for_test(), 0);
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prewarm_caller_does_not_retry_upstream_status_and_keeps_partial_failure() {
        const TOTAL: u64 = 1024 * 1024;
        let (base_url, upstream, server) = start_upstream(TOTAL, true).await;
        let session = session(&base_url, TOTAL);
        let (_temp, cache) = fixture_cache(&session);
        let report = report_store();
        let (result, execution) = run_prewarm(
            cache,
            reqwest::Client::new(),
            session,
            plan(32, 32, TOTAL),
            Arc::clone(&report),
            Duration::from_secs(2),
        )
        .await;

        assert!(result.unwrap_err().contains("category=upstream_status"));
        assert_eq!(upstream.background_requests.load(Ordering::SeqCst), 1);
        assert_eq!(execution.operation_count, 1);
        assert_eq!(execution.attempted_ranges.len(), 1);
        assert_eq!(execution.operation_requested_bytes, 32);
        assert_eq!(
            execution.attempted_ranges[0].outcome,
            PrewarmOutcome::UpstreamStatus
        );
        assert_eq!(execution.completed_transfer_bytes, 0);
        assert_eq!(execution.completed_transfer_count, 0);
        assert_eq!(execution.preempted_operation_count, 0);
        assert_eq!(execution.foreground_defer_count, 0);
        assert!(!execution.completed);
        assert_eq!(
            execution.failure.category,
            Some(PrewarmFailureCategory::UpstreamStatus)
        );
        assert_eq!(execution.failure.resource_id.as_deref(), Some("audio.m4s"));
        assert_eq!(execution.failure.start, Some(0));
        assert_eq!(execution.failure.end, Some(32));
        assert_eq!(execution.failure.upstream_status, Some(503));
        assert!(execution.elapsed_seconds > 0.0);
        let serialized = serde_json::to_string(
            &report
                .lock()
                .expect("report lock should be available")
                .prewarm_comparison,
        )
        .expect("partial report should serialize");
        assert!(!serialized.contains(&base_url));
        assert!(serialized.contains("\"upstream_status\":503"));
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prewarm_caller_rejects_overbudget_retry_without_fake_attempt() {
        const TOTAL: u64 = 10 * 1024 * 1024;
        const SELECTED: u64 = 8 * 1024 * 1024;
        const TRANSFER: u64 = 9 * 1024 * 1024;
        let (base_url, upstream, server) = start_upstream(TOTAL, false).await;
        let session = session(&base_url, TOTAL);
        let (_temp, cache) = fixture_cache(&session);
        let client = reqwest::Client::new();
        let foreground = spawn_foreground(cache.clone(), client.clone(), session.clone());
        tokio::time::timeout(
            Duration::from_secs(2),
            upstream.foreground_started.notified(),
        )
        .await
        .expect("foreground loopback request should be held");

        let report = report_store();
        let (result, execution) = run_prewarm(
            cache.clone(),
            client,
            session,
            plan(SELECTED, TRANSFER, TOTAL),
            report,
            Duration::from_secs(2),
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .contains("category=byte_budget_exceeded")
        );
        assert_eq!(execution.attempted_ranges.len(), 1);
        assert_eq!(execution.operation_count, 1);
        assert_eq!(execution.operation_requested_bytes, TRANSFER);
        assert_eq!(
            execution.attempted_ranges[0].outcome,
            PrewarmOutcome::Preempted
        );
        assert_eq!(execution.preempted_operation_count, 1);
        assert_eq!(execution.foreground_defer_count, 0);
        assert_eq!(
            execution.failure.category,
            Some(PrewarmFailureCategory::ByteBudgetExceeded)
        );
        assert_eq!(execution.failure.resource_id.as_deref(), Some("audio.m4s"));
        assert_eq!(execution.failure.start, Some(0));
        assert_eq!(execution.failure.end, Some(TRANSFER));
        assert_eq!(upstream.background_requests.load(Ordering::SeqCst), 0);

        upstream.foreground_release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), foreground)
            .await
            .expect("foreground request should finish after release")
            .expect("foreground task should join")
            .expect("foreground range should complete");
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prewarm_caller_deadline_preserves_partial_attempt_progress() {
        const TOTAL: u64 = 1024 * 1024;
        let (base_url, upstream, server) = start_upstream(TOTAL, false).await;
        let session = session(&base_url, TOTAL);
        let (_temp, cache) = fixture_cache(&session);
        let client = reqwest::Client::new();
        let foreground = spawn_foreground(cache.clone(), client.clone(), session.clone());
        tokio::time::timeout(
            Duration::from_secs(2),
            upstream.foreground_started.notified(),
        )
        .await
        .expect("foreground loopback request should be held");

        let report = report_store();
        let (result, execution) = run_prewarm(
            cache.clone(),
            client,
            session,
            plan(32, 32, TOTAL),
            Arc::clone(&report),
            Duration::from_millis(150),
        )
        .await;
        assert!(result.unwrap_err().contains("category=deadline_exceeded"));
        assert_eq!(execution.attempted_ranges.len(), 1);
        assert_eq!(execution.operation_count, 1);
        assert_eq!(execution.operation_requested_bytes, 32);
        assert_eq!(
            execution.attempted_ranges[0].outcome,
            PrewarmOutcome::Preempted
        );
        assert_eq!(execution.preempted_operation_count, 1);
        assert_eq!(execution.foreground_defer_count, 1);
        assert_eq!(
            execution.failure.category,
            Some(PrewarmFailureCategory::DeadlineExceeded)
        );
        assert_eq!(execution.failure.resource_id.as_deref(), Some("audio.m4s"));
        assert_eq!(execution.failure.start, Some(0));
        assert_eq!(execution.failure.end, Some(32));
        assert!(execution.elapsed_seconds >= 0.1);
        assert!(!execution.completed);
        assert_eq!(upstream.background_requests.load(Ordering::SeqCst), 0);
        let persisted = report
            .lock()
            .expect("report lock should be available")
            .prewarm_comparison
            .clone()
            .expect("partial report should be persisted in memory");
        assert_eq!(persisted.operation_count, 1);
        assert_eq!(persisted.operation_requested_bytes, 32);
        assert_eq!(persisted.elapsed_seconds, execution.elapsed_seconds);
        assert_eq!(
            persisted.failure.category,
            Some(PrewarmFailureCategory::DeadlineExceeded)
        );

        upstream.foreground_release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), foreground)
            .await
            .expect("foreground request should finish after release")
            .expect("foreground task should join")
            .expect("foreground range should complete");
        server.abort();
    }
}
