use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::bbdown_adapter::{BilibiliMediaRequest, BilibiliMediaRequestKind};

const VERSION: u8 = 1;
const HOST_TTL_SECS: u64 = 24 * 60 * 60;
const REPRESENTATION_TTL_SECS: u64 = 6 * 60 * 60;
const BASE_COOLDOWN_SECS: u64 = 10;
const MAX_COOLDOWN_SECS: u64 = 10 * 60;
const MAX_HOSTS: usize = 256;
const MAX_REPRESENTATIONS: usize = 512;
const MAX_ORIGINS_PER_REPRESENTATION: usize = 3;
const MAX_FILE_BYTES: usize = 192 * 1024;
const MAX_CANDIDATES: usize = 32;
const MAX_CONTENT_ID_BYTES: usize = 256;
const MAX_CODEC_BYTES: usize = 128;
const PERSISTENCE_THROTTLE: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub(crate) struct CdnHistory {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<HistoryState>,
    path: Option<PathBuf>,
    writer: Mutex<WriterControl>,
    writer_idle: tokio::sync::watch::Sender<bool>,
    shutdown_lock: tokio::sync::Mutex<()>,
    last_write: Mutex<Option<Instant>>,
    persistence_lock: Mutex<()>,
    persisted_sequence: Mutex<u64>,
}

#[derive(Default)]
struct WriterControl {
    admission_closed: bool,
    running: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct HistoryState {
    version: u8,
    sequence: u64,
    hosts: BTreeMap<String, HostRecord>,
    representations: BTreeMap<String, RepresentationRecord>,
}

#[derive(Clone, Serialize, Deserialize)]
struct HostRecord {
    updated_at: u64,
    event_sequence: u64,
    latest: HealthEvent,
    failures: u32,
    cooldown_until: Option<u64>,
    throughput_bps: Option<u64>,
    latency_micros: Option<u64>,
    range_supported: Option<bool>,
}

#[derive(Clone, Serialize, Deserialize)]
struct RepresentationRecord {
    digest: String,
    origin: String,
    updated_at: u64,
    event_sequence: u64,
    latest: HealthEvent,
    failures: u32,
    cooldown_until: Option<u64>,
    quarantined_until: Option<u64>,
    throughput_bps: Option<u64>,
    latency_micros: Option<u64>,
    range_supported: Option<bool>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
enum HealthEvent {
    Success,
    TransportFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CdnObservationSource {
    Playback,
    // Retained for typed probe observations; PR3 does not run active probes.
    #[allow(dead_code)]
    Probe,
    DownloadShard,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CdnObservationOutcome {
    Complete,
    Partial,
    Cancelled,
    ConnectionFailure,
    Timeout,
    ServerFailure,
    SourceUnavailable,
    IntegrityMismatch,
}

#[derive(Clone, Debug)]
pub(crate) struct CdnObservation {
    pub source: CdnObservationSource,
    pub outcome: CdnObservationOutcome,
    pub bytes: u64,
    pub elapsed: Option<Duration>,
    pub first_byte_latency: Option<Duration>,
    pub range_supported: Option<bool>,
}

impl CdnObservation {
    pub(crate) fn new(
        source: CdnObservationSource,
        outcome: CdnObservationOutcome,
        bytes: u64,
    ) -> Self {
        Self {
            source,
            outcome,
            bytes,
            elapsed: None,
            first_byte_latency: None,
            range_supported: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn playback_complete(
        bytes: u64,
        elapsed: Duration,
        first_byte_latency: Duration,
        range_supported: Option<bool>,
    ) -> Self {
        Self {
            source: CdnObservationSource::Playback,
            outcome: CdnObservationOutcome::Complete,
            bytes,
            elapsed: Some(elapsed),
            first_byte_latency: Some(first_byte_latency),
            range_supported,
        }
    }

    #[cfg(test)]
    pub(crate) fn probe_complete(bytes: u64, elapsed: Duration) -> Self {
        Self {
            source: CdnObservationSource::Probe,
            outcome: CdnObservationOutcome::Complete,
            bytes,
            elapsed: Some(elapsed),
            first_byte_latency: None,
            range_supported: None,
        }
    }

    pub(crate) fn download_shard(bytes: u64) -> Self {
        Self::new(
            CdnObservationSource::DownloadShard,
            CdnObservationOutcome::Complete,
            bytes,
        )
    }

    #[cfg(test)]
    pub(crate) fn failure(source: CdnObservationSource, outcome: CdnObservationOutcome) -> Self {
        Self::new(source, outcome, 0)
    }
}

impl Default for CdnHistory {
    fn default() -> Self {
        Self::memory(HistoryState {
            version: VERSION,
            ..HistoryState::default()
        })
    }
}

impl CdnHistory {
    pub(crate) fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let (state, path) = match read_state(&path) {
            Ok(Some(state)) => (state, Some(path)),
            Ok(None) => (
                HistoryState {
                    version: VERSION,
                    ..HistoryState::default()
                },
                Some(path),
            ),
            Err(_) => (
                HistoryState {
                    version: VERSION,
                    ..HistoryState::default()
                },
                None,
            ),
        };
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(state),
                path,
                writer: Mutex::new(WriterControl::default()),
                writer_idle: tokio::sync::watch::channel(true).0,
                shutdown_lock: tokio::sync::Mutex::new(()),
                last_write: Mutex::new(None),
                persistence_lock: Mutex::new(()),
                persisted_sequence: Mutex::new(0),
            }),
        }
    }

    fn memory(state: HistoryState) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(state),
                path: None,
                writer: Mutex::new(WriterControl::default()),
                writer_idle: tokio::sync::watch::channel(true).0,
                shutdown_lock: tokio::sync::Mutex::new(()),
                last_write: Mutex::new(None),
                persistence_lock: Mutex::new(()),
                persisted_sequence: Mutex::new(0),
            }),
        }
    }

    pub(crate) fn rank_request(&self, request: &BilibiliMediaRequest) -> Vec<String> {
        self.rank_request_at(request, now_seconds())
    }

    pub(crate) fn rank_request_for_range(
        &self,
        request: &BilibiliMediaRequest,
        chunk_index: u64,
    ) -> Vec<String> {
        self.rank_request_for_range_at(request, chunk_index, now_seconds())
    }

    pub(crate) fn record_request(
        &self,
        request: &BilibiliMediaRequest,
        url: &str,
        observation: CdnObservation,
    ) {
        self.record_request_at(request, url, observation, now_seconds());
    }

    pub(crate) fn record_origin(&self, url: &str, observation: CdnObservation) {
        self.record_origin_at(url, observation, now_seconds());
    }

    pub(crate) async fn shutdown_and_wait(&self) {
        let _shutdown_guard = self.shared.shutdown_lock.lock().await;
        let mut writer_idle = self.shared.writer_idle.subscribe();
        {
            let mut writer = self
                .shared
                .writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            writer.admission_closed = true;
            if !writer.running {
                self.shared.writer_idle.send_replace(true);
            }
        }
        while !*writer_idle.borrow() {
            if writer_idle.changed().await.is_err() {
                break;
            }
        }
        let Some(path) = self.shared.path.clone() else {
            return;
        };
        let snapshot = self.snapshot();
        let shared = Arc::clone(&self.shared);
        let _ =
            tokio::task::spawn_blocking(move || persist_snapshot(&shared, &path, &snapshot)).await;
    }

    #[cfg(test)]
    fn flush(&self) -> io::Result<()> {
        let Some(path) = self.shared.path.as_deref() else {
            return Ok(());
        };
        let snapshot = self.snapshot();
        persist_snapshot(&self.shared, path, &snapshot)?;
        if let Ok(mut last_write) = self.shared.last_write.lock() {
            *last_write = Some(Instant::now());
        }
        Ok(())
    }

    fn rank_request_at(&self, request: &BilibiliMediaRequest, now: u64) -> Vec<String> {
        let candidates = request_candidates(request);
        let digest = representation_digest(request);
        let mut state = self.lock_state();
        prune(&mut state, now);
        let mut remaining = candidates.into_iter();
        let bounded_prefix = remaining.by_ref().take(MAX_CANDIDATES).collect::<Vec<_>>();
        let unchanged_tail = remaining.collect::<Vec<_>>();
        let mut ranked = bounded_prefix
            .into_iter()
            .enumerate()
            .map(|(index, url)| {
                let origin = canonical_origin(&url);
                let rep = digest.as_ref().and_then(|digest| {
                    origin.as_ref().and_then(|origin| {
                        state
                            .representations
                            .get(&representation_map_key(digest, origin))
                    })
                });
                let host = origin.as_ref().and_then(|origin| state.hosts.get(origin));
                let quarantine = rep
                    .and_then(|entry| entry.quarantined_until)
                    .is_some_and(|until| now < until);
                let cooldown = rep
                    .and_then(|entry| entry.cooldown_until)
                    .is_some_and(|until| now < until)
                    || host
                        .and_then(|entry| entry.cooldown_until)
                        .is_some_and(|until| now < until);
                let host_failed = host.is_some_and(|entry| !entry.latest.is_success());
                let known_healthy = rep.is_some_and(|entry| {
                    entry.latest.is_success() && !host_failed && !cooldown && !quarantine
                }) || (rep.is_none()
                    && !host_failed
                    && host.is_some_and(|entry| entry.latest.is_success() && !cooldown));
                let tier = if quarantine || cooldown {
                    2
                } else if known_healthy {
                    0
                } else {
                    1
                };
                let metrics = rep
                    .filter(|entry| entry.latest.is_success() && !host_failed)
                    .map(|entry| (entry.throughput_bps, entry.latency_micros))
                    .or_else(|| {
                        host.filter(|entry| entry.latest.is_success() && !host_failed)
                            .map(|entry| (entry.throughput_bps, entry.latency_micros))
                    });
                let (throughput, latency) = metrics.unwrap_or((None, None));
                let recent = rep
                    .filter(|entry| {
                        entry.latest.is_success() && !host_failed && !cooldown && !quarantine
                    })
                    .map_or((0, 0), |entry| (entry.updated_at, entry.event_sequence));
                (tier, throughput, latency, recent, index, url)
            })
            .collect::<Vec<_>>()
            .tap_sort();
        ranked.extend(unchanged_tail);
        ranked
    }

    fn rank_request_for_range_at(
        &self,
        request: &BilibiliMediaRequest,
        chunk_index: u64,
        now: u64,
    ) -> Vec<String> {
        let ranked = self.rank_request_at(request, now);
        let digest = representation_digest(request);
        let mut state = self.lock_state();
        prune(&mut state, now);
        let mut origins = HashSet::new();
        let mut selected = Vec::new();
        for (index, url) in ranked.iter().take(MAX_CANDIDATES).enumerate() {
            let Some(origin) = canonical_origin(url) else {
                continue;
            };
            let host = state.hosts.get(&origin);
            let representation = digest.as_ref().and_then(|digest| {
                state
                    .representations
                    .get(&representation_map_key(digest, &origin))
            });
            let blocked = host
                .and_then(|entry| entry.cooldown_until)
                .is_some_and(|until| now < until)
                || representation
                    .and_then(|entry| entry.cooldown_until)
                    .is_some_and(|until| now < until)
                || representation
                    .and_then(|entry| entry.quarantined_until)
                    .is_some_and(|until| now < until)
                || host.is_some_and(|entry| entry.range_supported == Some(false))
                || representation.is_some_and(|entry| entry.range_supported == Some(false));
            if !blocked && origins.insert(origin) {
                selected.push(index);
                if selected.len() == MAX_ORIGINS_PER_REPRESENTATION {
                    break;
                }
            }
        }
        drop(state);
        if selected.len() < 2 {
            return ranked;
        }

        // This is bounded exploration, not identity proof; the range store
        // must validate compatibility before publishing any cross-origin bytes.
        let rotation = (chunk_index % selected.len() as u64) as usize;
        let mut ordered = Vec::with_capacity(ranked.len());
        for offset in 0..selected.len() {
            ordered.push(ranked[selected[(rotation + offset) % selected.len()]].clone());
        }
        ordered.extend(
            ranked
                .into_iter()
                .enumerate()
                .filter_map(|(index, url)| (!selected.contains(&index)).then_some(url)),
        );
        ordered
    }

    fn record_request_at(
        &self,
        request: &BilibiliMediaRequest,
        url: &str,
        observation: CdnObservation,
        now: u64,
    ) {
        let digest = representation_digest(request);
        self.record_at(url, observation, digest.as_deref(), now);
    }

    fn record_origin_at(&self, url: &str, observation: CdnObservation, now: u64) {
        self.record_at(url, observation, None, now);
    }

    fn record_at(&self, url: &str, observation: CdnObservation, digest: Option<&str>, now: u64) {
        let Some(origin) = canonical_origin(url) else {
            return;
        };
        let mut state = self.lock_state();
        prune(&mut state, now);
        state.sequence = state.sequence.saturating_add(1);
        let sequence = state.sequence;
        let transport_failure = matches!(
            observation.outcome,
            CdnObservationOutcome::ConnectionFailure
                | CdnObservationOutcome::Timeout
                | CdnObservationOutcome::ServerFailure
        );
        let completed = observation.outcome == CdnObservationOutcome::Complete;
        let integrity_mismatch = observation.outcome == CdnObservationOutcome::IntegrityMismatch;
        if transport_failure || completed {
            let entry = state
                .hosts
                .entry(origin.clone())
                .or_insert_with(|| HostRecord {
                    updated_at: now,
                    event_sequence: sequence,
                    latest: if completed {
                        HealthEvent::Success
                    } else {
                        HealthEvent::TransportFailure
                    },
                    failures: 0,
                    cooldown_until: None,
                    throughput_bps: None,
                    latency_micros: None,
                    range_supported: None,
                });
            update_host(entry, &observation, now, sequence, transport_failure);
        }
        if let Some(digest) =
            digest.filter(|_| completed || transport_failure || integrity_mismatch)
        {
            let key = representation_map_key(digest, &origin);
            let entry = state
                .representations
                .entry(key)
                .or_insert_with(|| RepresentationRecord {
                    digest: digest.to_owned(),
                    origin: origin.clone(),
                    updated_at: now,
                    event_sequence: sequence,
                    latest: if transport_failure || integrity_mismatch {
                        HealthEvent::TransportFailure
                    } else {
                        HealthEvent::Success
                    },
                    failures: 0,
                    cooldown_until: None,
                    quarantined_until: None,
                    throughput_bps: None,
                    latency_micros: None,
                    range_supported: None,
                });
            update_representation(
                entry,
                &observation,
                now,
                sequence,
                transport_failure,
                integrity_mismatch,
            );
            prune_representation_top_k(&mut state, digest);
        }
        prune(&mut state, now);
        drop(state);
        self.schedule_write();
    }

    fn snapshot(&self) -> HistoryState {
        self.lock_state().clone()
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, HistoryState> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn schedule_write(&self) {
        let Some(path) = self.shared.path.clone() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut writer = self
            .shared
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if writer.admission_closed || writer.running {
            return;
        }
        writer.running = true;
        self.shared.writer_idle.send_replace(false);
        let shared = Arc::clone(&self.shared);
        drop(runtime.spawn(run_writer(shared, path)));
    }
}

async fn run_writer(shared: Arc<Shared>, path: PathBuf) {
    loop {
        if shared
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .admission_closed
        {
            set_writer_idle(&shared);
            return;
        }
        let wait = shared
            .last_write
            .lock()
            .ok()
            .and_then(|last| last.map(|last| PERSISTENCE_THROTTLE.saturating_sub(last.elapsed())))
            .unwrap_or_default();
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        let snapshot = shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let snapshot_sequence = snapshot.sequence;
        let worker_shared = Arc::clone(&shared);
        let worker_path = path.clone();
        let result = tokio::task::spawn_blocking(move || {
            persist_snapshot(&worker_shared, &worker_path, &snapshot)
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            set_writer_idle(&shared);
            return;
        }
        if let Ok(mut last_write) = shared.last_write.lock() {
            *last_write = Some(Instant::now());
        }

        let mut writer = shared
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current_sequence = shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .sequence;
        if current_sequence > snapshot_sequence && !writer.admission_closed {
            drop(writer);
            continue;
        }
        writer.running = false;
        shared.writer_idle.send_replace(true);
        return;
    }
}

fn set_writer_idle(shared: &Shared) {
    let mut writer = shared
        .writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    writer.running = false;
    shared.writer_idle.send_replace(true);
}

trait SortCdnCandidates {
    fn tap_sort(self) -> Vec<String>;
}

impl SortCdnCandidates for Vec<(u8, Option<u64>, Option<u64>, (u64, u64), usize, String)> {
    fn tap_sort(mut self) -> Vec<String> {
        self.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| match (left.1, right.1) {
                    (Some(a), Some(b)) => b.cmp(&a),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    _ => std::cmp::Ordering::Equal,
                })
                .then_with(|| match (left.2, right.2) {
                    (Some(a), Some(b)) => a.cmp(&b),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    _ => std::cmp::Ordering::Equal,
                })
                .then_with(|| right.3.cmp(&left.3))
                .then_with(|| left.4.cmp(&right.4))
        });
        self.into_iter().map(|entry| entry.5).collect()
    }
}

impl HealthEvent {
    fn is_success(self) -> bool {
        matches!(self, Self::Success)
    }
}

fn request_candidates(request: &BilibiliMediaRequest) -> Vec<String> {
    let mut seen = HashSet::new();
    std::iter::once(&request.url)
        .chain(request.backup_urls.iter())
        .filter(|url| seen.insert((*url).clone()))
        .cloned()
        .collect()
}

fn representation_digest(request: &BilibiliMediaRequest) -> Option<String> {
    if request.kind == BilibiliMediaRequestKind::FlvSegment
        || request.cache_key.content_id.len() > MAX_CONTENT_ID_BYTES
        || request.cache_key.content_id.is_empty()
        || request
            .codecs
            .as_ref()
            .is_some_and(|value| value.len() > MAX_CODEC_BYTES)
    {
        return None;
    }
    let mut hash = Sha256::new();
    hash_field(&mut hash, request.cache_key.content_id.as_bytes());
    hash_field(
        &mut hash,
        format!("{:?}", request.cache_key.media_kind).as_bytes(),
    );
    hash_optional_u32(&mut hash, request.stream_id);
    hash_optional_string(&mut hash, request.codecs.as_deref());
    hash_optional_u32(&mut hash, request.width);
    hash_optional_u32(&mut hash, request.height);
    hash_optional_u64(&mut hash, request.size);
    hash_optional_u64(&mut hash, request.bandwidth);
    hash_optional_u32(&mut hash, request.duration_seconds);
    Some(hex_digest(hash.finalize().as_slice()))
}

fn hash_field(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
}

fn hash_optional_string(hash: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            hash.update([1]);
            hash_field(hash, value.as_bytes());
        }
        None => hash.update([0]),
    }
}

fn hash_optional_u32(hash: &mut Sha256, value: Option<u32>) {
    match value {
        Some(value) => {
            hash.update([1]);
            hash_field(hash, &value.to_be_bytes());
        }
        None => hash.update([0]),
    }
}

fn hash_optional_u64(hash: &mut Sha256, value: Option<u64>) {
    match value {
        Some(value) => {
            hash.update([1]);
            hash_field(hash, &value.to_be_bytes());
        }
        None => hash.update([0]),
    }
}

fn canonical_origin(value: &str) -> Option<String> {
    if value.len() > 16 * 1024 {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

fn representation_map_key(digest: &str, origin: &str) -> String {
    format!("{digest}|{origin}")
}

fn update_host(
    entry: &mut HostRecord,
    observation: &CdnObservation,
    now: u64,
    seq: u64,
    failure: bool,
) {
    entry.updated_at = now;
    entry.event_sequence = seq;
    if failure {
        entry.latest = HealthEvent::TransportFailure;
        entry.failures = entry.failures.saturating_add(1);
        entry.cooldown_until = Some(now.saturating_add(cooldown_secs(entry.failures)));
        entry.throughput_bps = None;
        entry.latency_micros = None;
        entry.range_supported = None;
        return;
    }
    entry.latest = HealthEvent::Success;
    entry.failures = 0;
    entry.cooldown_until = None;
    update_metrics(
        &mut entry.throughput_bps,
        &mut entry.latency_micros,
        &mut entry.range_supported,
        observation,
    );
}

fn update_representation(
    entry: &mut RepresentationRecord,
    observation: &CdnObservation,
    now: u64,
    seq: u64,
    failure: bool,
    integrity_mismatch: bool,
) {
    entry.updated_at = now;
    entry.event_sequence = seq;
    if integrity_mismatch {
        entry.quarantined_until = Some(now.saturating_add(REPRESENTATION_TTL_SECS));
        entry.latest = HealthEvent::TransportFailure;
        entry.throughput_bps = None;
        entry.latency_micros = None;
        entry.range_supported = None;
        return;
    }
    if failure {
        entry.latest = HealthEvent::TransportFailure;
        entry.failures = entry.failures.saturating_add(1);
        entry.cooldown_until = Some(now.saturating_add(cooldown_secs(entry.failures)));
        entry.throughput_bps = None;
        entry.latency_micros = None;
        entry.range_supported = None;
        return;
    }
    if observation.outcome != CdnObservationOutcome::Complete {
        return;
    }
    entry.latest = HealthEvent::Success;
    entry.failures = 0;
    entry.cooldown_until = None;
    entry.quarantined_until = None;
    update_metrics(
        &mut entry.throughput_bps,
        &mut entry.latency_micros,
        &mut entry.range_supported,
        observation,
    );
}

fn update_metrics(
    throughput: &mut Option<u64>,
    latency: &mut Option<u64>,
    range_supported: &mut Option<bool>,
    observation: &CdnObservation,
) {
    if observation.source != CdnObservationSource::DownloadShard {
        if let Some(elapsed) = observation.elapsed.filter(|duration| !duration.is_zero()) {
            let sample = ((u128::from(observation.bytes) * 8 * 1_000_000)
                / elapsed.as_micros().max(1))
            .min(u128::from(u64::MAX)) as u64;
            *throughput = Some(ewma(*throughput, sample));
        }
        if let Some(first_byte) = observation.first_byte_latency {
            let sample = first_byte.as_micros().min(u128::from(u64::MAX)) as u64;
            *latency = Some(ewma(*latency, sample));
        }
    }
    if let Some(supported) = observation.range_supported {
        *range_supported = Some(supported);
    }
}

fn ewma(previous: Option<u64>, sample: u64) -> u64 {
    previous.map_or(sample, |old| {
        ((u128::from(old) * 3 + u128::from(sample)) / 4) as u64
    })
}

fn cooldown_secs(failures: u32) -> u64 {
    BASE_COOLDOWN_SECS
        .saturating_mul(1_u64 << failures.saturating_sub(1).min(6))
        .min(MAX_COOLDOWN_SECS)
}

fn prune_representation_top_k(state: &mut HistoryState, digest: &str) {
    let mut entries = state
        .representations
        .iter()
        .filter(|(_, record)| record.digest == digest)
        .map(|(key, record)| (key.clone(), record.updated_at, record.event_sequence))
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| std::cmp::Reverse((entry.1, entry.2)));
    for (key, _, _) in entries.into_iter().skip(MAX_ORIGINS_PER_REPRESENTATION) {
        state.representations.remove(&key);
    }
}

fn prune(state: &mut HistoryState, now: u64) {
    state
        .hosts
        .retain(|_, record| now.saturating_sub(record.updated_at) < HOST_TTL_SECS);
    state
        .representations
        .retain(|_, record| now.saturating_sub(record.updated_at) < REPRESENTATION_TTL_SECS);
    if state.hosts.len() > MAX_HOSTS {
        let mut oldest = state
            .hosts
            .iter()
            .map(|(key, record)| (key.clone(), record.updated_at))
            .collect::<Vec<_>>();
        oldest.sort_by_key(|entry| entry.1);
        for (key, _) in oldest.into_iter().take(state.hosts.len() - MAX_HOSTS) {
            state.hosts.remove(&key);
        }
    }
    if state.representations.len() > MAX_REPRESENTATIONS {
        let mut oldest = state
            .representations
            .iter()
            .map(|(key, record)| (key.clone(), record.updated_at))
            .collect::<Vec<_>>();
        oldest.sort_by_key(|entry| entry.1);
        for (key, _) in oldest
            .into_iter()
            .take(state.representations.len() - MAX_REPRESENTATIONS)
        {
            state.representations.remove(&key);
        }
    }
}

fn read_state(path: &Path) -> io::Result<Option<HistoryState>> {
    let mut file = match open_nofollow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // Validate the opened object, not a pathname lookup that could race with open.
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(invalid_history_file());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(invalid_history_file());
    }
    let mut state: HistoryState =
        serde_json::from_slice(&bytes).map_err(|_| invalid_history_file())?;
    if state.version != VERSION {
        return Err(invalid_history_file());
    }
    validate_loaded_state(&mut state);
    prune(&mut state, now_seconds());
    Ok(Some(state))
}

fn validate_loaded_state(state: &mut HistoryState) {
    state.hosts.retain(|origin, record| {
        canonical_origin(origin).as_deref() == Some(origin.as_str())
            && record
                .cooldown_until
                .is_none_or(|value| value <= record.updated_at.saturating_add(MAX_COOLDOWN_SECS))
    });
    state.representations.retain(|key, record| {
        key == &representation_map_key(&record.digest, &record.origin)
            && record.digest.len() == 64
            && record.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            && canonical_origin(&record.origin).as_deref() == Some(record.origin.as_str())
            && record
                .cooldown_until
                .is_none_or(|value| value <= record.updated_at.saturating_add(MAX_COOLDOWN_SECS))
            && record.quarantined_until.is_none_or(|value| {
                value <= record.updated_at.saturating_add(REPRESENTATION_TTL_SECS)
            })
    });
    let digests = state
        .representations
        .values()
        .map(|entry| entry.digest.clone())
        .collect::<HashSet<_>>();
    for digest in digests {
        prune_representation_top_k(state, &digest);
    }
    state
        .hosts
        .retain(|origin, record| origin.len() <= 512 && record.failures <= 1_000_000);
}

fn persist_snapshot(shared: &Shared, path: &Path, state: &HistoryState) -> io::Result<()> {
    let _io_guard = shared
        .persistence_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut persisted_sequence = shared
        .persisted_sequence
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.sequence < *persisted_sequence {
        return Ok(());
    }
    persist_state(path, state)?;
    *persisted_sequence = state.sequence;
    Ok(())
}

fn persist_state(path: &Path, state: &HistoryState) -> io::Result<()> {
    let parent = path.parent().ok_or_else(invalid_history_file)?;
    if !fs::symlink_metadata(parent)?.file_type().is_dir() {
        return Err(invalid_history_file());
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => return Err(invalid_history_file()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let bytes = serde_json::to_vec(state).map_err(|_| invalid_history_file())?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(invalid_history_file());
    }
    let temporary = parent.join(format!(".cdn-history-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = create_private_file(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn open_nofollow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    options.open(path)
}

fn invalid_history_file() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid CDN history file")
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    bytes
        .as_ref()
        .iter()
        .flat_map(|byte| {
            [
                HEX[(byte >> 4) as usize] as char,
                HEX[(byte & 0x0f) as usize] as char,
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request(primary: &str, backups: &[&str]) -> BilibiliMediaRequest {
        BilibiliMediaRequest {
            kind: BilibiliMediaRequestKind::Video,
            stream_id: Some(80_000),
            url: primary.to_owned(),
            backup_urls: backups.iter().map(|value| (*value).to_owned()).collect(),
            headers: Vec::new(),
            mime_type: Some("video/mp4".to_owned()),
            codecs: Some("avc1.640028".to_owned()),
            bandwidth: Some(4_000_000),
            width: Some(1920),
            height: Some(1080),
            frame_rate: Some("30".to_owned()),
            size: Some(80_000_000),
            duration_seconds: Some(120),
            cache_key: crate::bbdown_adapter::BilibiliMediaCacheKey {
                content_id: "BV1synthetic".to_owned(),
                media_kind: BilibiliMediaRequestKind::Video,
                stream_id: Some(80_000),
                codecs: Some("avc1.640028".to_owned()),
                source_hash: "source-hash-is-not-part-of-history".to_owned(),
            },
        }
    }

    fn probe(bytes: u64, elapsed_ms: u64) -> CdnObservation {
        CdnObservation::probe_complete(bytes, Duration::from_millis(elapsed_ms))
    }

    fn record_at(
        history: &CdnHistory,
        request: &BilibiliMediaRequest,
        url: &str,
        observation: CdnObservation,
        now: u64,
    ) {
        history.record_request_at(request, url, observation, now);
    }

    #[test]
    fn ranking_deduplicates_exact_urls_and_keeps_original_fallback() {
        let history = CdnHistory::default();
        let first = "https://a.example/video?version=1";
        let second = "https://b.example/video?version=2";
        let third = "https://c.example/video?version=3";
        let request = make_request(first, &[second, first, third]);
        record_at(&history, &request, second, probe(10_000, 100), 100);
        assert_eq!(
            history.rank_request_at(&request, 101),
            vec![second, first, third]
        );
    }

    #[test]
    fn canonical_origin_preserves_ipv6_literal_and_non_default_port() {
        assert_eq!(
            Some("https://[::1]:8443".to_owned()),
            canonical_origin("https://[::1]:8443/media")
        );
    }

    #[test]
    fn range_fanout_rotates_distinct_origins_and_keeps_every_fallback() {
        let history = CdnHistory::default();
        let first = "https://a.example/video";
        let same_origin = "https://a.example/backup";
        let second = "https://b.example/video";
        let third = "https://c.example/video";
        let request = make_request(first, &[same_origin, second, third]);
        assert_eq!(
            history.rank_request_for_range_at(&request, 0, 100),
            vec![first, second, third, same_origin]
        );
        assert_eq!(
            history.rank_request_for_range_at(&request, 1, 100),
            vec![second, third, first, same_origin]
        );
        assert_eq!(
            history.rank_request_for_range_at(&request, 3, 100),
            vec![first, second, third, same_origin]
        );
    }

    #[test]
    fn range_fanout_never_promotes_cooldown_quarantine_or_nonrange_origins() {
        let history = CdnHistory::default();
        let good = "https://good.example/video";
        let alternative = "https://alternative.example/video";
        let quarantined = "https://quarantined.example/video";
        let cooling = "https://cooling.example/video";
        let nonrange = "https://nonrange.example/video";
        let request = make_request(good, &[quarantined, cooling, nonrange, alternative]);
        record_at(
            &history,
            &request,
            quarantined,
            CdnObservation::failure(
                CdnObservationSource::Playback,
                CdnObservationOutcome::IntegrityMismatch,
            ),
            100,
        );
        record_at(
            &history,
            &request,
            cooling,
            CdnObservation::failure(
                CdnObservationSource::Playback,
                CdnObservationOutcome::Timeout,
            ),
            100,
        );
        record_at(
            &history,
            &request,
            nonrange,
            CdnObservation::playback_complete(
                1024,
                Duration::from_secs(1),
                Duration::from_millis(1),
                Some(false),
            ),
            100,
        );
        let ranked = history.rank_request_for_range_at(&request, 1, 101);
        assert_eq!(&ranked[..2], &[alternative, good]);
        assert_eq!(ranked.len(), 5);
        assert!(ranked[2..].contains(&quarantined.to_owned()));
        assert!(ranked[2..].contains(&cooling.to_owned()));
        assert!(ranked[2..].contains(&nonrange.to_owned()));
    }

    #[test]
    fn range_fanout_keeps_representation_quarantine_scoped_and_large_tail_unchanged() {
        let history = CdnHistory::default();
        let first = "https://first.example/video";
        let second = "https://second.example:8443/video";
        let request = make_request(first, &[second]);
        record_at(
            &history,
            &request,
            second,
            CdnObservation::failure(
                CdnObservationSource::Playback,
                CdnObservationOutcome::IntegrityMismatch,
            ),
            100,
        );
        assert_eq!(
            history.rank_request_for_range_at(&request, 1, 101)[0],
            first
        );
        let mut unrelated = request.clone();
        unrelated.stream_id = Some(64_000);
        unrelated.cache_key.stream_id = Some(64_000);
        assert_eq!(
            history.rank_request_for_range_at(&unrelated, 1, 101)[0],
            second
        );

        let backups = (0..MAX_CANDIDATES + 4)
            .map(|index| format!("https://edge-{index}.example/video"))
            .collect::<Vec<_>>();
        let mut large = make_request(first, &[]);
        large.backup_urls = backups;
        let ordinary = history.rank_request_at(&large, 101);
        let fanout = history.rank_request_for_range_at(&large, u64::MAX, 101);
        assert_eq!(&fanout[MAX_CANDIDATES..], &ordinary[MAX_CANDIDATES..]);
        assert_eq!(fanout.len(), ordinary.len());
    }

    #[test]
    fn candidates_beyond_ranking_bound_remain_in_original_fallback_order() {
        let history = CdnHistory::default();
        let urls = (0..MAX_CANDIDATES + 3)
            .map(|index| format!("https://edge-{index}.example/video?version=1"))
            .collect::<Vec<_>>();
        let backups = urls.iter().skip(1).map(String::as_str).collect::<Vec<_>>();
        let request = make_request(&urls[0], &backups);
        let ranked = history.rank_request_at(&request, 100);
        assert_eq!(ranked.len(), urls.len());
        assert_eq!(&ranked[MAX_CANDIDATES..], &urls[MAX_CANDIDATES..]);
    }

    #[test]
    fn refreshed_signed_url_retains_preference_and_distinct_ports_do_not_merge() {
        let history = CdnHistory::default();
        let old_a = "https://edge.example:8443/old-path?expiry=10";
        let old_b = "https://edge.example:9443/old-path?expiry=10";
        let mut original = make_request(old_b, &[old_a]);
        record_at(&history, &original, old_a, probe(50_000, 100), 100);

        original.url = "https://edge.example:9443/new?expiry=20".to_owned();
        original.backup_urls = vec!["https://edge.example:8443/new?expiry=20".to_owned()];
        original.cache_key.source_hash = "renewed-source-hash".to_owned();
        assert_eq!(
            history.rank_request_at(&original, 101),
            vec![original.backup_urls[0].clone(), original.url.clone()]
        );

        let other_port = make_request(
            "https://neutral.example/video?version=1",
            &["https://edge.example:9443/new?expiry=20"],
        );
        history.record_origin_at(old_a, probe(25_000, 100), 100);
        assert_eq!(
            history.rank_request_at(&other_port, 101),
            vec![other_port.url, other_port.backup_urls[0].clone()]
        );
    }

    #[test]
    fn representation_metadata_isolation_keeps_quality_candidates_separate() {
        let history = CdnHistory::default();
        let good = "https://a.example/video?version=1";
        let other = "https://b.example/video?version=1";
        let request = make_request(other, &[good]);
        record_at(&history, &request, good, probe(100_000, 100), 100);

        let mut different_size = request.clone();
        different_size.size = Some(90_000_000);
        let original_digest = representation_digest(&request).unwrap();
        let different_digest = representation_digest(&different_size).unwrap();
        assert_ne!(original_digest, different_digest);
        let origin = canonical_origin(good).unwrap();
        let state = history.lock_state();
        assert!(
            state
                .representations
                .contains_key(&representation_map_key(&original_digest, &origin))
        );
        assert!(
            !state
                .representations
                .contains_key(&representation_map_key(&different_digest, &origin))
        );
        drop(state);
        assert_eq!(
            history.rank_request_at(&different_size, 101),
            vec![good.to_owned(), other.to_owned()]
        );
    }

    #[test]
    fn transport_failure_supersedes_success_even_at_the_same_timestamp() {
        let history = CdnHistory::default();
        let good = "https://a.example/video?version=1";
        let other = "https://b.example/video?version=1";
        let request = make_request(good, &[other]);
        record_at(&history, &request, good, probe(100_000, 100), 100);
        record_at(
            &history,
            &request,
            good,
            CdnObservation::failure(
                CdnObservationSource::Playback,
                CdnObservationOutcome::Timeout,
            ),
            100,
        );
        assert_eq!(history.rank_request_at(&request, 101), vec![other, good]);
        assert_eq!(
            history.rank_request_at(&request, 100 + BASE_COOLDOWN_SECS),
            vec![good, other]
        );
    }

    #[test]
    fn integrity_quarantine_is_representation_scoped_and_source_unavailable_is_neutral() {
        let history = CdnHistory::default();
        let a = "https://a.example/video?version=1";
        let b = "https://b.example/video?version=1";
        let request = make_request(a, &[b]);
        record_at(&history, &request, a, probe(100_000, 100), 100);
        record_at(
            &history,
            &request,
            a,
            CdnObservation::failure(
                CdnObservationSource::Playback,
                CdnObservationOutcome::IntegrityMismatch,
            ),
            101,
        );
        assert_eq!(history.rank_request_at(&request, 102), vec![b, a]);

        let mut unrelated = make_request(a, &[b]);
        unrelated.cache_key.content_id = "BV1different-content".to_owned();
        assert_eq!(
            history.rank_request_at(&unrelated, 102),
            vec![a.to_owned(), b.to_owned()]
        );
        history.record_origin_at(
            "https://auth.example/item?version=1",
            CdnObservation::failure(
                CdnObservationSource::Playback,
                CdnObservationOutcome::SourceUnavailable,
            ),
            103,
        );
        let auth = make_request("https://auth.example/item?version=2", &[b]);
        assert_eq!(
            history.rank_request_at(&auth, 104),
            vec![auth.url, b.to_owned()]
        );
    }

    #[test]
    fn partial_and_cancelled_shards_do_not_create_success_metrics() {
        let history = CdnHistory::default();
        let a = "https://a.example/video?version=1";
        let b = "https://b.example/video?version=1";
        let request = make_request(b, &[a]);
        record_at(
            &history,
            &request,
            a,
            CdnObservation::new(
                CdnObservationSource::Playback,
                CdnObservationOutcome::Partial,
                500,
            ),
            100,
        );
        record_at(
            &history,
            &request,
            a,
            CdnObservation::new(
                CdnObservationSource::Playback,
                CdnObservationOutcome::Cancelled,
                500,
            ),
            101,
        );
        record_at(
            &history,
            &request,
            a,
            CdnObservation::download_shard(5_000),
            102,
        );
        assert_eq!(history.rank_request_at(&request, 103), vec![a, b]);
        let record = history
            .lock_state()
            .representations
            .values()
            .next()
            .unwrap()
            .clone();
        assert_eq!(record.throughput_bps, None);
        assert_eq!(record.latency_micros, None);
    }

    #[test]
    fn download_shards_do_not_contribute_timing_metrics() {
        let history = CdnHistory::default();
        let request = make_request("https://a.example/video?version=1", &[]);
        history.record_request(
            &request,
            &request.url,
            CdnObservation {
                source: CdnObservationSource::DownloadShard,
                outcome: CdnObservationOutcome::Complete,
                bytes: 1_000_000,
                elapsed: Some(Duration::from_millis(10)),
                first_byte_latency: Some(Duration::from_millis(1)),
                range_supported: None,
            },
        );
        let state = history.lock_state();
        let host = state.hosts.values().next().unwrap();
        assert_eq!(host.throughput_bps, None);
        assert_eq!(host.latency_micros, None);

        let probe_history = CdnHistory::default();
        let probe_request = make_request("https://probe.example/video?version=1", &[]);
        record_at(
            &probe_history,
            &probe_request,
            &probe_request.url,
            probe(1_000_000, 100),
            100,
        );
        let state = probe_history.lock_state();
        let host = state.hosts.values().next().unwrap();
        assert!(host.throughput_bps.is_some());
        assert_eq!(host.latency_micros, None);
    }

    #[test]
    fn ttl_expires_host_and_representation_entries() {
        let history = CdnHistory::default();
        let a = "https://a.example/video?version=1";
        let b = "https://b.example/video?version=1";
        let request = make_request(b, &[a]);
        record_at(&history, &request, a, probe(100_000, 100), 100);
        assert_eq!(history.rank_request_at(&request, 101)[0], a);
        assert_eq!(
            history.rank_request_at(&request, 100 + REPRESENTATION_TTL_SECS)[0],
            a
        );
        assert!(history.lock_state().representations.is_empty());
        assert!(!history.lock_state().hosts.is_empty());
        assert_eq!(history.rank_request_at(&request, 100 + HOST_TTL_SECS)[0], b);
        assert!(history.lock_state().hosts.is_empty());
    }

    #[test]
    fn persistence_round_trip_is_bounded_and_contains_no_url_or_secret_material() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cdn-history.json");
        let history = CdnHistory::load(&path);
        let a = "https://a.example/signed/path?expiry=42&version=7";
        let b = "https://b.example/video?version=1";
        let request = make_request(b, &[a]);
        record_at(&history, &request, a, probe(100_000, 100), now_seconds());
        history.flush().unwrap();

        let bytes = fs::read(&path).unwrap();
        assert!(bytes.len() <= MAX_FILE_BYTES);
        let persisted = String::from_utf8(bytes).unwrap();
        assert!(persisted.contains("https://a.example"));
        assert!(!persisted.contains("/signed/path"));
        assert!(!persisted.contains("expiry=42"));
        assert!(!persisted.contains("BV1synthetic"));
        assert!(!persisted.contains("source-hash-is-not-part-of-history"));

        let restored = CdnHistory::load(&path);
        let refreshed = make_request(
            "https://b.example/new?expiry=99",
            &["https://a.example/fresh?version=8"],
        );
        assert_eq!(
            restored.rank_request(&refreshed),
            vec![refreshed.backup_urls[0].clone(), refreshed.url.clone()]
        );
    }

    #[test]
    fn malformed_and_oversized_files_are_not_overwritten_after_load() {
        let directory = tempfile::tempdir().unwrap();
        assert!(read_state(directory.path()).is_err());

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            let fifo = directory.path().join("history.fifo");
            let fifo_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
            assert!(read_state(&fifo).is_err());
        }

        let malformed = directory.path().join("malformed.json");
        let malformed_bytes = b"{not-json";
        fs::write(&malformed, malformed_bytes).unwrap();
        assert_invalid_load_preserves(&malformed, malformed_bytes);

        let unsupported = directory.path().join("unsupported.json");
        let unsupported_bytes = br#"{"version":2}"#;
        fs::write(&unsupported, unsupported_bytes).unwrap();
        assert_invalid_load_preserves(&unsupported, unsupported_bytes);

        let oversized = directory.path().join("oversized.json");
        let oversized_bytes = vec![b' '; MAX_FILE_BYTES + 1];
        fs::write(&oversized, &oversized_bytes).unwrap();
        assert_invalid_load_preserves(&oversized, &oversized_bytes);

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let target = directory.path().join("target.json");
            fs::write(&target, b"target contents").unwrap();
            let link = directory.path().join("link.json");
            symlink(&target, &link).unwrap();
            let history = CdnHistory::load(&link);
            assert!(history.shared.path.is_none());
            let request = make_request("https://a.example/video?version=1", &[]);
            record_at(&history, &request, &request.url, probe(100, 50), 100);
            history.flush().unwrap();
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read(&target).unwrap(), b"target contents");
        }
    }

    fn assert_invalid_load_preserves(path: &Path, original: &[u8]) {
        let history = CdnHistory::load(path);
        assert!(history.shared.path.is_none());
        assert!(history.lock_state().hosts.is_empty());
        let request = make_request("https://a.example/video?version=1", &[]);
        record_at(&history, &request, &request.url, probe(100, 50), 100);
        history.flush().unwrap();
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn unreadable_file_is_not_overwritten_when_permissions_deny_read() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("unreadable.json");
            let original = br#"{"version":1}"#;
            fs::write(&path, original).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
            let unreadable = read_state(&path).is_err();
            if unreadable {
                let history = CdnHistory::load(&path);
                assert!(history.shared.path.is_none());
                let request = make_request("https://a.example/video?version=1", &[]);
                record_at(&history, &request, &request.url, probe(100, 50), 100);
                history.flush().unwrap();
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            if unreadable {
                assert_eq!(fs::read(&path).unwrap(), original);
            }
        }
    }

    #[test]
    fn missing_file_can_be_loaded_and_persisted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cdn-history.json");
        let history = CdnHistory::load(&path);
        assert!(history.shared.path.is_some());
        let request = make_request("https://a.example/video?version=1", &[]);
        record_at(
            &history,
            &request,
            &request.url,
            probe(100, 50),
            now_seconds(),
        );
        history.flush().unwrap();
        assert_eq!(read_state(&path).unwrap().unwrap().hosts.len(), 1);
    }

    #[test]
    fn persistence_failure_never_panics_or_changes_observation_api_result() {
        let directory = tempfile::tempdir().unwrap();
        let not_a_directory = directory.path().join("file");
        fs::write(&not_a_directory, b"x").unwrap();
        let history = CdnHistory::load(not_a_directory.join("cdn-history.json"));
        assert!(history.shared.path.is_none());
        let request = make_request("https://a.example/video?version=1", &[]);
        record_at(&history, &request, &request.url, probe(100, 50), 100);
        history.flush().unwrap();
        assert_eq!(history.rank_request_at(&request, 101), vec![request.url]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_persists_the_last_coalesced_observation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cdn-history.json");
        let history = CdnHistory::load(&path);
        let first = "https://a.example/video?version=1";
        let last = "https://b.example/video?version=1";
        let request = make_request(first, &[last]);
        let now = now_seconds();
        record_at(&history, &request, first, probe(10_000, 100), now);
        record_at(&history, &request, last, probe(20_000, 100), now);

        let _ = tokio::join!(history.shutdown_and_wait(), history.shutdown_and_wait());

        let restored = CdnHistory::load(&path);
        assert_eq!(
            restored.rank_request_at(&request, now.saturating_add(1)),
            vec![last, first]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_and_later_records_do_not_recreate_removed_case_root() {
        let directory = tempfile::tempdir().unwrap();
        let case_root = directory.path().join("case-root");
        fs::create_dir(&case_root).unwrap();
        let path = case_root.join("cdn-history.json");
        let history = CdnHistory::load(&path);
        let request = make_request("https://a.example/video?version=1", &[]);
        record_at(&history, &request, &request.url, probe(100, 50), 100);
        fs::remove_dir_all(&case_root).unwrap();

        history.shutdown_and_wait().await;
        history.record_request(&request, &request.url, probe(200, 50));
        assert!(!case_root.exists());
    }
}
