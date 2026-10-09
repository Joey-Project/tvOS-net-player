use std::{
    collections::{HashMap, HashSet},
    fs,
    future::Future,
    io::{self, Read, Seek, SeekFrom, Write},
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::http::StatusCode;
use futures_util::{StreamExt, TryStreamExt};
use prost_types::Timestamp;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{OwnedSemaphorePermit, Semaphore},
};

use crate::{
    bbdown_adapter::{
        BilibiliHttpHeader, BilibiliMediaCacheKey, BilibiliMediaRequest, BilibiliMediaRequestKind,
        BilibiliPlaybackVariantKind,
    },
    cdn_history::{CdnHistory, CdnObservation, CdnObservationOutcome, CdnObservationSource},
    generated::tvos_net_player::v1::{
        LibraryItem, LibrarySource, MediaVariant, PlaybackProtocol, PlaybackSource,
    },
    hls::{
        HlsAbrGroup, HlsAbrGroupKind, HlsAbrLevel, HlsAbrMetadata, HlsMediaResource,
        HlsMediaResourceMetadata, HlsMediaSegment, HlsPlaybackSession, HlsVariant,
        HlsVariantMetadata, mp4_initialization_length, should_forward_media_request_header,
    },
    hls_playback_progress::{
        HlsPlaybackActivityState, HlsPlaybackProgressSnapshot, PlaybackProgressIntent,
    },
    hls_range_cache::{
        HlsRangeCache, HlsRangeError, HlsRangePriority, HlsRangeResourceStatus, HlsReadyRange,
        HlsSessionRemovalGuard, PersistedRangeExtent, PersistedRangeManifest, PersistedRangeOrigin,
        RANGE_MAX_CHUNK_BYTES, RANGE_MAX_CHUNKS, RANGE_MAX_SIZE, RANGE_MIN_CHUNK_BYTES,
        RANGE_STARTUP_CHUNK_BYTES, RangeChunkKey, RangeResourceKey, SessionPublicationMode,
    },
    library::{OpenedMediaFile, open_read_no_follow},
    mp4_segments::{Mp4SegmentRange, mp4_fragment_ranges},
    playback_policy::PlaybackPolicy,
    transcoding::{
        HlsTranscodingPlan, HlsTranscodingPlanState, LAN_TRANSCODING_AUDIO_BANDWIDTH_BPS,
        LAN_TRANSCODING_AUDIO_CODEC, LAN_TRANSCODING_MAX_FRAME_RATE, LAN_TRANSCODING_MAX_HEIGHT,
        LAN_TRANSCODING_MAX_VIDEO_BANDWIDTH_BPS, LAN_TRANSCODING_MAX_WIDTH,
        LAN_TRANSCODING_VIDEO_CODEC, LanTranscodingError, LanTranscodingJobControl,
        run_hls_ffmpeg_transcode,
    },
};

const HLS_CACHE_SCHEMA_VERSION: u32 = 1;
const HLS_CACHE_DIR: &str = ".tvos-net-player/hls";
const HLS_LIBRARY_ITEM_PREFIX: &str = "bilibili.hls.";
const HLS_CACHE_VARIANT_LABEL: &str = "Offline HLS";
const HLS_INITIALIZATION_SCAN_BYTES: u64 = 1024 * 1024;
const HLS_PREWARM_HEAD_BYTES: u64 = HLS_INITIALIZATION_SCAN_BYTES;
const HLS_FIRST_WINDOW_PREFETCH_SECONDS: u64 = 30;
const HLS_FIRST_WINDOW_PREFETCH_MAX_BYTES: u64 = 8 * 1024 * 1024;
const HLS_PLAYBACK_POSITION_PREFETCH_MAX_BYTES: u64 = 32 * 1024 * 1024;
const HLS_TRANSCODED_RESOURCE_ID: &str = "transcoded.m4s";
const HLS_TRANSCODED_VIDEO_CODEC: &str = LAN_TRANSCODING_VIDEO_CODEC;
const HLS_TRANSCODED_AUDIO_CODEC: &str = LAN_TRANSCODING_AUDIO_CODEC;
const HLS_TRANSCODING_TEMP_FILE_SUFFIX: &str = ".transcode.tmp";
const HLS_TRANSCODING_COMMIT_MARKER_FILE: &str = "transcoding-commit.tmp";
const HLS_TRANSCODING_COMMIT_MARKER_TTL: Duration = Duration::from_secs(10 * 60);
const HLS_TRANSCODING_COMMIT_MARKER_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
static ETAG_MISMATCH_DIAGNOSTIC_USED: AtomicBool = AtomicBool::new(false);
static SESSION_MANIFEST_PUBLICATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

type RangeChunkFetch = (
    Vec<u8>,
    String,
    Option<String>,
    Option<String>,
    Duration,
    PersistedRangeManifest,
);

struct RangeExtentCommitInput<'a> {
    expected: &'a PersistedRangeManifest,
    bytes: &'a [u8],
    final_url: &'a str,
    etag: &'a Option<String>,
    last_modified: &'a Option<String>,
}

#[derive(Debug)]
enum RangeExtentCommitFailure {
    StaleTargetValidator,
    Rejected(HlsRangeError),
}

impl From<HlsRangeError> for RangeExtentCommitFailure {
    fn from(error: HlsRangeError) -> Self {
        Self::Rejected(error)
    }
}

impl From<io::Error> for RangeExtentCommitFailure {
    fn from(error: io::Error) -> Self {
        Self::Rejected(HlsRangeError::Io(error))
    }
}

fn session_manifest_publication_guard() -> io::Result<std::sync::MutexGuard<'static, ()>> {
    SESSION_MANIFEST_PUBLICATION_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| io::Error::other("HLS session publication lock poisoned"))
}

#[derive(Clone)]
pub(crate) struct HlsCacheStore {
    root_path: Arc<PathBuf>,
    cdn_history: Arc<CdnHistory>,
    range_cache: Arc<HlsRangeCache>,
    range_checkpoint_locks: Arc<Mutex<HashMap<RangeResourceKey, Weak<tokio::sync::Mutex<()>>>>>,
    #[cfg(test)]
    remove_session_failures: Arc<Mutex<HashSet<String>>>,
}

pub(crate) struct HlsCacheSessionDirectoryScan {
    entries: Option<fs::ReadDir>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HlsCacheEvictionPolicy {
    pub(crate) max_bytes: u64,
    pub(crate) high_watermark_percent: u8,
    pub(crate) low_watermark_percent: u8,
}

impl HlsCacheEvictionPolicy {
    pub(crate) fn eviction_enabled(self) -> bool {
        self.max_bytes > 0
    }

    pub(crate) fn high_watermark_bytes(self) -> u64 {
        percentage_bytes(self.max_bytes, self.high_watermark_percent)
    }

    pub(crate) fn low_watermark_bytes(self) -> u64 {
        percentage_bytes(self.max_bytes, self.low_watermark_percent)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsCacheUsageSnapshot {
    pub(crate) used_bytes: u64,
    pub(crate) completed_session_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsCacheStatusSnapshot {
    pub(crate) policy: HlsCacheEvictionPolicy,
    pub(crate) usage: HlsCacheUsageSnapshot,
    pub(crate) last_eviction: Option<HlsCacheEvictionSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsCacheCompletedEntry {
    pub(crate) session_id: String,
    pub(crate) library_item_id: String,
    pub(crate) size_bytes: u64,
    pub(crate) updated_at: SystemTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsCacheEvictionSummary {
    pub(crate) reason: String,
    pub(crate) started_used_bytes: u64,
    pub(crate) finished_used_bytes: u64,
    pub(crate) target_used_bytes: u64,
    pub(crate) projected_added_bytes: u64,
    pub(crate) evicted_bytes: u64,
    pub(crate) evicted_session_ids: Vec<String>,
    pub(crate) target_reached: bool,
    pub(crate) completed_at: SystemTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsCachePartialEntry {
    pub(crate) session_id: String,
    pub(crate) size_bytes: u64,
    pub(crate) updated_at: SystemTime,
}

#[derive(Clone)]
pub(crate) struct HlsTranscodingExecutionConfig {
    pub(crate) ffmpeg_path: PathBuf,
    pub(crate) permits: Arc<Semaphore>,
    pub(crate) active_job_count: Arc<AtomicUsize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsCacheCompletion {
    pub(crate) library_item_id: String,
    pub(crate) session: HlsPlaybackSession,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HlsCacheFillControl {
    Continue,
    Cancel,
    Preempt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HlsCacheFillProgress {
    pub(crate) downloaded_bytes: u64,
    pub(crate) total_bytes: Option<u64>,
}

#[derive(Clone, Copy, Default)]
struct EtagMismatchProbe {
    same_origin: bool,
    total_matches: bool,
    prefix_hash_matches: bool,
    baseline_etag_matches: bool,
    response_etag_syntax_valid: bool,
    prefix_etag_syntax_valid: bool,
}

enum CandidateVerificationError {
    Fallback(HlsRangeError),
    Mismatch(HlsRangeError),
    Abort(HlsRangeError),
}

impl From<HlsRangeError> for CandidateVerificationError {
    fn from(error: HlsRangeError) -> Self {
        match error {
            HlsRangeError::Network(_)
            | HlsRangeError::UpstreamStatus(_)
            | HlsRangeError::RangeUnsupported
            | HlsRangeError::InvalidResponse(_) => Self::Fallback(error),
            error => Self::Abort(error),
        }
    }
}

impl HlsCacheStore {
    pub(crate) fn new(root_path: impl Into<PathBuf>) -> Self {
        Self {
            root_path: Arc::new(root_path.into()),
            cdn_history: Arc::new(CdnHistory::default()),
            range_cache: Arc::new(HlsRangeCache::new(2, 0)),
            range_checkpoint_locks: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            remove_session_failures: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub(crate) fn with_cdn_history(mut self, cdn_history: Arc<CdnHistory>) -> Self {
        self.cdn_history = cdn_history;
        self
    }

    pub(crate) fn with_range_parallelism(mut self, parallelism: usize) -> Self {
        assert!((1..=8).contains(&parallelism));
        self.range_cache = Arc::new(HlsRangeCache::new(
            parallelism,
            self.range_cache.quota_bytes(),
        ));
        self
    }

    pub(crate) fn with_range_budget(mut self, max_cache_bytes: u64) -> Self {
        self.range_cache = Arc::new(HlsRangeCache::new(
            self.range_cache.parallelism(),
            max_cache_bytes,
        ));
        self
    }

    pub(crate) async fn begin_session_removal(
        &self,
        session_id: &str,
    ) -> Result<HlsSessionRemovalGuard, HlsRangeError> {
        self.range_cache.begin_session_removal(session_id).await
    }

    pub(crate) fn try_begin_session_removal(
        &self,
        session_id: &str,
    ) -> Result<Option<HlsSessionRemovalGuard>, HlsRangeError> {
        self.range_cache.try_begin_session_removal(session_id)
    }

    pub(crate) fn range_activity_counts(&self) -> (usize, usize) {
        self.range_cache.range_activity_counts()
    }

    pub(crate) async fn drain_range_writers(&self) {
        self.range_cache.drain().await;
    }

    pub(crate) async fn ensure_resource_range<F>(
        &self,
        client: &reqwest::Client,
        session_id: &str,
        resource: &HlsMediaResource,
        requested: Range<u64>,
        priority: HlsRangePriority,
        control: &F,
    ) -> Result<HlsReadyRange, HlsRangeError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        self.ensure_range_inner(client, session_id, resource, requested, priority, control)
            .await
    }

    pub(crate) async fn read_resource_range(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        requested: Range<u64>,
    ) -> Result<Option<Vec<u8>>, HlsRangeError> {
        let _active = self
            .range_cache
            .enter(session_id, HlsRangePriority::Foreground)?;
        self.read_durable_range(session_id, resource, requested)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn fill_missing_resource_ranges<F>(
        &self,
        client: &reqwest::Client,
        session_id: &str,
        resource: &HlsMediaResource,
        priority: HlsRangePriority,
        control: &F,
    ) -> Result<HlsRangeResourceStatus, HlsRangeError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        self.fill_missing_ranges_with_progress(
            client,
            session_id,
            resource,
            priority,
            control,
            |_| {},
        )
        .await
    }

    #[cfg(test)]
    pub(crate) fn range_durable_bytes(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
    ) -> Result<u64, HlsRangeError> {
        let loaded = self.load_range_manifest(session_id, resource)?;
        let Some(loaded) = loaded else {
            return Ok(0);
        };
        Ok(loaded.manifest.durable_bytes)
    }

    pub(crate) fn save_session(&self, session: &HlsPlaybackSession) -> io::Result<()> {
        let _guard = session_manifest_publication_guard()?;
        let existing = self.read_session_manifest_for_publication(&session.id)?;
        let session = match existing.as_ref() {
            Some(existing) if session_refresh_matches(existing, session) => {
                preserve_persisted_request_candidates(existing, session)
            }
            _ => session.clone(),
        };
        let mode = self.session_publication_mode(&session, false)?;
        self.save_session_with_mode_and_hook(&session, mode, || {})
    }

    pub(crate) fn save_refreshed_session(
        &self,
        expected: &HlsPlaybackSession,
        replacement: &HlsPlaybackSession,
    ) -> io::Result<()> {
        let _guard = session_manifest_publication_guard()?;
        if expected.id != replacement.id || !session_refresh_matches(expected, replacement) {
            return Err(invalid_session_publication("refresh-replacement-binding"));
        }
        let existing = self
            .read_session_manifest_for_publication(&expected.id)?
            .ok_or_else(|| invalid_session_publication("refresh-expected-missing"))?;
        if existing != *expected {
            return Err(invalid_session_publication("refresh-expected-mismatch"));
        }
        let publication = self
            .range_cache
            .begin_session_publication(&replacement.id, SessionPublicationMode::Refresh)
            .map_err(hls_range_error_to_io)?;
        let existing = self
            .read_session_manifest_for_publication(&expected.id)?
            .ok_or_else(|| invalid_session_publication("refresh-recheck-missing"))?;
        if existing != *expected {
            return Err(invalid_session_publication("refresh-recheck-mismatch"));
        }
        self.validate_session_publication(replacement, SessionPublicationMode::Refresh)?;
        let session_dir = self.session_dir(&replacement.id)?;
        self.ensure_cache_directory(&session_dir)?;
        self.write_json_atomically(
            &session_dir.join("session.json"),
            &PersistedHlsSession::from(replacement.clone()),
        )?;
        publication.commit();
        Ok(())
    }

    #[cfg(test)]
    fn save_session_with_publication_hook<F>(
        &self,
        session: &HlsPlaybackSession,
        after_guard: F,
    ) -> io::Result<()>
    where
        F: FnOnce(),
    {
        let mode = self.session_publication_mode(session, false)?;
        self.save_session_with_mode_and_hook(session, mode, after_guard)
    }

    fn save_session_with_mode_and_hook<F>(
        &self,
        session: &HlsPlaybackSession,
        mode: SessionPublicationMode,
        after_guard: F,
    ) -> io::Result<()>
    where
        F: FnOnce(),
    {
        let publication = self
            .range_cache
            .begin_session_publication(&session.id, mode)
            .map_err(hls_range_error_to_io)?;
        after_guard();
        self.validate_session_publication(session, mode)?;
        let session_dir = self.session_dir(&session.id)?;
        self.ensure_cache_directory(&session_dir)?;
        self.write_json_atomically(
            &session_dir.join("session.json"),
            &PersistedHlsSession::from(session.clone()),
        )?;
        publication.commit();
        Ok(())
    }

    pub(crate) fn save_completed_session(&self, session: &HlsPlaybackSession) -> io::Result<()> {
        let _guard = session_manifest_publication_guard()?;
        let completed = sanitized_completed_session(session);
        let mode = self.session_publication_mode(&completed, true)?;
        self.save_session_with_mode_and_hook(&completed, mode, || {})
    }

    pub(crate) fn save_restored_source_completed_session(
        &self,
        completed: &HlsPlaybackSession,
        completed_library_item_id: &str,
    ) -> io::Result<()> {
        let _guard = session_manifest_publication_guard()?;
        let completed = sanitized_completed_session(completed);
        if completed_library_item_id != Self::completed_library_item_id(&completed.id) {
            return Err(invalid_session_publication("restore-task-item-binding"));
        }
        let existing = self
            .read_session_manifest_for_publication(&completed.id)?
            .ok_or_else(|| invalid_session_publication("restore-source-manifest-missing"))?;
        if !source_restore_completion_matches(self, &existing, &completed) {
            return Err(invalid_session_publication("restore-source-proof"));
        }
        let publication = self
            .range_cache
            .begin_session_publication(&completed.id, SessionPublicationMode::OwnedRestoration)
            .map_err(hls_range_error_to_io)?;
        let existing = self
            .read_session_manifest_for_publication(&completed.id)?
            .ok_or_else(|| invalid_session_publication("restore-source-manifest-recheck"))?;
        if !source_restore_completion_matches(self, &existing, &completed) {
            return Err(invalid_session_publication("restore-source-proof-recheck"));
        }
        let session_dir = self.session_dir(&completed.id)?;
        self.ensure_cache_directory(&session_dir)?;
        self.write_json_atomically(
            &session_dir.join("session.json"),
            &PersistedHlsSession::from(completed),
        )?;
        publication.commit();
        Ok(())
    }

    fn session_publication_mode(
        &self,
        session: &HlsPlaybackSession,
        completed: bool,
    ) -> io::Result<SessionPublicationMode> {
        match self.read_session_manifest_for_publication(&session.id)? {
            None => Ok(SessionPublicationMode::Fresh),
            Some(existing)
                if completed && completed_publication_matches(self, &existing, session) =>
            {
                Ok(SessionPublicationMode::OwnedCompletion)
            }
            Some(existing) if !completed && session_refresh_matches(&existing, session) => {
                Ok(SessionPublicationMode::Refresh)
            }
            Some(existing) => {
                let stage =
                    session_refresh_mismatch_stage(&existing, session).unwrap_or(if completed {
                        "completion-proof"
                    } else {
                        "unknown-binding"
                    });
                Err(invalid_session_publication(stage))
            }
        }
    }

    fn validate_session_publication(
        &self,
        session: &HlsPlaybackSession,
        mode: SessionPublicationMode,
    ) -> io::Result<()> {
        match (
            mode,
            self.read_session_manifest_for_publication(&session.id)?,
        ) {
            (SessionPublicationMode::Fresh, None) => Ok(()),
            (SessionPublicationMode::Refresh, Some(existing))
                if session_refresh_matches(&existing, session) =>
            {
                Ok(())
            }
            (SessionPublicationMode::OwnedCompletion, Some(existing))
                if completed_publication_matches(self, &existing, session) =>
            {
                Ok(())
            }
            _ => Err(invalid_session_publication("guard-boundary")),
        }
    }

    fn read_session_manifest_for_publication(
        &self,
        session_id: &str,
    ) -> io::Result<Option<HlsPlaybackSession>> {
        let path = self.session_dir(session_id)?.join("session.json");
        let Some(expected_identity) = safe_file_identity(self, &path)? else {
            return Ok(None);
        };
        let mut options = fs::OpenOptions::new();
        options.read(true);
        let file = open_range_file(&mut options, &path)?;
        if file_object_identity(&file.metadata()?) != expected_identity {
            return Err(invalid_session_publication("manifest-open-identity"));
        }
        let max_bytes = crate::hls_range_cache::RANGE_MAX_MANIFEST_BYTES;
        if file.metadata()?.len() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HLS session manifest exceeds its size bound",
            ));
        }
        let mut bytes = Vec::new();
        file.take(max_bytes + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HLS session manifest exceeds its size bound",
            ));
        }
        if safe_file_identity(self, &path)?.as_deref() != Some(expected_identity.as_str()) {
            return Err(invalid_session_publication("manifest-read-identity"));
        }
        let persisted = serde_json::from_slice::<PersistedHlsSession>(&bytes)
            .map_err(|_| invalid_session_publication("manifest-decode"))?;
        if persisted.schema_version != HLS_CACHE_SCHEMA_VERSION || persisted.id != session_id {
            return Err(invalid_session_publication("manifest-binding"));
        }
        HlsPlaybackSession::try_from(persisted)
            .map(Some)
            .map_err(|_| invalid_session_publication("manifest-validation"))
    }

    fn completed_transcode_matches(
        &self,
        existing: &HlsPlaybackSession,
        completed: &HlsPlaybackSession,
    ) -> bool {
        if existing.id != completed.id
            || existing.effective_policy != completed.effective_policy
            || existing.transcoding.state != HlsTranscodingPlanState::Ready
            || completed.transcoding.state != HlsTranscodingPlanState::NotRequired
            || completed.transcoding.source_variant_id != existing.variant.id
            || completed.variant.id != existing.variant.id
            || completed.variant.video.id != HLS_TRANSCODED_RESOURCE_ID
            || completed.variant.audio.is_some()
            || completed.variant.video.request.kind != BilibiliMediaRequestKind::Video
        {
            return false;
        }
        let source_variants =
            std::iter::once(&existing.variant).chain(existing.alternate_variants.iter());
        if !source_variants.into_iter().all(|source| {
            completed
                .alternate_variants
                .iter()
                .any(|candidate| variants_preserve_resource_bindings(source, candidate))
        }) {
            return false;
        }
        let expected_key = transcoded_cache_key(existing, &completed.variant.codecs);
        if completed.variant.video.request.cache_key != expected_key {
            return false;
        }
        let Some(metadata) = self.read_resource_metadata(&existing.id, HLS_TRANSCODED_RESOURCE_ID)
        else {
            return false;
        };
        if metadata.schema_version != HLS_CACHE_SCHEMA_VERSION
            || metadata.id != HLS_TRANSCODED_RESOURCE_ID
            || BilibiliMediaCacheKey::from(metadata.cache_key) != expected_key
            || metadata.initialization_length == 0
            || metadata.initialization_length >= metadata.total_length
        {
            return false;
        }
        let Ok(path) = self.resource_path(&existing.id, HLS_TRANSCODED_RESOURCE_ID) else {
            return false;
        };
        if !matches!(safe_file_identity(self, &path), Ok(Some(_))) {
            return false;
        }
        fs::metadata(path).is_ok_and(|file| file.len() == metadata.total_length)
    }

    pub(crate) fn session_directory_scan(&self) -> io::Result<HlsCacheSessionDirectoryScan> {
        self.reject_cache_path_symlink(self.root_path.as_ref())?;
        match fs::metadata(self.root_path.as_ref()) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "HLS cache root path is not a directory",
                ));
            }
            Err(error) => return Err(error),
        }

        let store_root = self.store_root();
        self.reject_cache_path_symlink(&store_root)?;
        let entries = match fs::read_dir(store_root) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        Ok(HlsCacheSessionDirectoryScan { entries })
    }

    pub(crate) fn remove_session(&self, session_id: &str) -> io::Result<()> {
        #[cfg(test)]
        if self
            .remove_session_failures
            .lock()
            .expect("HLS remove-session failure lock poisoned")
            .remove(session_id)
        {
            return Err(io::Error::other("injected HLS session removal failure"));
        }
        let session_dir = self.session_dir(session_id)?;
        self.reject_cache_path_symlink(&session_dir)?;
        match fs::remove_dir_all(session_dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_next_remove_session(&self, session_id: impl Into<String>) {
        self.remove_session_failures
            .lock()
            .expect("HLS remove-session failure lock poisoned")
            .insert(session_id.into());
    }

    #[cfg(test)]
    pub(crate) fn remove_session_managed_resources(&self, session_id: &str) -> io::Result<()> {
        self.remove_session_managed_resources_inner(session_id, false)
    }

    pub(crate) fn remove_session_managed_resources_for_eviction(
        &self,
        session_id: &str,
    ) -> io::Result<()> {
        self.remove_session_managed_resources_inner(session_id, true)
    }

    fn remove_session_managed_resources_inner(
        &self,
        session_id: &str,
        remove_transcode_outputs: bool,
    ) -> io::Result<()> {
        let Some(session) = self.load_session(session_id) else {
            return Ok(());
        };
        for resource in session_unique_media_resources(&session) {
            self.remove_cached_resource(session_id, &resource.id)?;
            self.remove_prewarmed_resource(session_id, &resource.id)?;
        }
        if remove_transcode_outputs {
            self.remove_transcode_generated_resources(session_id)?;
        }
        self.remove_unreferenced_session_managed_resources(&session)?;
        Ok(())
    }

    pub(crate) fn load_sessions(&self) -> io::Result<Vec<HlsPlaybackSession>> {
        self.reject_cache_path_symlink(self.root_path.as_ref())?;
        match fs::metadata(self.root_path.as_ref()) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "HLS cache root path is not a directory",
                ));
            }
            Err(error) => return Err(error),
        }

        let store_root = self.store_root();
        self.reject_cache_path_symlink(&store_root)?;
        let entries = match fs::read_dir(store_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };

        let mut sessions = Vec::new();
        for entry in entries.flatten() {
            let session_dir = entry.path();
            let Some(directory_session_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_cache_id(&directory_session_id).is_err() {
                continue;
            }
            if self.reject_cache_path_symlink(&session_dir).is_err() {
                continue;
            }
            let path = session_dir.join("session.json");
            let Some(bytes) = self.read_cache_file(&path) else {
                continue;
            };
            let Ok(persisted) = serde_json::from_slice::<PersistedHlsSession>(&bytes) else {
                continue;
            };
            if persisted.schema_version != HLS_CACHE_SCHEMA_VERSION {
                continue;
            }
            if persisted.id != directory_session_id {
                continue;
            }
            if let Ok(session) = HlsPlaybackSession::try_from(persisted) {
                sessions.push(session);
            }
        }

        sessions.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(sessions)
    }

    pub(crate) fn completed_session_ids(&self, sessions: &[HlsPlaybackSession]) -> HashSet<String> {
        sessions
            .iter()
            .filter_map(|session| {
                self.completed_library_item(session)
                    .map(|_| session.id.clone())
            })
            .collect()
    }

    pub(crate) fn list_completed_library_items(&self) -> Vec<LibraryItem> {
        let sessions = match self.load_sessions() {
            Ok(sessions) => sessions,
            Err(error) => {
                eprintln!("Failed to scan completed HLS cache sessions: {error}");
                return Vec::new();
            }
        };
        let mut items = sessions
            .iter()
            .filter_map(|session| self.completed_library_item(session))
            .collect::<Vec<_>>();
        items.sort_by(|left, right| {
            left.title
                .to_lowercase()
                .cmp(&right.title.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        items
    }

    pub(crate) fn usage_snapshot(&self) -> io::Result<HlsCacheUsageSnapshot> {
        let entries = self.completed_cache_entries()?;
        let used_bytes = self.managed_usage_size_bytes()?;
        Ok(HlsCacheUsageSnapshot {
            used_bytes,
            completed_session_count: entries.len(),
        })
    }

    pub(crate) fn completed_cache_entries(&self) -> io::Result<Vec<HlsCacheCompletedEntry>> {
        self.remove_unreferenced_managed_resources()?;
        let mut entries = self
            .load_sessions()?
            .iter()
            .filter_map(|session| self.completed_cache_entry(session))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.updated_at
                .cmp(&right.updated_at)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        Ok(entries)
    }

    pub(crate) fn partial_cache_entries(&self) -> io::Result<Vec<HlsCachePartialEntry>> {
        self.remove_unreferenced_managed_resources()?;
        let mut entries = Vec::new();
        for session in self.load_sessions()? {
            if let Some(entry) = self.partial_cache_entry(&session)? {
                entries.push(entry);
            }
        }
        entries.sort_by(|left, right| {
            left.updated_at
                .cmp(&right.updated_at)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        Ok(entries)
    }

    pub(crate) fn get_completed_library_item(&self, item_id: &str) -> Option<LibraryItem> {
        let session_id = session_id_from_library_item_id(item_id)?;
        let session = self.load_session(&session_id)?;
        self.completed_library_item(&session)
    }

    pub(crate) fn completed_session(&self, session_id: &str) -> Option<HlsPlaybackSession> {
        let session = self.load_session(session_id)?;
        self.session_is_complete(&session).then_some(session)
    }

    pub(crate) fn source_resources_are_complete(&self, session: &HlsPlaybackSession) -> bool {
        self.source_session_resources_are_complete(session)
    }

    pub(crate) fn playback_session(&self, session_id: &str) -> Option<HlsPlaybackSession> {
        self.load_session(session_id)
    }

    pub(crate) fn create_playback_source(
        &self,
        item_id: &str,
        variant_id: &str,
        uri: String,
    ) -> Option<PlaybackSource> {
        let item = self.get_completed_library_item(item_id)?;
        if !item.variants.iter().any(|variant| variant.id == variant_id) {
            return None;
        }

        Some(PlaybackSource {
            item_id: item_id.to_owned(),
            variant_id: variant_id.to_owned(),
            protocol: PlaybackProtocol::Hls.into(),
            uri,
            expires_at: None,
        })
    }

    pub(crate) fn completed_library_item_id(session_id: &str) -> String {
        format!("{HLS_LIBRARY_ITEM_PREFIX}{session_id}")
    }

    pub(crate) fn session_id_from_library_item_id(item_id: &str) -> Option<String> {
        session_id_from_library_item_id(item_id)
    }

    pub(crate) fn cached_resource(
        &self,
        session_id: &str,
        resource_id: &str,
    ) -> Option<CachedHlsResource> {
        let metadata = self.read_resource_metadata(session_id, resource_id)?;
        if !self.resource_cache_key_matches(session_id, resource_id, &metadata.cache_key) {
            return None;
        }
        let file_path = self.resource_path(session_id, resource_id).ok()?;
        self.reject_cache_path_symlink(&file_path).ok()?;
        let file_metadata = fs::symlink_metadata(&file_path).ok()?;
        if file_metadata.file_type().is_symlink()
            || !file_metadata.is_file()
            || file_metadata.len() != metadata.total_length
            || metadata.initialization_length == 0
            || metadata.initialization_length >= metadata.total_length
        {
            return None;
        }
        let segments = validated_cached_segments(
            metadata.segments,
            metadata.initialization_length,
            metadata.total_length,
        );

        Some(CachedHlsResource {
            path: file_path,
            content_type: metadata.content_type,
            initialization_length: metadata.initialization_length,
            total_length: metadata.total_length,
            segments,
            last_modified: file_metadata.modified().unwrap_or(UNIX_EPOCH),
        })
    }

    pub(crate) fn completed_primary_resource_bytes(
        &self,
        session: &HlsPlaybackSession,
    ) -> io::Result<u64> {
        let stored = self
            .read_session_manifest_for_publication(&session.id)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        if session.transcoding.state == HlsTranscodingPlanState::Ready
            || stored.variant.id != session.variant.id
            || !resource_bindings_match_after_header_scrub(
                &stored.variant.video,
                &session.variant.video,
            )
            || match (&stored.variant.audio, &session.variant.audio) {
                (Some(stored), Some(selected)) => {
                    !resource_bindings_match_after_header_scrub(stored, selected)
                }
                (None, None) => false,
                _ => true,
            }
        {
            return Err(invalid_completed_resource_data());
        }

        let mut resources = Vec::with_capacity(2);
        resources.push(&session.variant.video);
        if let Some(audio) = &session.variant.audio {
            if audio.id == session.variant.video.id {
                return Err(invalid_completed_resource_data());
            }
            resources.push(audio);
        }

        resources.into_iter().try_fold(0_u64, |total, resource| {
            let length = self.completed_resource_length(session, resource)?;
            total
                .checked_add(length)
                .ok_or_else(invalid_completed_resource_data)
        })
    }

    fn completed_resource_length(
        &self,
        session: &HlsPlaybackSession,
        resource: &HlsMediaResource,
    ) -> io::Result<u64> {
        let metadata_path = self.resource_metadata_path(&session.id, &resource.id)?;
        self.reject_cache_path_symlink(&metadata_path)?;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        let metadata_file = open_range_file(&mut options, &metadata_path)?;
        let metadata_length = metadata_file.metadata()?.len();
        if metadata_length > crate::hls_range_cache::RANGE_MAX_MANIFEST_BYTES {
            return Err(invalid_completed_resource_data());
        }
        let mut bytes = Vec::with_capacity(metadata_length as usize);
        metadata_file
            .take(crate::hls_range_cache::RANGE_MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > crate::hls_range_cache::RANGE_MAX_MANIFEST_BYTES {
            return Err(invalid_completed_resource_data());
        }
        let metadata = serde_json::from_slice::<PersistedHlsCachedResource>(&bytes)
            .map_err(|_| invalid_completed_resource_data())?;
        if metadata.schema_version != HLS_CACHE_SCHEMA_VERSION
            || metadata.id != resource.id
            || metadata.cache_key
                != PersistedBilibiliMediaCacheKey::from(resource.request.cache_key.clone())
            || metadata.initialization_length == 0
            || metadata.initialization_length >= metadata.total_length
            || resource
                .request
                .size
                .is_some_and(|size| size != metadata.total_length)
        {
            return Err(invalid_completed_resource_data());
        }

        let data_path = self.resource_path(&session.id, &resource.id)?;
        let expected_identity = safe_file_identity(self, &data_path)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        let data = open_range_file(&mut options, &data_path)?;
        let data_metadata = data.metadata()?;
        if file_object_identity(&data_metadata) != expected_identity
            || data_metadata.len() != metadata.total_length
        {
            return Err(invalid_completed_resource_data());
        }
        Ok(metadata.total_length)
    }

    pub(crate) fn open_cached_resource(
        &self,
        session_id: &str,
        resource_id: &str,
    ) -> Option<OpenedMediaFile> {
        let cached = self.cached_resource(session_id, resource_id)?;
        let relative_path = self.resource_relative_path(session_id, resource_id).ok()?;
        let file = open_read_no_follow(self.root_path.as_ref(), &relative_path).ok()?;
        Some(OpenedMediaFile {
            file,
            content_type: cached.content_type,
            last_modified: cached.last_modified,
            size_bytes: cached.total_length,
        })
    }

    pub(crate) fn prewarmed_resource(
        &self,
        session_id: &str,
        resource_id: &str,
    ) -> Option<PrewarmedHlsResource> {
        let metadata = self.read_prewarmed_resource_metadata(session_id, resource_id)?;
        if !self.resource_cache_key_matches(session_id, resource_id, &metadata.cache_key) {
            return None;
        }
        let file_path = self.resource_prewarm_path(session_id, resource_id).ok()?;
        self.reject_cache_path_symlink(&file_path).ok()?;
        let file_metadata = fs::symlink_metadata(&file_path).ok()?;
        if file_metadata.file_type().is_symlink()
            || !file_metadata.is_file()
            || file_metadata.len() != metadata.prefix_length
        {
            return None;
        }
        if metadata.initialization_length == 0
            || metadata.initialization_length >= metadata.total_length
            || metadata.prefix_length > metadata.total_length
            || metadata.initialization_length > metadata.prefix_length
        {
            return None;
        }

        Some(PrewarmedHlsResource {
            path: file_path,
            content_type: metadata.content_type,
            initialization_length: metadata.initialization_length,
            prefix_length: metadata.prefix_length,
            target_prefix_length: metadata
                .target_prefix_length
                .unwrap_or(metadata.prefix_length),
            target_window_seconds: metadata
                .target_window_seconds
                .unwrap_or(HLS_FIRST_WINDOW_PREFETCH_SECONDS),
            total_length: metadata.total_length,
            last_modified: file_metadata.modified().unwrap_or(UNIX_EPOCH),
        })
    }

    pub(crate) fn open_prewarmed_resource(
        &self,
        session_id: &str,
        resource_id: &str,
    ) -> Option<OpenedPrewarmedHlsResource> {
        let prewarmed = self.prewarmed_resource(session_id, resource_id)?;
        let relative_path = self
            .resource_prewarm_relative_path(session_id, resource_id)
            .ok()?;
        let file = open_read_no_follow(self.root_path.as_ref(), &relative_path).ok()?;
        Some(OpenedPrewarmedHlsResource {
            file,
            content_type: prewarmed.content_type,
            last_modified: prewarmed.last_modified,
            prefix_length: prewarmed.prefix_length,
            total_length: prewarmed.total_length,
        })
    }

    #[cfg(test)]
    pub(crate) async fn cache_session_resources(
        &self,
        client: &reqwest::Client,
        session: &HlsPlaybackSession,
    ) -> Result<String, HlsCacheError> {
        self.cache_session_resources_until(client, session, || false)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn cache_session_resources_until<F>(
        &self,
        client: &reqwest::Client,
        session: &HlsPlaybackSession,
        should_cancel: F,
    ) -> Result<String, HlsCacheError>
    where
        F: Fn() -> bool + Send + Sync,
    {
        self.cache_session_resources_with_control(
            client,
            session,
            || {
                if should_cancel() {
                    HlsCacheFillControl::Cancel
                } else {
                    HlsCacheFillControl::Continue
                }
            },
            |_| {},
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn cache_session_resources_with_control<F, P>(
        &self,
        client: &reqwest::Client,
        session: &HlsPlaybackSession,
        control: F,
        progress: P,
    ) -> Result<String, HlsCacheError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
        P: Fn(HlsCacheFillProgress) + Send + Sync,
    {
        Ok(self
            .cache_session_resources_completion_with_control(
                client, session, control, progress, None,
            )
            .await?
            .library_item_id)
    }

    pub(crate) async fn cache_session_resources_completion_with_control<F, P>(
        &self,
        client: &reqwest::Client,
        session: &HlsPlaybackSession,
        control: F,
        progress: P,
        transcoding: Option<HlsTranscodingExecutionConfig>,
    ) -> Result<HlsCacheCompletion, HlsCacheError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
        P: Fn(HlsCacheFillProgress) + Send + Sync,
    {
        if let Err(error) = check_fill_control(&control) {
            if matches!(&error, HlsCacheError::Cancelled) {
                let _ = self.remove_session(&session.id);
            }
            return Err(error);
        }
        let result = self
            .cache_session_resources_inner(client, session, &control, &progress, transcoding)
            .await;
        if matches!(&result, Err(HlsCacheError::Cancelled)) {
            let _ = self.remove_session(&session.id);
        }
        result
    }

    #[cfg(test)]
    pub(crate) async fn prewarm_session_first_frame_with_control<F>(
        &self,
        client: &reqwest::Client,
        session: &HlsPlaybackSession,
        control: F,
    ) -> Result<(), HlsCacheError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        self.prewarm_session_first_frame_with_playback_progress(client, session, None, control)
            .await
    }

    pub(crate) async fn prewarm_session_first_frame_with_playback_progress<F>(
        &self,
        client: &reqwest::Client,
        session: &HlsPlaybackSession,
        playback_progress: Option<&HlsPlaybackProgressSnapshot>,
        control: F,
    ) -> Result<(), HlsCacheError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        check_fill_control(&control)?;
        self.save_session(session)?;
        let prefetch_context = hls_playback_prefetch_context(session, playback_progress);
        self.prewarm_resource(
            client,
            &session.id,
            &session.variant.video,
            prefetch_context.as_ref(),
            &control,
        )
        .await?;
        if let Some(audio) = &session.variant.audio {
            self.prewarm_resource(
                client,
                &session.id,
                audio,
                prefetch_context.as_ref(),
                &control,
            )
            .await?;
        }
        Ok(())
    }

    async fn cache_session_resources_inner<F, P>(
        &self,
        client: &reqwest::Client,
        session: &HlsPlaybackSession,
        control: &F,
        progress: &P,
        transcoding: Option<HlsTranscodingExecutionConfig>,
    ) -> Result<HlsCacheCompletion, HlsCacheError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
        P: Fn(HlsCacheFillProgress) + Send + Sync,
    {
        self.save_session(session)?;
        let total_bytes = hls_session_declared_size_bytes(session);
        let mut downloaded_bytes = 0_u64;
        progress(HlsCacheFillProgress {
            downloaded_bytes,
            total_bytes,
        });
        downloaded_bytes = downloaded_bytes.saturating_add(
            self.cache_resource(
                client,
                &session.id,
                &session.variant.video,
                control,
                |resource_downloaded_bytes| {
                    progress(HlsCacheFillProgress {
                        downloaded_bytes: downloaded_bytes
                            .saturating_add(resource_downloaded_bytes),
                        total_bytes,
                    });
                },
            )
            .await?,
        );
        progress(HlsCacheFillProgress {
            downloaded_bytes,
            total_bytes,
        });
        if let Some(audio) = &session.variant.audio {
            downloaded_bytes = downloaded_bytes.saturating_add(
                self.cache_resource(
                    client,
                    &session.id,
                    audio,
                    control,
                    |resource_downloaded_bytes| {
                        progress(HlsCacheFillProgress {
                            downloaded_bytes: downloaded_bytes
                                .saturating_add(resource_downloaded_bytes),
                            total_bytes,
                        });
                    },
                )
                .await?,
            );
            progress(HlsCacheFillProgress {
                downloaded_bytes,
                total_bytes,
            });
        }
        let transcode_commit_guard = HlsTranscodingCommitGuard::create_if_needed(self, session)?;
        let completed_session = self
            .transcode_cached_session_if_needed(
                session,
                transcoding,
                control,
                &transcode_commit_guard,
            )
            .await?;
        transcode_commit_guard.refresh()?;
        self.save_completed_session(&completed_session)?;
        transcode_commit_guard.finish();
        if completed_session.variant.video.id == HLS_TRANSCODED_RESOURCE_ID
            && let Err(error) =
                self.remove_unreferenced_session_managed_resources(&completed_session)
        {
            eprintln!(
                "Failed to remove unreferenced HLS source resources after LAN transcoding: {error}"
            );
        }
        self.remove_prewarmed_session_resources(session)?;

        Ok(HlsCacheCompletion {
            library_item_id: Self::completed_library_item_id(&session.id),
            session: completed_session,
        })
    }

    async fn transcode_cached_session_if_needed<F>(
        &self,
        session: &HlsPlaybackSession,
        transcoding: Option<HlsTranscodingExecutionConfig>,
        control: &F,
        transcode_commit_guard: &HlsTranscodingCommitGuard,
    ) -> Result<HlsPlaybackSession, HlsCacheError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        if session.transcoding.state != HlsTranscodingPlanState::Ready {
            return Ok(session.clone());
        }
        let Some(transcoding) = transcoding else {
            return Err(HlsCacheError::InvalidResource(
                "LAN transcoding was planned but no execution config was provided".to_owned(),
            ));
        };

        let _permit = acquire_transcoding_permit(&transcoding, control).await?;
        let _active_job = ActiveTranscodingJob::start(Arc::clone(&transcoding.active_job_count));
        transcode_commit_guard.refresh()?;

        let cached_video = self
            .cached_resource(&session.id, &session.variant.video.id)
            .ok_or_else(|| {
                HlsCacheError::InvalidResource(
                    "LAN transcoding source video was not cached".to_owned(),
                )
            })?;
        let cached_audio = session
            .variant
            .audio
            .as_ref()
            .map(|audio| {
                self.cached_resource(&session.id, &audio.id).ok_or_else(|| {
                    HlsCacheError::InvalidResource(
                        "LAN transcoding source audio was not cached".to_owned(),
                    )
                })
            })
            .transpose()?;

        let output_path = self.resource_path(&session.id, HLS_TRANSCODED_RESOURCE_ID)?;
        let temp_path = transcoding_temp_path_for_output(&output_path);
        self.prepare_temp_path(&temp_path)?;
        self.remove_cached_resource(&session.id, HLS_TRANSCODED_RESOURCE_ID)?;

        let marker_refresh_error = Mutex::new(None);
        let transcode_result = run_hls_ffmpeg_transcode(
            &transcoding.ffmpeg_path,
            &cached_video.path,
            cached_audio.as_ref().map(|audio| audio.path.as_path()),
            &temp_path,
            &|| match control() {
                HlsCacheFillControl::Continue => {
                    if let Err(error) = transcode_commit_guard.refresh_if_due() {
                        if let Ok(mut stored_error) = marker_refresh_error.lock()
                            && stored_error.is_none()
                        {
                            *stored_error = Some(error);
                        }
                        return LanTranscodingJobControl::Cancel;
                    }
                    LanTranscodingJobControl::Continue
                }
                HlsCacheFillControl::Cancel => LanTranscodingJobControl::Cancel,
                HlsCacheFillControl::Preempt => LanTranscodingJobControl::Preempt,
            },
        )
        .await;
        let marker_refresh_error = {
            marker_refresh_error
                .lock()
                .map_err(|_| io::Error::other("HLS transcoding marker refresh state was poisoned"))?
                .take()
        };
        if let Some(error) = marker_refresh_error {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(error.into());
        }
        if let Err(error) = transcode_result {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(hls_cache_error_from_transcoding(error));
        }
        transcode_commit_guard.refresh()?;
        if let Err(error) = check_fill_control(control) {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(error);
        }

        let total_length = tokio::fs::metadata(&temp_path).await?.len();
        let initialization_length = cached_mp4_initialization_length(&temp_path).await?;
        if initialization_length == 0 || initialization_length >= total_length {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(HlsCacheError::InvalidResource(
                "LAN transcoding output MP4 initialization range was invalid".to_owned(),
            ));
        }
        let segments = hls_segments_from_mp4_ranges(
            mp4_fragment_ranges(&temp_path, initialization_length, total_length)
                .unwrap_or_default(),
        );
        if let Err(error) = self.reject_cache_path_symlink(&output_path) {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(error.into());
        }
        tokio::fs::rename(&temp_path, &output_path).await?;

        let completed_session = transcoded_completed_session(session, total_length);
        let metadata = PersistedHlsCachedResource {
            schema_version: HLS_CACHE_SCHEMA_VERSION,
            id: HLS_TRANSCODED_RESOURCE_ID.to_owned(),
            content_type: "video/mp4".to_owned(),
            total_length,
            initialization_length,
            segments: segments
                .into_iter()
                .map(PersistedHlsMediaSegment::from)
                .collect(),
            cache_key: PersistedBilibiliMediaCacheKey::from(
                completed_session.variant.video.request.cache_key.clone(),
            ),
        };
        self.write_json_atomically(
            &self.resource_metadata_path(&session.id, HLS_TRANSCODED_RESOURCE_ID)?,
            &metadata,
        )?;
        Ok(completed_session)
    }

    fn remove_unreferenced_managed_resources(&self) -> io::Result<()> {
        for session in self.load_sessions()? {
            self.remove_unreferenced_session_managed_resources(&session)?;
        }
        Ok(())
    }

    fn remove_unreferenced_session_managed_resources(
        &self,
        session: &HlsPlaybackSession,
    ) -> io::Result<()> {
        let session_dir = self.session_dir(&session.id)?;
        self.reject_cache_path_symlink(&session_dir)?;
        let mut retained = referenced_session_managed_file_names(session);
        if self.transcoding_commit_marker_is_active(session)? {
            insert_resource_managed_file_names(&mut retained, HLS_TRANSCODED_RESOURCE_ID);
            retained.insert(transcoding_temp_file_name(HLS_TRANSCODED_RESOURCE_ID));
        }
        let entries = match fs::read_dir(session_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };

        for entry in entries {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            if retained.contains(file_name) || !is_managed_resource_file_name(file_name) {
                continue;
            }
            self.remove_managed_cache_file_if_exists(&entry.path())?;
        }
        Ok(())
    }

    fn transcoding_commit_marker_is_active(
        &self,
        session: &HlsPlaybackSession,
    ) -> io::Result<bool> {
        let marker_path = self.transcoding_commit_marker_path(&session.id)?;
        self.reject_cache_path_symlink(&marker_path)?;
        let metadata = match fs::metadata(&marker_path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "HLS transcoding commit marker already exists and is not a file",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };

        if session.transcoding.state != HlsTranscodingPlanState::Ready {
            self.remove_transcoding_commit_marker_if_exists(&session.id)?;
            return Ok(false);
        }

        match metadata.modified()?.elapsed() {
            Ok(age) if age > HLS_TRANSCODING_COMMIT_MARKER_TTL => {
                self.remove_transcoding_commit_marker_if_exists(&session.id)?;
                Ok(false)
            }
            Ok(_) | Err(_) => Ok(true),
        }
    }

    async fn prewarm_resource<F>(
        &self,
        client: &reqwest::Client,
        session_id: &str,
        resource: &HlsMediaResource,
        prefetch_context: Option<&HlsPlaybackPrefetchContext>,
        control: &F,
    ) -> Result<(), HlsCacheError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        check_fill_control(control)?;
        if self.cached_resource(session_id, &resource.id).is_some() {
            return Ok(());
        }
        let target = hls_prefetch_prefix_target(resource, prefetch_context);
        if let Some(prewarmed) = self.prewarmed_resource(session_id, &resource.id) {
            let target_prefix_length = target.prefix_bytes.min(prewarmed.total_length);
            if prewarmed.prefix_length >= target_prefix_length {
                return Ok(());
            }
        }

        let session_dir = self.session_dir(session_id)?;
        self.ensure_cache_directory(&session_dir)?;
        let prewarm_path = self.resource_prewarm_path(session_id, &resource.id)?;
        let temp_path = prewarm_path.with_extension("tmp");
        let mut last_error = None;
        for url in self.cdn_history.rank_request(&resource.request) {
            if url.trim().is_empty() {
                continue;
            }
            check_fill_control(control)?;
            self.prepare_temp_path(&temp_path)?;
            match download_resource_prefix(
                client,
                resource,
                &url,
                &temp_path,
                target,
                control,
                &self.cdn_history,
            )
            .await
            {
                Ok(prefix) => {
                    if prefix.initialization_length == 0
                        || prefix.initialization_length >= prefix.total_length
                        || prefix.initialization_length > prefix.prefix_length
                    {
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        last_error = Some(HlsCacheError::InvalidResource(
                            "prewarmed HLS MP4 initialization range was invalid".to_owned(),
                        ));
                        self.record_cdn_observation(
                            resource,
                            &prefix.final_url,
                            passive_cache_observation(
                                CdnObservationOutcome::IntegrityMismatch,
                                prefix.prefix_length,
                                None,
                                None,
                                Some(false),
                            ),
                        );
                        continue;
                    }
                    if let Err(error) = check_fill_control(control) {
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        return Err(error);
                    }
                    if let Err(error) = self.reject_cache_path_symlink(&prewarm_path) {
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        return Err(error.into());
                    }
                    tokio::fs::rename(&temp_path, &prewarm_path).await?;
                    let metadata = PersistedHlsPrewarmedResource {
                        schema_version: HLS_CACHE_SCHEMA_VERSION,
                        id: resource.id.clone(),
                        content_type: resource.content_type().to_owned(),
                        prefix_length: prefix.prefix_length,
                        target_prefix_length: Some(prefix.target_prefix_length),
                        target_window_seconds: Some(prefix.target_window_seconds),
                        total_length: prefix.total_length,
                        initialization_length: prefix.initialization_length,
                        cache_key: PersistedBilibiliMediaCacheKey::from(
                            resource.request.cache_key.clone(),
                        ),
                    };
                    self.write_json_atomically(
                        &self.resource_prewarm_metadata_path(session_id, &resource.id)?,
                        &metadata,
                    )?;
                    self.record_cdn_observation(
                        resource,
                        &prefix.final_url,
                        passive_cache_observation(
                            CdnObservationOutcome::Partial,
                            prefix.prefix_length,
                            None,
                            None,
                            Some(true),
                        ),
                    );
                    check_fill_control(control)?;
                    return Ok(());
                }
                Err(error) => {
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            HlsCacheError::InvalidResource("HLS media request did not contain a URL".to_owned())
        }))
    }

    fn completed_library_item(&self, session: &HlsPlaybackSession) -> Option<LibraryItem> {
        let entry = self.completed_cache_entry(session)?;
        let cached_video = self.cached_resource(&session.id, &session.variant.video.id)?;

        Some(LibraryItem {
            id: Self::completed_library_item_id(&session.id),
            title: session.title.clone(),
            subtitle: "Bilibili offline HLS cache".to_owned(),
            source: LibrarySource::Bilibili.into(),
            source_id: session.id.clone(),
            poster_uri: String::new(),
            variants: vec![MediaVariant {
                id: session.variant.id.clone(),
                label: HLS_CACHE_VARIANT_LABEL.to_owned(),
                protocol: PlaybackProtocol::Hls.into(),
                container: "hls".to_owned(),
                video_codec: session.variant.codecs.first().cloned().unwrap_or_default(),
                audio_codec: completed_variant_audio_codec(&session.variant),
                width: session
                    .variant
                    .width
                    .unwrap_or_default()
                    .try_into()
                    .unwrap_or(i32::MAX),
                height: session
                    .variant
                    .height
                    .unwrap_or_default()
                    .try_into()
                    .unwrap_or(i32::MAX),
                bitrate: session.variant.bandwidth.try_into().unwrap_or(i64::MAX),
                size_bytes: entry.size_bytes.try_into().unwrap_or(i64::MAX),
            }],
            created_at: Some(timestamp_from_system_time(
                created_time_for_path(&cached_video.path).unwrap_or(UNIX_EPOCH),
            )),
            updated_at: Some(timestamp_from_system_time(entry.updated_at)),
        })
    }

    fn completed_cache_entry(
        &self,
        session: &HlsPlaybackSession,
    ) -> Option<HlsCacheCompletedEntry> {
        if !self.session_is_complete(session) {
            return None;
        }
        let size_bytes = self.session_managed_resource_size(session).ok()?;
        let updated_at = self
            .resource_modification_times(session)
            .into_iter()
            .max()
            .unwrap_or(UNIX_EPOCH);

        Some(HlsCacheCompletedEntry {
            session_id: session.id.clone(),
            library_item_id: Self::completed_library_item_id(&session.id),
            size_bytes,
            updated_at,
        })
    }

    fn partial_cache_entry(
        &self,
        session: &HlsPlaybackSession,
    ) -> io::Result<Option<HlsCachePartialEntry>> {
        if self.session_is_complete(session) {
            return Ok(None);
        }
        let size_bytes = self.session_managed_resource_size(session)?;
        if size_bytes == 0 {
            return Ok(None);
        }
        let updated_at = self
            .managed_resource_modification_times(session)
            .into_iter()
            .max()
            .unwrap_or(UNIX_EPOCH);

        Ok(Some(HlsCachePartialEntry {
            session_id: session.id.clone(),
            size_bytes,
            updated_at,
        }))
    }

    fn session_is_complete(&self, session: &HlsPlaybackSession) -> bool {
        if session.transcoding.state == HlsTranscodingPlanState::Ready {
            return false;
        }
        self.source_session_resources_are_complete(session)
    }

    fn source_session_resources_are_complete(&self, session: &HlsPlaybackSession) -> bool {
        self.cached_resource(&session.id, &session.variant.video.id)
            .is_some_and(|cached| {
                session
                    .variant
                    .video
                    .request
                    .size
                    .is_none_or(|size| size == cached.total_length)
            })
            && session.variant.audio.as_ref().is_none_or(|audio| {
                self.cached_resource(&session.id, &audio.id)
                    .is_some_and(|cached| {
                        audio
                            .request
                            .size
                            .is_none_or(|size| size == cached.total_length)
                    })
            })
    }

    pub(crate) fn session_projected_remaining_size_bytes(
        &self,
        session: &HlsPlaybackSession,
    ) -> Option<u64> {
        let mut total = 0_u64;
        for resource in session
            .variant
            .audio
            .iter()
            .chain(std::iter::once(&session.variant.video))
        {
            let declared_size = resource.request.size?;
            let cached_size = self
                .cached_resource(&session.id, &resource.id)
                .map(|cached| cached.total_length)
                .unwrap_or_default()
                .min(declared_size);
            total = total.checked_add(declared_size.saturating_sub(cached_size))?;
        }
        Some(total)
    }

    pub(crate) fn session_projected_finalization_added_size_bytes(
        &self,
        session: &HlsPlaybackSession,
    ) -> Option<u64> {
        let remaining_source_bytes = self
            .session_projected_remaining_size_bytes(session)
            .unwrap_or_default();
        let transcoded_output_bytes =
            self.session_projected_transcode_output_size_bytes(session)?;
        remaining_source_bytes.checked_add(transcoded_output_bytes)
    }

    fn session_projected_transcode_output_size_bytes(
        &self,
        session: &HlsPlaybackSession,
    ) -> Option<u64> {
        if session.transcoding.state != HlsTranscodingPlanState::Ready {
            return Some(0);
        }

        let duration_bytes = u64::from(session.variant.duration_seconds)
            .checked_mul(transcoded_bandwidth(session.variant.audio.is_some()))?
            .checked_add(7)?
            / 8;
        let known_source_bytes = self
            .session_known_source_size_floor(session)
            .unwrap_or_default();
        Some(duration_bytes.max(known_source_bytes))
    }

    fn session_known_source_size_floor(&self, session: &HlsPlaybackSession) -> Option<u64> {
        let mut total = 0_u64;
        let mut found_known_size = false;
        for resource in session
            .variant
            .audio
            .iter()
            .chain(std::iter::once(&session.variant.video))
        {
            let known_size = resource
                .request
                .size
                .into_iter()
                .chain(
                    self.cached_resource(&session.id, &resource.id)
                        .map(|cached| cached.total_length),
                )
                .max();
            if let Some(known_size) = known_size {
                found_known_size = true;
                total = total.checked_add(known_size)?;
            }
        }
        found_known_size.then_some(total)
    }

    fn managed_usage_size_bytes(&self) -> io::Result<u64> {
        let mut total = 0_u64;
        for session in self.load_sessions()? {
            total = total.saturating_add(self.session_managed_resource_size(&session)?);
        }
        Ok(total)
    }

    fn session_managed_resource_size(&self, session: &HlsPlaybackSession) -> io::Result<u64> {
        let mut source_size = 0_u64;
        for resource in session_unique_media_resources(session) {
            source_size =
                source_size.saturating_add(self.resource_managed_size(&session.id, &resource.id)?);
        }
        Ok(source_size.saturating_add(self.active_transcode_managed_size(session)?))
    }

    fn active_transcode_managed_size(&self, session: &HlsPlaybackSession) -> io::Result<u64> {
        if !self.transcoding_commit_marker_is_active(session)? {
            return Ok(0);
        }
        let generated_size = self
            .resource_managed_size(&session.id, HLS_TRANSCODED_RESOURCE_ID)?
            .max(self.managed_file_size(
                &self.resource_path(&session.id, HLS_TRANSCODED_RESOURCE_ID)?,
            )?);
        let temp_size = self.managed_file_size(&self.resource_path(
            &session.id,
            &transcoding_temp_file_name(HLS_TRANSCODED_RESOURCE_ID),
        )?)?;
        Ok(generated_size.saturating_add(temp_size))
    }

    fn resource_managed_size(&self, session_id: &str, resource_id: &str) -> io::Result<u64> {
        if let Some(cached) = self.cached_resource(session_id, resource_id) {
            Ok(cached.total_length)
        } else if let Some(prewarmed) = self.prewarmed_resource(session_id, resource_id) {
            Ok(prewarmed.prefix_length)
        } else {
            self.range_managed_size(session_id, resource_id)
        }
    }

    fn managed_file_size(&self, path: &Path) -> io::Result<u64> {
        self.reject_cache_path_symlink(path)?;
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => Ok(metadata.len()),
            Ok(_) => Ok(0),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error),
        }
    }

    fn remove_prewarmed_session_resources(&self, session: &HlsPlaybackSession) -> io::Result<()> {
        for resource in session
            .variant
            .audio
            .iter()
            .chain(std::iter::once(&session.variant.video))
        {
            self.remove_prewarmed_resource(&session.id, &resource.id)?;
        }
        Ok(())
    }

    fn resource_modification_times(&self, session: &HlsPlaybackSession) -> Vec<SystemTime> {
        session_unique_media_resources(session)
            .into_iter()
            .filter_map(|resource| self.cached_resource(&session.id, &resource.id))
            .map(|resource| resource.last_modified)
            .collect()
    }

    fn managed_resource_modification_times(&self, session: &HlsPlaybackSession) -> Vec<SystemTime> {
        session_unique_media_resources(session)
            .into_iter()
            .filter_map(|resource| {
                self.cached_resource(&session.id, &resource.id)
                    .map(|resource| resource.last_modified)
                    .or_else(|| {
                        self.prewarmed_resource(&session.id, &resource.id)
                            .map(|resource| resource.last_modified)
                    })
            })
            .collect()
    }

    fn load_session(&self, session_id: &str) -> Option<HlsPlaybackSession> {
        let path = self.session_dir(session_id).ok()?.join("session.json");
        let bytes = self.read_cache_file(&path)?;
        let persisted = serde_json::from_slice::<PersistedHlsSession>(&bytes).ok()?;
        if persisted.schema_version != HLS_CACHE_SCHEMA_VERSION {
            return None;
        }
        if persisted.id != session_id {
            return None;
        }

        HlsPlaybackSession::try_from(persisted).ok()
    }

    async fn cache_resource<F, P>(
        &self,
        client: &reqwest::Client,
        session_id: &str,
        resource: &HlsMediaResource,
        control: &F,
        progress: P,
    ) -> Result<u64, HlsCacheError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
        P: Fn(u64) + Send + Sync,
    {
        check_fill_control(control)?;
        if let Some(cached) = self.cached_resource(session_id, &resource.id) {
            if self
                .load_range_manifest_serialized(session_id, resource)
                .await
                .map_err(hls_cache_error_from_range)?
                .is_some()
            {
                self.finalize_range_resource(session_id, resource, cached.total_length)
                    .await
                    .map_err(hls_cache_error_from_range)?;
            }
            progress(cached.total_length);
            return Ok(cached.total_length);
        }
        let status = self
            .fill_missing_ranges_with_progress(
                client,
                session_id,
                resource,
                HlsRangePriority::Background,
                control,
                &progress,
            )
            .await
            .map_err(hls_cache_error_from_range)?;
        let total_length = match status {
            HlsRangeResourceStatus::Complete { total_length } => total_length,
            HlsRangeResourceStatus::Partial { .. } => {
                return Err(HlsCacheError::InvalidResource(
                    "HLS range fill ended without complete resource coverage".to_owned(),
                ));
            }
        };
        self.finalize_range_resource(session_id, resource, total_length)
            .await
            .map_err(hls_cache_error_from_range)?;
        self.remove_prewarmed_resource(session_id, &resource.id)?;
        check_fill_control(control)?;
        progress(total_length);
        Ok(total_length)
    }

    fn record_cdn_observation(
        &self,
        resource: &HlsMediaResource,
        final_url: &str,
        observation: CdnObservation,
    ) {
        let attribution_url = response_candidate_url(&resource.request, final_url);
        let Some(attribution_url) = attribution_url else {
            return;
        };
        self.cdn_history
            .record_request(&resource.request, &attribution_url, observation);
    }

    fn read_resource_metadata(
        &self,
        session_id: &str,
        resource_id: &str,
    ) -> Option<PersistedHlsCachedResource> {
        let bytes =
            self.read_cache_file(&self.resource_metadata_path(session_id, resource_id).ok()?)?;
        let metadata = serde_json::from_slice::<PersistedHlsCachedResource>(&bytes).ok()?;
        (metadata.schema_version == HLS_CACHE_SCHEMA_VERSION && metadata.id == resource_id)
            .then_some(metadata)
    }

    fn read_prewarmed_resource_metadata(
        &self,
        session_id: &str,
        resource_id: &str,
    ) -> Option<PersistedHlsPrewarmedResource> {
        let bytes = self.read_cache_file(
            &self
                .resource_prewarm_metadata_path(session_id, resource_id)
                .ok()?,
        )?;
        let metadata = serde_json::from_slice::<PersistedHlsPrewarmedResource>(&bytes).ok()?;
        (metadata.schema_version == HLS_CACHE_SCHEMA_VERSION && metadata.id == resource_id)
            .then_some(metadata)
    }

    fn resource_cache_key_matches(
        &self,
        session_id: &str,
        resource_id: &str,
        cache_key: &PersistedBilibiliMediaCacheKey,
    ) -> bool {
        let Some(session) = self.load_session(session_id) else {
            return false;
        };
        let Some(resource) = session.media_resource(resource_id) else {
            return false;
        };
        *cache_key == PersistedBilibiliMediaCacheKey::from(resource.request.cache_key.clone())
    }

    fn store_root(&self) -> PathBuf {
        self.root_path.join(HLS_CACHE_DIR)
    }

    fn session_dir(&self, session_id: &str) -> io::Result<PathBuf> {
        validate_cache_id(session_id)?;
        Ok(self.store_root().join(session_id))
    }

    fn resource_path(&self, session_id: &str, resource_id: &str) -> io::Result<PathBuf> {
        validate_cache_id(resource_id)?;
        Ok(self.session_dir(session_id)?.join(resource_id))
    }

    fn resource_prewarm_path(&self, session_id: &str, resource_id: &str) -> io::Result<PathBuf> {
        validate_cache_id(resource_id)?;
        Ok(self
            .session_dir(session_id)?
            .join(format!("{resource_id}.prewarm")))
    }

    fn resource_range_data_path(&self, session_id: &str, resource_id: &str) -> io::Result<PathBuf> {
        validate_cache_id(resource_id)?;
        Ok(self
            .session_dir(session_id)?
            .join(format!("{resource_id}.range.data")))
    }

    fn resource_range_manifest_path(
        &self,
        session_id: &str,
        resource_id: &str,
    ) -> io::Result<PathBuf> {
        validate_cache_id(resource_id)?;
        Ok(self
            .session_dir(session_id)?
            .join(format!("{resource_id}.range.json")))
    }

    fn resource_range_full_get_temp_path(
        &self,
        session_id: &str,
        resource_id: &str,
    ) -> io::Result<PathBuf> {
        validate_cache_id(resource_id)?;
        Ok(self
            .session_dir(session_id)?
            .join(format!("{resource_id}.range-full.tmp")))
    }

    fn resource_relative_path(&self, session_id: &str, resource_id: &str) -> io::Result<String> {
        validate_cache_id(session_id)?;
        validate_cache_id(resource_id)?;
        Ok(format!("{HLS_CACHE_DIR}/{session_id}/{resource_id}"))
    }

    fn resource_prewarm_relative_path(
        &self,
        session_id: &str,
        resource_id: &str,
    ) -> io::Result<String> {
        validate_cache_id(session_id)?;
        validate_cache_id(resource_id)?;
        Ok(format!(
            "{HLS_CACHE_DIR}/{session_id}/{resource_id}.prewarm"
        ))
    }

    fn resource_metadata_path(&self, session_id: &str, resource_id: &str) -> io::Result<PathBuf> {
        validate_cache_id(resource_id)?;
        Ok(self
            .session_dir(session_id)?
            .join(format!("{resource_id}.json")))
    }

    fn resource_prewarm_metadata_path(
        &self,
        session_id: &str,
        resource_id: &str,
    ) -> io::Result<PathBuf> {
        validate_cache_id(resource_id)?;
        Ok(self
            .session_dir(session_id)?
            .join(format!("{resource_id}.prewarm.json")))
    }

    fn transcoding_commit_marker_path(&self, session_id: &str) -> io::Result<PathBuf> {
        Ok(self
            .session_dir(session_id)?
            .join(HLS_TRANSCODING_COMMIT_MARKER_FILE))
    }

    fn remove_cached_resource(&self, session_id: &str, resource_id: &str) -> io::Result<()> {
        self.remove_managed_cache_file_if_exists(&self.resource_path(session_id, resource_id)?)?;
        self.remove_managed_cache_file_if_exists(
            &self.resource_metadata_path(session_id, resource_id)?,
        )?;
        self.remove_managed_cache_file_if_exists(
            &self.resource_range_data_path(session_id, resource_id)?,
        )?;
        self.remove_managed_cache_file_if_exists(
            &self.resource_range_manifest_path(session_id, resource_id)?,
        )?;
        self.remove_managed_cache_file_if_exists(
            &self.resource_range_full_get_temp_path(session_id, resource_id)?,
        )
    }

    fn remove_prewarmed_resource(&self, session_id: &str, resource_id: &str) -> io::Result<()> {
        self.remove_managed_cache_file_if_exists(
            &self.resource_prewarm_path(session_id, resource_id)?,
        )?;
        self.remove_managed_cache_file_if_exists(
            &self.resource_prewarm_metadata_path(session_id, resource_id)?,
        )
    }

    fn remove_transcode_generated_resources(&self, session_id: &str) -> io::Result<()> {
        self.remove_cached_resource(session_id, HLS_TRANSCODED_RESOURCE_ID)?;
        self.remove_managed_cache_file_if_exists(&self.resource_path(
            session_id,
            &transcoding_temp_file_name(HLS_TRANSCODED_RESOURCE_ID),
        )?)?;
        self.remove_transcoding_commit_marker_if_exists(session_id)
    }

    fn remove_transcoding_commit_marker_if_exists(&self, session_id: &str) -> io::Result<()> {
        let path = self.transcoding_commit_marker_path(session_id)?;
        self.reject_cache_path_symlink(&path)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "HLS transcoding commit marker path must not be a symlink",
            )),
            Ok(metadata) if metadata.is_file() => {
                fs::remove_file(self.transcoding_commit_marker_path(session_id)?)
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "HLS transcoding commit marker already exists and is not a file",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn remove_managed_cache_file_if_exists(&self, path: &Path) -> io::Result<()> {
        self.reject_cache_path_symlink(path)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "HLS cache managed file path must not be a symlink",
            )),
            Ok(metadata) if metadata.is_file() => fs::remove_file(path),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "HLS cache managed file path already exists and is not a file",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn ensure_cache_directory(&self, path: &Path) -> io::Result<()> {
        self.reject_cache_path_symlink(path)?;
        fs::create_dir_all(path)?;
        self.reject_cache_path_symlink(path)?;
        let metadata = fs::metadata(path)?;
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "HLS cache path is not a directory",
            ));
        }
        Ok(())
    }

    fn prepare_temp_path(&self, path: &Path) -> io::Result<()> {
        self.reject_cache_path_symlink(path)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "HLS cache temp path must not be a symlink",
            )),
            Ok(metadata) if metadata.is_file() => fs::remove_file(path),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "HLS cache temp path already exists and is not a file",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn write_json_atomically<T: Serialize>(&self, path: &Path, value: &T) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            self.ensure_cache_directory(parent)?;
        }
        let temp_path = path.with_extension("tmp");
        self.prepare_temp_path(&temp_path)?;
        let bytes = serde_json::to_vec_pretty(value).map_err(invalid_data)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        if let Err(error) = self.reject_cache_path_symlink(path) {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }
        fs::rename(temp_path, path)
    }

    fn reject_cache_path_symlink(&self, path: &Path) -> io::Result<()> {
        if cache_path_contains_symlink(self.root_path.as_ref(), path)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "HLS cache path must not contain symlinks",
            ));
        }
        Ok(())
    }

    fn read_cache_file(&self, path: &Path) -> Option<Vec<u8>> {
        self.reject_cache_path_symlink(path).ok()?;
        fs::read(path).ok()
    }
}

#[cfg_attr(test, derive(Clone))]
struct LoadedRangeManifest {
    manifest: PersistedRangeManifest,
    bytes_digest: String,
    file_identity: String,
}

enum RangeEnsureState {
    Completed(HlsReadyRange),
    Partial(Box<LoadedRangeManifest>),
}

impl HlsCacheStore {
    fn range_resource_key(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
    ) -> Result<RangeResourceKey, HlsRangeError> {
        validate_cache_id(session_id)?;
        validate_cache_id(&resource.id)?;
        Ok(RangeResourceKey {
            session_id: session_id.to_owned(),
            resource_id: resource.id.clone(),
            representation: range_representation_digest(resource),
        })
    }

    fn range_publication_lock(&self, key: &RangeResourceKey) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .range_checkpoint_locks
            .lock()
            .expect("HLS range publication lock registry poisoned");
        if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(key.clone(), Arc::downgrade(&lock));
        lock
    }

    fn range_managed_size(&self, session_id: &str, resource_id: &str) -> io::Result<u64> {
        // Charge staged media files only; sparse file length is conservative quota usage, never progress.
        let mut total = 0_u64;
        for path in [
            self.resource_range_data_path(session_id, resource_id)?,
            self.resource_range_full_get_temp_path(session_id, resource_id)?,
            self.resource_path(session_id, resource_id)?,
        ] {
            total = total.saturating_add(self.managed_file_size(&path)?);
        }
        Ok(total)
    }

    fn ensure_range_manifest(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        key: &RangeResourceKey,
    ) -> Result<LoadedRangeManifest, HlsRangeError> {
        if let Some(loaded) = self.load_range_manifest(session_id, resource)? {
            return Ok(loaded);
        }
        let data_path = self.resource_range_data_path(session_id, &resource.id)?;
        let manifest_path = self.resource_range_manifest_path(session_id, &resource.id)?;
        let parent = data_path.parent().ok_or_else(|| {
            HlsRangeError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid cache path",
            ))
        })?;
        self.ensure_cache_directory(parent)?;
        self.reject_cache_path_symlink(&data_path)?;
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        let data = open_range_file(&mut options, &data_path)?;
        data.sync_all()?;
        let identity = file_object_identity(&data.metadata()?);
        sync_directory(parent)?;
        let manifest = PersistedRangeManifest {
            schema_version: crate::hls_range_cache::RANGE_MANIFEST_SCHEMA_VERSION,
            generation: 1,
            resource_id: resource.id.clone(),
            representation_digest: key.representation.clone(),
            data_identity: Some(identity),
            total_length: resource.request.size,
            strong_etag: None,
            validator_origin: None,
            last_modified: None,
            prefix_length: 0,
            prefix_sha256: None,
            durable_bytes: 0,
            validated_origins: Vec::new(),
            extents: Vec::new(),
        };
        self.write_json_atomically(&manifest_path, &manifest)?;
        sync_directory(parent)?;
        self.load_range_manifest(session_id, resource)?
            .ok_or(HlsRangeError::IdentityChanged)
    }

    async fn publish_range_manifest<F>(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        expected: LoadedRangeManifest,
        update: F,
    ) -> Result<LoadedRangeManifest, HlsRangeError>
    where
        F: FnOnce(&mut PersistedRangeManifest),
    {
        let key = self.range_resource_key(session_id, resource)?;
        let publication_lock = self.range_publication_lock(&key);
        let _publication = publication_lock.lock().await;
        let current = self
            .load_range_manifest(session_id, resource)?
            .ok_or(HlsRangeError::IdentityChanged)?;
        if !range_manifest_rebase_compatible(&expected.manifest, &current.manifest) {
            return Err(identity_error_at("checkpoint-rebase-identity"));
        }
        let mut manifest = current.manifest.clone();
        update(&mut manifest);
        manifest.generation = manifest
            .generation
            .checked_add(1)
            .ok_or(HlsRangeError::IdentityChanged)?;
        manifest.durable_bytes = durable_extent_bytes(&manifest.extents);
        validate_range_manifest(&manifest, resource)?;
        let path = self.resource_range_manifest_path(session_id, &resource.id)?;
        let latest = self
            .load_range_manifest(session_id, resource)?
            .ok_or(HlsRangeError::IdentityChanged)?;
        if latest.file_identity != current.file_identity
            || latest.bytes_digest != current.bytes_digest
        {
            return Err(identity_error_at("checkpoint-rebase-content"));
        }
        self.write_json_atomically(&path, &manifest)?;
        sync_directory(path.parent().ok_or_else(|| {
            HlsRangeError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid cache path",
            ))
        })?)?;
        self.load_range_manifest(session_id, resource)?
            .ok_or(HlsRangeError::IdentityChanged)
    }

    async fn publish_validated_origin(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        expected: LoadedRangeManifest,
        origin: String,
        strong_etag: Option<String>,
    ) -> Result<LoadedRangeManifest, HlsRangeError> {
        let prefix_sha256 = expected
            .manifest
            .prefix_sha256
            .clone()
            .ok_or(HlsRangeError::IdentityChanged)?;
        let key = self.range_resource_key(session_id, resource)?;
        let publication_lock = self.range_publication_lock(&key);
        let _publication = publication_lock.lock().await;
        let current = self
            .load_range_manifest(session_id, resource)?
            .ok_or(HlsRangeError::IdentityChanged)?;
        if !range_manifest_rebase_compatible_for_origin_validation(
            &expected.manifest,
            &current.manifest,
            &origin,
        ) {
            return Err(identity_error_at("origin-binding-rebase"));
        }
        let binding_state = |manifest: &PersistedRangeManifest| {
            manifest
                .validated_origins
                .iter()
                .find(|binding| binding.origin == origin)
                .map(|binding| (binding.prefix_sha256.clone(), binding.strong_etag.clone()))
        };
        let expected_binding = binding_state(&expected.manifest);
        let current_binding = binding_state(&current.manifest);
        let desired_binding = Some((prefix_sha256.clone(), strong_etag.clone()));
        if current_binding == desired_binding {
            return Ok(current);
        }
        if current_binding != expected_binding {
            return Err(identity_error_at("origin-binding-validator-conflict"));
        }

        let mut manifest = current.manifest.clone();
        if let Some(binding) = manifest
            .validated_origins
            .iter_mut()
            .find(|binding| binding.origin == origin)
        {
            binding.prefix_sha256 = prefix_sha256;
            binding.strong_etag = strong_etag;
        } else {
            manifest.validated_origins.push(PersistedRangeOrigin {
                origin,
                prefix_sha256,
                strong_etag,
            });
            manifest.validated_origins.truncate(64);
        }
        manifest.generation = manifest
            .generation
            .checked_add(1)
            .ok_or(HlsRangeError::IdentityChanged)?;
        manifest.durable_bytes = durable_extent_bytes(&manifest.extents);
        validate_range_manifest(&manifest, resource)?;
        let path = self.resource_range_manifest_path(session_id, &resource.id)?;
        let latest = self
            .load_range_manifest(session_id, resource)?
            .ok_or(HlsRangeError::IdentityChanged)?;
        if latest.file_identity != current.file_identity
            || latest.bytes_digest != current.bytes_digest
            || !range_manifest_rebase_compatible(&current.manifest, &latest.manifest)
        {
            return Err(identity_error_at("origin-binding-publish-content"));
        }
        self.write_json_atomically(&path, &manifest)?;
        sync_directory(path.parent().ok_or_else(|| {
            HlsRangeError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid cache path",
            ))
        })?)?;
        self.load_range_manifest(session_id, resource)?
            .ok_or(HlsRangeError::IdentityChanged)
    }

    fn data_path_for_manifest(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        manifest: &PersistedRangeManifest,
    ) -> Result<PathBuf, HlsRangeError> {
        let partial = self.resource_range_data_path(session_id, &resource.id)?;
        match safe_file_identity(self, &partial)? {
            Some(identity) if Some(identity.as_str()) == manifest.data_identity.as_deref() => {
                return Ok(partial);
            }
            Some(_) => {}
            None => {}
        }
        let final_path = self.resource_path(session_id, &resource.id)?;
        match safe_file_identity(self, &final_path)? {
            Some(identity) if Some(identity.as_str()) == manifest.data_identity.as_deref() => {
                Ok(final_path)
            }
            Some(_) => Err(identity_error_at("completed-path-not-regular")),
            None => Err(HlsRangeError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "checkpoint data is missing",
            ))),
        }
    }

    // Keep the response validators and expected manifest snapshot explicit at publication.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    async fn commit_range_extent(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        expected: &PersistedRangeManifest,
        range: Range<u64>,
        bytes: &[u8],
        final_url: &str,
        etag: Option<String>,
        last_modified: Option<String>,
    ) -> Result<u64, HlsRangeError> {
        self.commit_range_extent_with_failure(
            session_id,
            resource,
            range,
            RangeExtentCommitInput {
                expected,
                bytes,
                final_url,
                etag: &etag,
                last_modified: &last_modified,
            },
        )
        .await
        .map_err(|failure| match failure {
            RangeExtentCommitFailure::StaleTargetValidator => HlsRangeError::IdentityChanged,
            RangeExtentCommitFailure::Rejected(error) => error,
        })
    }

    async fn fetch_and_commit_range_extent<F, Fut>(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        range: Range<u64>,
        mut fetch: F,
    ) -> Result<(u64, RangeChunkFetch), HlsRangeError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<RangeChunkFetch, HlsRangeError>>,
    {
        let mut fetched = fetch().await?;
        let mut retried = false;
        loop {
            let (bytes, final_url, etag, last_modified, _, expected) = &fetched;
            match self
                .commit_range_extent_with_failure(
                    session_id,
                    resource,
                    range.clone(),
                    RangeExtentCommitInput {
                        expected,
                        bytes,
                        final_url,
                        etag,
                        last_modified,
                    },
                )
                .await
            {
                Ok(durable_bytes) => return Ok((durable_bytes, fetched)),
                Err(RangeExtentCommitFailure::StaleTargetValidator) if !retried => {
                    retried = true;
                    drop(fetched);
                    fetched = fetch().await?;
                }
                Err(RangeExtentCommitFailure::StaleTargetValidator) => {
                    return Err(identity_error_at(
                        "extent-publish-repeat-target-validator-change",
                    ));
                }
                Err(RangeExtentCommitFailure::Rejected(error)) => return Err(error),
            }
        }
    }

    async fn commit_range_extent_with_failure(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        range: Range<u64>,
        input: RangeExtentCommitInput<'_>,
    ) -> Result<u64, RangeExtentCommitFailure> {
        let RangeExtentCommitInput {
            expected,
            bytes,
            final_url,
            etag,
            last_modified,
        } = input;
        if bytes.len() as u64 != range.end.saturating_sub(range.start) {
            return Err(HlsRangeError::InvalidResponse("range length mismatch".to_owned()).into());
        }
        let key = self.range_resource_key(session_id, resource)?;
        let publication_lock = self.range_publication_lock(&key);
        let _publication = publication_lock.lock().await;
        let origin = media_url_origin(final_url)
            .ok_or_else(|| HlsRangeError::InvalidResponse("invalid CDN origin".to_owned()))?;
        let loaded = self
            .load_range_manifest(session_id, resource)?
            .ok_or(HlsRangeError::IdentityChanged)?;
        if !range_manifest_rebase_compatible_for_extent_publication(
            expected,
            &loaded.manifest,
            &origin,
        ) {
            if range_manifest_target_validator_snapshot_is_stale(
                expected,
                &loaded.manifest,
                &origin,
            ) {
                return Err(RangeExtentCommitFailure::StaleTargetValidator);
            }
            return Err(identity_error_at(
                range_manifest_extent_rebase_failure_stage(expected, &loaded.manifest, &origin)
                    .unwrap_or("extent-publish-rebase-unclassified"),
            )
            .into());
        }
        if let Some(binding) = loaded
            .manifest
            .validated_origins
            .iter()
            .find(|binding| binding.origin == origin)
            && binding.strong_etag.as_ref() != etag.as_ref()
        {
            return Err(identity_error_at("extent-origin-validator-change").into());
        }
        if loaded
            .manifest
            .extents
            .iter()
            .any(|extent| extent.start < range.end && range.start < extent.end)
        {
            let exact = loaded.manifest.extents.iter().any(|extent| {
                extent.start == range.start
                    && extent.end == range.end
                    && extent.sha256 == sha256_hex(bytes)
            });
            if exact {
                return Ok(loaded.manifest.durable_bytes);
            }
            return Err(identity_error_at("extent-overlap-content").into());
        }
        let data_path = self.data_path_for_manifest(session_id, resource, &loaded.manifest)?;
        self.reject_cache_path_symlink(&data_path)?;
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true);
        let mut data = open_range_file(&mut options, &data_path)?;
        if file_object_identity(&data.metadata()?)
            != loaded.manifest.data_identity.clone().unwrap_or_default()
        {
            return Err(identity_error_at("extent-data-object").into());
        }
        data.seek(SeekFrom::Start(range.start))?;
        data.write_all(bytes)?;
        data.sync_all()?;

        let prefix_length = if range.start == 0 {
            bytes.len().min(RANGE_STARTUP_CHUNK_BYTES as usize) as u64
        } else {
            loaded.manifest.prefix_length
        };
        let prefix_sha256 = if range.start == 0 {
            Some(sha256_hex(&bytes[..prefix_length as usize]))
        } else {
            loaded.manifest.prefix_sha256.clone()
        };
        let mut manifest = loaded.manifest.clone();
        manifest.generation = manifest
            .generation
            .checked_add(1)
            .ok_or(HlsRangeError::IdentityChanged)?;
        manifest
            .extents
            .retain(|extent| extent.end <= range.start || extent.start >= range.end);
        manifest.extents.push(PersistedRangeExtent {
            start: range.start,
            end: range.end,
            sha256: sha256_hex(bytes),
        });
        manifest.extents.sort_by_key(|extent| extent.start);
        manifest.durable_bytes = durable_extent_bytes(&manifest.extents);
        if range.start == 0 {
            manifest.prefix_length = prefix_length;
            manifest.prefix_sha256 = prefix_sha256.clone();
            manifest.strong_etag = etag.clone();
            manifest.validator_origin = Some(origin.clone());
            manifest.last_modified = last_modified.clone();
            if let Some(prefix_sha256) = prefix_sha256 {
                manifest.validated_origins.push(PersistedRangeOrigin {
                    origin,
                    prefix_sha256,
                    strong_etag: etag.clone(),
                });
                manifest.validated_origins.truncate(64);
            }
        }
        validate_range_manifest(&manifest, resource)?;
        let manifest_path = self.resource_range_manifest_path(session_id, &resource.id)?;
        let latest = self
            .load_range_manifest(session_id, resource)?
            .ok_or(HlsRangeError::IdentityChanged)?;
        if latest.file_identity != loaded.file_identity
            || latest.bytes_digest != loaded.bytes_digest
            || !range_manifest_rebase_compatible(&loaded.manifest, &latest.manifest)
        {
            return Err(identity_error_at("extent-checkpoint-content").into());
        }
        self.write_json_atomically(&manifest_path, &manifest)?;
        sync_directory(manifest_path.parent().ok_or_else(|| {
            HlsRangeError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid cache path",
            ))
        })?)?;
        Ok(manifest.durable_bytes)
    }

    // This compatibility path keeps borrowed control and progress callbacks scoped to one fill.
    #[allow(clippy::too_many_arguments)]
    async fn fill_full_get_compat<F, P>(
        &self,
        client: &reqwest::Client,
        session_id: &str,
        resource: &HlsMediaResource,
        key: &RangeResourceKey,
        priority: HlsRangePriority,
        control: &F,
        progress: &P,
    ) -> Result<HlsRangeResourceStatus, HlsRangeError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
        P: Fn(u64) + Send + Sync,
    {
        if priority != HlsRangePriority::Background {
            return Err(HlsRangeError::RangeUnsupported);
        }
        let loaded = self.ensure_range_manifest(session_id, resource, key)?;
        if !loaded.manifest.extents.is_empty() {
            return Err(HlsRangeError::RangeUnsupported);
        }
        let control_key = RangeChunkKey {
            resource: key.clone(),
            total_length: 1,
            range: 0..1,
        };
        let candidates = self
            .cdn_history
            .rank_request(&resource.request)
            .into_iter()
            .filter(|url| !url.trim().is_empty())
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Err(HlsRangeError::RangeUnsupported);
        }
        let temp_path = self.resource_range_full_get_temp_path(session_id, &resource.id)?;
        let mut last_error = None;
        'candidate: for url in candidates {
            self.range_cache
                .check_priority_control(&control_key, priority, control)?;
            let started = Instant::now();
            let mut request = client.get(&url);
            for header in &resource.request.headers {
                if header.name.eq_ignore_ascii_case("range")
                    || header.name.eq_ignore_ascii_case("if-range")
                    || !should_forward_media_request_header(
                        &header.name,
                        &resource.request.url,
                        &url,
                    )
                {
                    continue;
                }
                request = request.header(header.name.as_str(), header.value.as_str());
            }
            let send = request.send();
            tokio::pin!(send);
            let send_result = loop {
                self.range_cache
                    .check_priority_control(&control_key, priority, control)?;
                tokio::select! {
                    response = &mut send => break response,
                    () = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            };
            let response = match send_result {
                Ok(response) => response,
                Err(error) => {
                    let error = HlsRangeError::from(error);
                    self.record_cdn_observation(
                        resource,
                        &url,
                        passive_cache_observation(
                            range_outcome_for_error(&error),
                            0,
                            Some(started.elapsed()),
                            None,
                            None,
                        ),
                    );
                    last_error = Some(error);
                    continue;
                }
            };
            let status = response.status();
            let final_url = response.url().as_str().to_owned();
            if status != StatusCode::OK {
                if status == StatusCode::PARTIAL_CONTENT {
                    return Err(HlsRangeError::InvalidResponse(
                        "full-resource fallback returned partial content".to_owned(),
                    ));
                }
                let error = HlsRangeError::UpstreamStatus(status);
                self.record_cdn_observation(
                    resource,
                    &final_url,
                    passive_cache_observation(
                        cache_outcome_for_status(status),
                        0,
                        Some(started.elapsed()),
                        None,
                        None,
                    ),
                );
                if is_retryable_range_candidate_error(&error) {
                    last_error = Some(error);
                    continue;
                }
                return Err(error);
            }

            let first_byte_latency = started.elapsed();
            let declared_length = response.content_length();
            if let (Some(expected), Some(declared)) = (resource.request.size, declared_length)
                && expected != declared
            {
                self.record_cdn_observation(
                    resource,
                    &final_url,
                    passive_cache_observation(
                        CdnObservationOutcome::IntegrityMismatch,
                        declared,
                        Some(first_byte_latency),
                        Some(first_byte_latency),
                        Some(false),
                    ),
                );
                last_error = Some(HlsRangeError::InvalidResponse(
                    "full-resource length did not match metadata".to_owned(),
                ));
                continue;
            }
            let Some(expected_length) = resource.request.size.or(declared_length) else {
                last_error = Some(HlsRangeError::InvalidResponse(
                    "full-resource length was unknown".to_owned(),
                ));
                continue;
            };
            if expected_length == 0 {
                last_error = Some(HlsRangeError::InvalidResponse(
                    "full-resource length was empty".to_owned(),
                ));
                continue;
            }

            let current_length = self.range_managed_size(session_id, &resource.id)?;
            let usage = self.managed_usage_size_bytes().map_err(HlsRangeError::Io)?;
            let scratch_target = current_length.saturating_add(expected_length);
            let _reservation =
                self.range_cache
                    .reserve(key.clone(), scratch_target, current_length, usage)?;

            self.reject_cache_path_symlink(&temp_path)?;
            let mut options = fs::OpenOptions::new();
            options.read(true).write(true).create(true).truncate(true);
            let mut file = open_range_file(&mut options, &temp_path)?;
            let mut stream = response.bytes_stream();
            let mut total = 0_u64;
            let mut candidate_error = None;
            loop {
                self.range_cache
                    .check_priority_control(&control_key, priority, control)?;
                let next = tokio::select! {
                    item = stream.next() => item,
                    () = tokio::time::sleep(Duration::from_millis(50)) => continue,
                };
                let Some(next) = next else { break };
                let chunk = match next {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        candidate_error = Some(HlsRangeError::from(error));
                        break;
                    }
                };
                total = total.saturating_add(chunk.len() as u64);
                if total > expected_length {
                    candidate_error = Some(HlsRangeError::InvalidResponse(
                        "full-resource body exceeded its declared length".to_owned(),
                    ));
                    break;
                }
                file.write_all(&chunk)?;
                self.range_cache
                    .note_file_length(key, current_length.saturating_add(total));
            }
            if candidate_error.is_none() && total != expected_length {
                candidate_error = Some(HlsRangeError::InvalidResponse(
                    "full-resource body length was incomplete".to_owned(),
                ));
            }
            if let Some(error) = candidate_error {
                file.set_len(0)?;
                file.sync_all()?;
                self.range_cache.note_file_length(key, 0);
                self.record_cdn_observation(
                    resource,
                    &final_url,
                    passive_cache_observation(
                        range_outcome_for_error(&error),
                        total,
                        Some(started.elapsed()),
                        Some(first_byte_latency),
                        Some(false),
                    ),
                );
                if matches!(error, HlsRangeError::Network(_))
                    || matches!(&error, HlsRangeError::InvalidResponse(_))
                {
                    last_error = Some(error);
                    continue 'candidate;
                }
                return Err(error);
            }
            file.sync_all()?;

            let initialization_length = match cached_mp4_initialization_length(&temp_path).await {
                Ok(length) if length > 0 && length < total => length,
                _ => {
                    file.set_len(0)?;
                    file.sync_all()?;
                    self.range_cache.note_file_length(key, 0);
                    self.record_cdn_observation(
                        resource,
                        &final_url,
                        passive_cache_observation(
                            CdnObservationOutcome::IntegrityMismatch,
                            total,
                            Some(started.elapsed()),
                            Some(first_byte_latency),
                            Some(false),
                        ),
                    );
                    last_error = Some(HlsRangeError::InvalidResponse(
                        "full-resource MP4 initialization was invalid".to_owned(),
                    ));
                    continue 'candidate;
                }
            };
            let segments = match mp4_fragment_ranges(&temp_path, initialization_length, total) {
                Ok(segments) => segments,
                Err(_) => {
                    file.set_len(0)?;
                    file.sync_all()?;
                    self.range_cache.note_file_length(key, 0);
                    self.record_cdn_observation(
                        resource,
                        &final_url,
                        passive_cache_observation(
                            CdnObservationOutcome::IntegrityMismatch,
                            total,
                            Some(started.elapsed()),
                            Some(first_byte_latency),
                            Some(false),
                        ),
                    );
                    last_error = Some(HlsRangeError::InvalidResponse(
                        "full-resource MP4 fragment layout was invalid".to_owned(),
                    ));
                    continue 'candidate;
                }
            };
            drop(file);

            let publication_lock = self.range_publication_lock(key);
            let _publication = publication_lock.lock().await;
            let Some(current) = self.load_range_manifest(session_id, resource)? else {
                self.remove_managed_cache_file_if_exists(&temp_path)?;
                return Err(HlsRangeError::IdentityChanged);
            };
            if !range_manifest_rebase_compatible(&loaded.manifest, &current.manifest) {
                self.remove_managed_cache_file_if_exists(&temp_path)?;
                return Err(HlsRangeError::IdentityChanged);
            }
            if !current.manifest.extents.is_empty() {
                self.remove_managed_cache_file_if_exists(&temp_path)?;
                return Err(HlsRangeError::RangeUnsupported);
            }
            let final_path = self.resource_path(session_id, &resource.id)?;
            if safe_file_identity(self, &final_path)?.is_some() {
                return Err(HlsRangeError::IdentityChanged);
            }
            let metadata_path = self.resource_metadata_path(session_id, &resource.id)?;
            if safe_file_identity(self, &metadata_path)?.is_some() {
                return Err(HlsRangeError::IdentityChanged);
            }
            self.reject_cache_path_symlink(&final_path)?;
            fs::rename(&temp_path, &final_path)?;
            sync_directory(final_path.parent().ok_or_else(|| {
                HlsRangeError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid cache path",
                ))
            })?)?;
            let metadata = PersistedHlsCachedResource {
                schema_version: HLS_CACHE_SCHEMA_VERSION,
                id: resource.id.clone(),
                content_type: resource.content_type().to_owned(),
                total_length: total,
                initialization_length,
                segments: hls_segments_from_mp4_ranges(segments)
                    .into_iter()
                    .map(PersistedHlsMediaSegment::from)
                    .collect(),
                cache_key: PersistedBilibiliMediaCacheKey::from(resource.request.cache_key.clone()),
            };
            self.write_json_atomically(&metadata_path, &metadata)?;
            sync_directory(metadata_path.parent().ok_or_else(|| {
                HlsRangeError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid cache path",
                ))
            })?)?;
            self.remove_managed_cache_file_if_exists(
                &self.resource_range_data_path(session_id, &resource.id)?,
            )?;
            self.remove_managed_cache_file_if_exists(
                &self.resource_range_manifest_path(session_id, &resource.id)?,
            )?;
            sync_directory(final_path.parent().ok_or_else(|| {
                HlsRangeError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid cache path",
                ))
            })?)?;
            self.record_cdn_observation(
                resource,
                &final_url,
                passive_cache_observation(
                    CdnObservationOutcome::Complete,
                    total,
                    Some(started.elapsed()),
                    Some(first_byte_latency),
                    Some(false),
                ),
            );
            progress(total);
            return Ok(HlsRangeResourceStatus::Complete {
                total_length: total,
            });
        }
        self.remove_managed_cache_file_if_exists(&temp_path)?;
        if let Some(parent) = temp_path.parent() {
            sync_directory(parent)?;
        }
        Err(last_error.unwrap_or(HlsRangeError::RangeUnsupported))
    }

    async fn finalize_range_resource(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        total_length: u64,
    ) -> Result<(), HlsRangeError> {
        let key = self.range_resource_key(session_id, resource)?;
        let publication_lock = self.range_publication_lock(&key);
        let _publication = publication_lock.lock().await;
        let Some(loaded) = self.load_range_manifest(session_id, resource)? else {
            if self
                .cached_resource(session_id, &resource.id)
                .is_some_and(|cached| cached.total_length == total_length)
            {
                return Ok(());
            }
            return Err(HlsRangeError::IdentityChanged);
        };
        if loaded.manifest.total_length != Some(total_length)
            || loaded.manifest.durable_bytes != total_length
            || !range_is_covered(&loaded.manifest.extents, 0..total_length)
        {
            return Err(identity_error_at("finalize-incomplete-extents"));
        }
        self.verify_range_extents_sync(session_id, resource, &loaded.manifest)?;
        let data_path = self.data_path_for_manifest(session_id, resource, &loaded.manifest)?;
        if fs::metadata(&data_path)?.len() != total_length {
            return Err(identity_error_at("finalize-data-length"));
        }
        let initialization_length =
            cached_mp4_initialization_length(&data_path)
                .await
                .map_err(|_| {
                    HlsRangeError::InvalidResponse("invalid cached MP4 initialization".to_owned())
                })?;
        if initialization_length == 0 || initialization_length >= total_length {
            return Err(HlsRangeError::InvalidResponse(
                "invalid cached MP4 initialization range".to_owned(),
            ));
        }
        let segments = mp4_fragment_ranges(&data_path, initialization_length, total_length)
            .map_err(|_| {
                HlsRangeError::InvalidResponse("invalid cached MP4 fragment layout".to_owned())
            })?;
        let final_path = self.resource_path(session_id, &resource.id)?;
        let expected_identity = loaded.manifest.data_identity.as_deref();
        match safe_file_identity(self, &final_path)? {
            Some(identity) if Some(identity.as_str()) == expected_identity => {}
            Some(_) => return Err(identity_error_at("finalize-target-object")),
            None if data_path != final_path => {
                self.reject_cache_path_symlink(&final_path)?;
                fs::rename(&data_path, &final_path)?;
                sync_directory(final_path.parent().ok_or_else(|| {
                    HlsRangeError::Io(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid cache path",
                    ))
                })?)?;
            }
            None => return Err(identity_error_at("finalize-renamed-object-missing")),
        }

        let metadata_path = self.resource_metadata_path(session_id, &resource.id)?;
        if safe_file_identity(self, &metadata_path)?.is_some() {
            let cached = self.cached_resource(session_id, &resource.id);
            if cached.is_none_or(|cached| cached.total_length != total_length) {
                return Err(identity_error_at("finalize-existing-metadata-binding"));
            }
        } else {
            let metadata = PersistedHlsCachedResource {
                schema_version: HLS_CACHE_SCHEMA_VERSION,
                id: resource.id.clone(),
                content_type: resource.content_type().to_owned(),
                total_length,
                initialization_length,
                segments: hls_segments_from_mp4_ranges(segments)
                    .into_iter()
                    .map(PersistedHlsMediaSegment::from)
                    .collect(),
                cache_key: PersistedBilibiliMediaCacheKey::from(resource.request.cache_key.clone()),
            };
            self.write_json_atomically(&metadata_path, &metadata)?;
            sync_directory(metadata_path.parent().ok_or_else(|| {
                HlsRangeError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid cache path",
                ))
            })?)?;
        }
        self.remove_managed_cache_file_if_exists(
            &self.resource_range_manifest_path(session_id, &resource.id)?,
        )?;
        sync_directory(final_path.parent().ok_or_else(|| {
            HlsRangeError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid cache path",
            ))
        })?)?;
        Ok(())
    }

    fn load_range_manifest(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
    ) -> Result<Option<LoadedRangeManifest>, HlsRangeError> {
        let path = self.resource_range_manifest_path(session_id, &resource.id)?;
        let path_identity = match safe_file_identity(self, &path)? {
            Some(identity) => identity,
            None => {
                let data_path = self.resource_range_data_path(session_id, &resource.id)?;
                if safe_file_identity(self, &data_path)?.is_some() {
                    return Err(identity_error_at("manifest-missing-data-present"));
                }
                return Ok(None);
            }
        };
        let mut options = fs::OpenOptions::new();
        options.read(true);
        let file = open_range_file(&mut options, &path)?;
        let opened_identity = file_object_identity(&file.metadata()?);
        if opened_identity != path_identity {
            return Err(identity_error_at("manifest-open-object"));
        }
        let length = file.metadata()?.len();
        if length > crate::hls_range_cache::RANGE_MAX_MANIFEST_BYTES {
            return Err(HlsRangeError::InvalidResponse(
                "HLS range manifest exceeds its size bound".to_owned(),
            ));
        }
        let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or_default());
        file.take(crate::hls_range_cache::RANGE_MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if file_object_identity(&fs::metadata(&path)?) != opened_identity {
            return Err(identity_error_at("manifest-read-object"));
        }
        let manifest = serde_json::from_slice::<PersistedRangeManifest>(&bytes)
            .map_err(|_| HlsRangeError::InvalidResponse("malformed range checkpoint".to_owned()))?;
        validate_range_manifest(&manifest, resource)?;
        let data_path = self.data_path_for_manifest(session_id, resource, &manifest)?;
        let data_identity = safe_file_identity(self, &data_path)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "checkpoint data is missing"))?;
        if manifest.data_identity.as_deref() != Some(data_identity.as_str()) {
            return Err(identity_error_at("manifest-data-object-binding"));
        }
        Ok(Some(LoadedRangeManifest {
            manifest,
            bytes_digest: sha256_hex(&bytes),
            file_identity: opened_identity,
        }))
    }

    async fn load_range_manifest_serialized(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
    ) -> Result<Option<LoadedRangeManifest>, HlsRangeError> {
        let key = self.range_resource_key(session_id, resource)?;
        let publication_lock = self.range_publication_lock(&key);
        let _publication = publication_lock.lock().await;
        self.load_range_manifest(session_id, resource)
    }

    fn verify_range_extents_sync(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        manifest: &PersistedRangeManifest,
    ) -> Result<(), HlsRangeError> {
        let path = self.data_path_for_manifest(session_id, resource, manifest)?;
        self.reject_cache_path_symlink(&path)?;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        let mut file = open_range_file(&mut options, &path)?;
        let identity = file_object_identity(&file.metadata()?);
        if manifest.data_identity.as_deref() != Some(identity.as_str()) {
            return Err(identity_error_at("extent-verify-object"));
        }
        let file_length = file.metadata()?.len();
        for extent in &manifest.extents {
            let length = extent.end - extent.start;
            if extent.end > file_length {
                return Err(HlsRangeError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "checkpointed HLS range extends past the data file",
                )));
            }
            let size = usize::try_from(length)
                .map_err(|_| HlsRangeError::InvalidResponse("range is too large".to_owned()))?;
            let mut bytes = vec![0; size];
            file.seek(SeekFrom::Start(extent.start))?;
            file.read_exact(&mut bytes)?;
            if sha256_hex(&bytes) != extent.sha256 {
                return Err(identity_error_at("extent-verify-hash"));
            }
        }
        if !self.range_data_object_is_still_bound(session_id, resource, &identity)? {
            return Err(HlsRangeError::IdentityChanged);
        }
        Ok(())
    }

    fn range_data_object_is_still_bound(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        identity: &str,
    ) -> Result<bool, HlsRangeError> {
        for path in [
            self.resource_range_data_path(session_id, &resource.id)?,
            self.resource_path(session_id, &resource.id)?,
        ] {
            if safe_file_identity(self, &path)?.as_deref() == Some(identity) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn read_durable_range(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        requested: Range<u64>,
    ) -> Result<Option<Vec<u8>>, HlsRangeError> {
        self.read_durable_range_with_hook(session_id, resource, requested, || {})
            .await
    }

    async fn read_durable_range_with_hook<F>(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        requested: Range<u64>,
        after_open: F,
    ) -> Result<Option<Vec<u8>>, HlsRangeError>
    where
        F: FnOnce(),
    {
        if requested.start >= requested.end {
            return Err(HlsRangeError::InvalidResponse(
                "requested HLS range is empty or reversed".to_owned(),
            ));
        }
        let key = self.range_resource_key(session_id, resource)?;
        let publication_lock = self.range_publication_lock(&key);
        let _publication = publication_lock.lock().await;
        if let Some(cached) = self.cached_resource(session_id, &resource.id) {
            if requested.end > cached.total_length {
                return Err(HlsRangeError::InvalidResponse(
                    "requested HLS range exceeds the cached resource".to_owned(),
                ));
            }
            let mut file = open_read_no_follow(
                self.root_path.as_ref(),
                &self.resource_relative_path(session_id, &resource.id)?,
            )?;
            let length = usize::try_from(requested.end - requested.start)
                .map_err(|_| HlsRangeError::InvalidResponse("range is too large".to_owned()))?;
            let mut bytes = vec![0; length];
            file.seek(SeekFrom::Start(requested.start))?;
            file.read_exact(&mut bytes)?;
            return Ok(Some(bytes));
        }
        let Some(loaded) = self.load_range_manifest(session_id, resource)? else {
            return Ok(None);
        };
        let Some(total_length) = loaded.manifest.total_length else {
            return Ok(None);
        };
        if requested.end > total_length {
            return Err(HlsRangeError::InvalidResponse(
                "requested HLS range exceeds the discovered resource".to_owned(),
            ));
        }
        if !range_is_covered(&loaded.manifest.extents, requested.clone()) {
            return Ok(None);
        }
        let path = self.data_path_for_manifest(session_id, resource, &loaded.manifest)?;
        self.reject_cache_path_symlink(&path)?;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        let mut file = open_range_file(&mut options, &path)?;
        let opened_metadata = file.metadata()?;
        let identity = file_object_identity(&opened_metadata);
        if loaded.manifest.data_identity.as_deref() != Some(identity.as_str()) {
            return Err(identity_error_at("range-read-object"));
        }
        let length = usize::try_from(requested.end - requested.start)
            .map_err(|_| HlsRangeError::InvalidResponse("range is too large".to_owned()))?;
        let initial_file_length = opened_metadata.len();
        after_open();
        let mut bytes = vec![0; length];
        for extent in loaded
            .manifest
            .extents
            .iter()
            .filter(|extent| extent.start < requested.end && requested.start < extent.end)
        {
            if extent.end > initial_file_length {
                return Err(HlsRangeError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "checkpointed HLS range extends past the data file",
                )));
            }
            let mut hasher = Sha256::new();
            let mut cursor = extent.start;
            let mut chunk = vec![0; RANGE_MAX_CHUNK_BYTES as usize];
            while cursor < extent.end {
                let count = usize::try_from((extent.end - cursor).min(chunk.len() as u64))
                    .map_err(|_| HlsRangeError::InvalidResponse("range is too large".to_owned()))?;
                file.seek(SeekFrom::Start(cursor))?;
                file.read_exact(&mut chunk[..count])?;
                let chunk_end = cursor + count as u64;
                hasher.update(&chunk[..count]);

                let copy_start = requested.start.max(cursor);
                let copy_end = requested.end.min(chunk_end);
                if copy_start < copy_end {
                    let source_start = usize::try_from(copy_start - cursor).map_err(|_| {
                        HlsRangeError::InvalidResponse("range is too large".to_owned())
                    })?;
                    let source_end = usize::try_from(copy_end - cursor).map_err(|_| {
                        HlsRangeError::InvalidResponse("range is too large".to_owned())
                    })?;
                    let target_start =
                        usize::try_from(copy_start - requested.start).map_err(|_| {
                            HlsRangeError::InvalidResponse("range is too large".to_owned())
                        })?;
                    let target_end = target_start + (source_end - source_start);
                    bytes[target_start..target_end]
                        .copy_from_slice(&chunk[source_start..source_end]);
                }
                cursor = chunk_end;
            }
            if format!("{:x}", hasher.finalize()) != extent.sha256 {
                return Err(identity_error_at("range-read-extent-hash"));
            }
        }
        let final_metadata = file.metadata()?;
        if file_object_identity(&final_metadata) != identity
            || final_metadata.len() < initial_file_length
            || final_metadata.len() < requested.end
        {
            return Err(identity_error_at("range-read-final-object-or-length"));
        }
        Ok(Some(bytes))
    }

    async fn fill_missing_ranges_with_progress<F, P>(
        &self,
        client: &reqwest::Client,
        session_id: &str,
        resource: &HlsMediaResource,
        priority: HlsRangePriority,
        control: &F,
        progress: P,
    ) -> Result<HlsRangeResourceStatus, HlsRangeError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
        P: Fn(u64) + Send + Sync,
    {
        let _active = self.range_cache.enter(session_id, priority)?;
        let key = self.range_resource_key(session_id, resource)?;
        let publication_lock = self.range_publication_lock(&key);
        let mut loaded = {
            let _publication = publication_lock.lock().await;
            self.ensure_range_manifest(session_id, resource, &key)?
        };
        self.verify_range_extents_sync(session_id, resource, &loaded.manifest)?;
        let total = match loaded.manifest.total_length.or(resource.request.size) {
            Some(total) => total,
            None => match self
                .discover_range_total(client, resource, &key, priority, control)
                .await
            {
                Ok(total) => total,
                Err(HlsRangeError::RangeUnsupported)
                    if priority == HlsRangePriority::Background =>
                {
                    return self
                        .fill_full_get_compat(
                            client, session_id, resource, &key, priority, control, &progress,
                        )
                        .await;
                }
                Err(error) => return Err(error),
            },
        };
        if total == 0 {
            return Err(HlsRangeError::InvalidResponse(
                "HLS resource has zero length".to_owned(),
            ));
        }

        if !range_size_is_resumable(total) {
            return self
                .fill_full_get_compat(
                    client, session_id, resource, &key, priority, control, &progress,
                )
                .await;
        }
        if loaded.manifest.total_length != Some(total) {
            loaded = self
                .publish_range_manifest(session_id, resource, loaded, |manifest| {
                    manifest.total_length = Some(total);
                })
                .await?;
        }

        if loaded.manifest.extents.is_empty() {
            let first = 0..total.min(RANGE_STARTUP_CHUNK_BYTES);
            match self
                .ensure_range_span(
                    client, session_id, resource, &key, first, 0, priority, control, &progress,
                )
                .await
            {
                Ok(()) => {}
                Err(HlsRangeError::RangeUnsupported) => {
                    return self
                        .fill_full_get_compat(
                            client, session_id, resource, &key, priority, control, &progress,
                        )
                        .await;
                }
                Err(error) => return Err(error),
            }
            loaded = self
                .load_range_manifest_serialized(session_id, resource)
                .await?
                .ok_or(HlsRangeError::IdentityChanged)?;
        }

        let planned = planned_range_chunks(total);
        let durable = loaded.manifest.extents.clone();
        let missing = planned
            .into_iter()
            .enumerate()
            .filter(|(_, chunk)| !range_is_covered(&durable, chunk.clone()))
            .collect::<Vec<_>>();
        let store = self;
        let key = Arc::new(key);
        let progress = Arc::new(progress);
        futures_util::stream::iter(missing)
            .map(|(index, chunk)| {
                let key = Arc::clone(&key);
                let progress = Arc::clone(&progress);
                async move {
                    store
                        .ensure_range_span(
                            client,
                            session_id,
                            resource,
                            &key,
                            chunk,
                            index as u64,
                            priority,
                            control,
                            progress.as_ref(),
                        )
                        .await
                }
            })
            .buffer_unordered(self.range_cache.parallelism())
            .try_collect::<Vec<()>>()
            .await?;

        loaded = self
            .load_range_manifest_serialized(session_id, resource)
            .await?
            .ok_or(HlsRangeError::IdentityChanged)?;
        let durable_bytes = loaded.manifest.durable_bytes;
        if range_is_covered(&loaded.manifest.extents, 0..total) {
            Ok(HlsRangeResourceStatus::Complete {
                total_length: total,
            })
        } else {
            Ok(HlsRangeResourceStatus::Partial {
                total_length: total,
                durable_bytes,
                missing_ranges: missing_ranges(&loaded.manifest.extents, total),
            })
        }
    }

    async fn ensure_range_inner<F>(
        &self,
        client: &reqwest::Client,
        session_id: &str,
        resource: &HlsMediaResource,
        requested: Range<u64>,
        priority: HlsRangePriority,
        control: &F,
    ) -> Result<HlsReadyRange, HlsRangeError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        let _active = self.range_cache.enter(session_id, priority)?;
        if requested.start >= requested.end {
            return Err(HlsRangeError::InvalidResponse(
                "requested HLS range is empty or reversed".to_owned(),
            ));
        }
        if let Some(ready) = self.completed_range_ready(session_id, resource, requested.clone())? {
            return Ok(ready);
        }
        let mut loaded = match self
            .ensure_range_state_after_initial_miss(session_id, resource, requested.clone())
            .await?
        {
            RangeEnsureState::Completed(ready) => return Ok(ready),
            RangeEnsureState::Partial(loaded) => *loaded,
        };
        let key = self.range_resource_key(session_id, resource)?;
        let mut total = loaded.manifest.total_length.or(resource.request.size);
        if total.is_none() {
            total = Some(
                self.discover_range_total(client, resource, &key, priority, control)
                    .await?,
            );
        }
        let total = total.ok_or(HlsRangeError::RangeUnsupported)?;
        if !range_size_is_resumable(total) {
            return Err(HlsRangeError::RangeUnsupported);
        }
        if requested.end > total {
            return Err(HlsRangeError::InvalidResponse(
                "requested HLS range exceeds the discovered resource".to_owned(),
            ));
        }
        if loaded.manifest.total_length != Some(total) {
            loaded = self
                .publish_range_manifest(session_id, resource, loaded, |manifest| {
                    manifest.total_length = Some(total);
                })
                .await?;
        }
        if loaded.manifest.prefix_sha256.is_none() {
            let result = self
                .ensure_range_span(
                    client,
                    session_id,
                    resource,
                    &key,
                    0..total.min(RANGE_STARTUP_CHUNK_BYTES),
                    0,
                    priority,
                    control,
                    &|_| {},
                )
                .await;
            if let Err(error) = result {
                return self
                    .completed_range_after_identity_error(session_id, resource, requested, error);
            }
        }
        let chunks = planned_range_chunks(total)
            .into_iter()
            .enumerate()
            .filter(|(_, chunk)| {
                chunk.start < requested.end
                    && requested.start < chunk.end
                    && !range_is_covered(&loaded.manifest.extents, chunk.clone())
            })
            .collect::<Vec<_>>();
        let result = futures_util::stream::iter(chunks)
            .map(|(index, chunk)| {
                let key = key.clone();
                async move {
                    self.ensure_range_span(
                        client,
                        session_id,
                        resource,
                        &key,
                        chunk,
                        index as u64,
                        priority,
                        control,
                        &|_| {},
                    )
                    .await
                }
            })
            .buffer_unordered(self.range_cache.parallelism())
            .try_collect::<Vec<()>>()
            .await;
        if let Err(error) = result {
            return self
                .completed_range_after_identity_error(session_id, resource, requested, error);
        }
        if let Some(ready) = self.completed_range_ready(session_id, resource, requested.clone())? {
            return Ok(ready);
        }
        let loaded = self
            .load_range_manifest_serialized(session_id, resource)
            .await?
            .ok_or_else(|| identity_error_at("ensure-checkpoint-missing"))?;
        let strong_etag = loaded.manifest.strong_etag.clone();
        Ok(HlsReadyRange {
            requested,
            total_length: total,
            strong_etag,
        })
    }

    async fn ensure_range_state_after_initial_miss(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        requested: Range<u64>,
    ) -> Result<RangeEnsureState, HlsRangeError> {
        let key = self.range_resource_key(session_id, resource)?;
        let publication_lock = self.range_publication_lock(&key);
        let _publication = publication_lock.lock().await;
        if let Some(ready) = self.completed_range_ready(session_id, resource, requested)? {
            return Ok(RangeEnsureState::Completed(ready));
        }
        self.ensure_range_manifest(session_id, resource, &key)
            .map(Box::new)
            .map(RangeEnsureState::Partial)
    }

    fn completed_range_ready(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        requested: Range<u64>,
    ) -> Result<Option<HlsReadyRange>, HlsRangeError> {
        let metadata_path = self.resource_metadata_path(session_id, &resource.id)?;
        let Some(metadata_identity) = safe_file_identity(self, &metadata_path)? else {
            let final_path = self.resource_path(session_id, &resource.id)?;
            let manifest_path = self.resource_range_manifest_path(session_id, &resource.id)?;
            if safe_file_identity(self, &final_path)?.is_some()
                && safe_file_identity(self, &manifest_path)?.is_none()
            {
                return Err(identity_error_at(
                    "completed-file-without-metadata-or-checkpoint",
                ));
            }
            return Ok(None);
        };

        let metadata = self
            .read_resource_metadata(session_id, &resource.id)
            .ok_or_else(|| identity_error_at("completed-metadata-unreadable"))?;
        let expected_cache_key =
            PersistedBilibiliMediaCacheKey::from(resource.request.cache_key.clone());
        if metadata.cache_key != expected_cache_key {
            return Err(identity_error_at("completed-cache-key"));
        }
        let cached = self
            .cached_resource(session_id, &resource.id)
            .ok_or(HlsRangeError::IdentityChanged)?;
        let final_identity = safe_file_identity(self, &cached.path)?
            .ok_or_else(|| identity_error_at("completed-resource-object-missing"))?;
        if cached.total_length != metadata.total_length
            || safe_file_identity(self, &metadata_path)?.as_deref()
                != Some(metadata_identity.as_str())
            || safe_file_identity(self, &cached.path)?.as_deref() != Some(final_identity.as_str())
        {
            return Err(identity_error_at("completed-resource-object-or-length"));
        }
        if requested.end > cached.total_length {
            return Err(HlsRangeError::InvalidResponse(
                "requested HLS range exceeds the cached resource".to_owned(),
            ));
        }
        Ok(Some(HlsReadyRange {
            requested,
            total_length: cached.total_length,
            strong_etag: None,
        }))
    }

    fn completed_range_after_identity_error(
        &self,
        session_id: &str,
        resource: &HlsMediaResource,
        requested: Range<u64>,
        error: HlsRangeError,
    ) -> Result<HlsReadyRange, HlsRangeError> {
        if matches!(error, HlsRangeError::IdentityChanged)
            && let Some(ready) = self.completed_range_ready(session_id, resource, requested)?
        {
            return Ok(ready);
        }
        Err(error)
    }

    async fn discover_range_total<F>(
        &self,
        client: &reqwest::Client,
        resource: &HlsMediaResource,
        key: &RangeResourceKey,
        priority: HlsRangePriority,
        control: &F,
    ) -> Result<u64, HlsRangeError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        let total_hint = 1;
        let usage = self.managed_usage_size_bytes().map_err(HlsRangeError::Io)?;
        let _reservation = self
            .range_cache
            .reserve(key.clone(), total_hint, 0, usage)?;
        let control_key = RangeChunkKey {
            resource: key.clone(),
            total_length: total_hint,
            range: 0..1,
        };
        self.range_cache
            .check_priority_control(&control_key, priority, control)?;
        let mut last_error = None;
        let mut range_unsupported_seen = false;
        for url in self
            .cdn_history
            .rank_request(&resource.request)
            .into_iter()
            .filter(|url| !url.trim().is_empty())
        {
            self.range_cache
                .check_priority_control(&control_key, priority, control)?;
            let response = match self
                .send_range_request(
                    client,
                    resource,
                    &url,
                    0..1,
                    &control_key,
                    priority,
                    control,
                )
                .await
            {
                Ok(response) => response,
                Err(error) if is_retryable_range_candidate_error(&error) => {
                    self.record_cdn_observation(
                        resource,
                        &url,
                        passive_cache_observation(
                            range_outcome_for_error(&error),
                            0,
                            None,
                            None,
                            None,
                        ),
                    );
                    last_error = Some(error);
                    continue;
                }
                Err(error) => return Err(error),
            };
            let status = response.status();
            let final_url = response.url().as_str().to_owned();
            if status == StatusCode::OK {
                self.record_cdn_observation(
                    resource,
                    &final_url,
                    passive_cache_observation(
                        CdnObservationOutcome::Partial,
                        0,
                        None,
                        None,
                        Some(false),
                    ),
                );
                range_unsupported_seen = true;
                continue;
            }
            if status != StatusCode::PARTIAL_CONTENT {
                self.record_cdn_observation(
                    resource,
                    &final_url,
                    passive_cache_observation(
                        cache_outcome_for_status(status),
                        0,
                        None,
                        None,
                        None,
                    ),
                );
                let error = HlsRangeError::UpstreamStatus(status);
                if is_retryable_range_candidate_error(&error) {
                    last_error = Some(error);
                    continue;
                }
                return Err(error);
            }
            let headers = response.headers().clone();
            let Some((start, end, total)) = parse_content_range_header(&headers) else {
                self.record_cdn_observation(
                    resource,
                    &final_url,
                    passive_cache_observation(
                        CdnObservationOutcome::IntegrityMismatch,
                        0,
                        None,
                        None,
                        Some(false),
                    ),
                );
                return Err(HlsRangeError::InvalidResponse(
                    "invalid upstream Content-Range".to_owned(),
                ));
            };
            if start != 0 || end != 0 || total == 0 {
                self.record_cdn_observation(
                    resource,
                    &final_url,
                    passive_cache_observation(
                        CdnObservationOutcome::IntegrityMismatch,
                        0,
                        None,
                        None,
                        Some(false),
                    ),
                );
                return Err(HlsRangeError::InvalidResponse(
                    "invalid upstream range discovery response".to_owned(),
                ));
            }
            if !range_size_is_resumable(total) {
                self.record_cdn_observation(
                    resource,
                    &final_url,
                    passive_cache_observation(
                        CdnObservationOutcome::Partial,
                        1,
                        None,
                        None,
                        Some(true),
                    ),
                );
                return Err(HlsRangeError::RangeUnsupported);
            }
            let body = match self
                .read_range_body(response, 1, &control_key, priority, control)
                .await
            {
                Ok(body) => body,
                Err(error) if is_retryable_range_candidate_error(&error) => {
                    self.record_cdn_observation(
                        resource,
                        &final_url,
                        passive_cache_observation(
                            range_outcome_for_error(&error),
                            0,
                            None,
                            None,
                            None,
                        ),
                    );
                    last_error = Some(error);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if body.len() != 1 {
                self.record_cdn_observation(
                    resource,
                    &final_url,
                    passive_cache_observation(
                        CdnObservationOutcome::IntegrityMismatch,
                        body.len() as u64,
                        None,
                        None,
                        Some(false),
                    ),
                );
                return Err(HlsRangeError::InvalidResponse(
                    "upstream range discovery body length mismatch".to_owned(),
                ));
            }
            self.record_cdn_observation(
                resource,
                &final_url,
                passive_cache_observation(
                    CdnObservationOutcome::Partial,
                    1,
                    None,
                    None,
                    Some(true),
                ),
            );
            return Ok(total);
        }
        Err(if range_unsupported_seen {
            HlsRangeError::RangeUnsupported
        } else {
            last_error.unwrap_or(HlsRangeError::RangeUnsupported)
        })
    }

    // Span scheduling needs explicit chunk identity, priority, live control, and progress inputs.
    #[allow(clippy::too_many_arguments)]
    async fn ensure_range_span<F, P>(
        &self,
        client: &reqwest::Client,
        session_id: &str,
        resource: &HlsMediaResource,
        resource_key: &RangeResourceKey,
        range: Range<u64>,
        chunk_index: u64,
        priority: HlsRangePriority,
        control: &F,
        progress: &P,
    ) -> Result<(), HlsRangeError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
        P: Fn(u64) + Send + Sync,
    {
        let loaded = self
            .load_range_manifest_serialized(session_id, resource)
            .await?
            .ok_or(HlsRangeError::IdentityChanged)?;
        let total = loaded
            .manifest
            .total_length
            .ok_or(HlsRangeError::RangeUnsupported)?;
        if range.start >= range.end || range.end > total {
            return Err(HlsRangeError::InvalidResponse(
                "invalid requested HLS chunk bounds".to_owned(),
            ));
        }
        let chunk_key = RangeChunkKey {
            resource: resource_key.clone(),
            total_length: total,
            range: range.clone(),
        };
        let usage = self.managed_usage_size_bytes().map_err(HlsRangeError::Io)?;
        let current_length = self
            .range_managed_size(session_id, &resource.id)
            .map_err(HlsRangeError::Io)?;
        let _reservation =
            self.range_cache
                .reserve(resource_key.clone(), range.end, current_length, usage)?;
        loop {
            self.range_cache
                .check_priority_control(&chunk_key, priority, control)?;
            if range_is_covered(&loaded.manifest.extents, range.clone())
                && self
                    .read_durable_range(session_id, resource, range.clone())
                    .await?
                    .is_some()
            {
                return Ok(());
            }
            let flight = self.range_cache.claim_chunk(chunk_key.clone(), priority)?;
            if flight.is_owner() {
                let _permit = self
                    .range_cache
                    .acquire_chunk_permit(&chunk_key, priority, control)
                    .await?;
                if self
                    .read_durable_range(session_id, resource, range.clone())
                    .await?
                    .is_some()
                {
                    drop(flight);
                    return Ok(());
                }
                let (durable_bytes, (bytes, final_url, _, _, elapsed, _)) = self
                    .fetch_and_commit_range_extent(session_id, resource, range.clone(), || {
                        self.fetch_range_chunk(
                            client,
                            session_id,
                            resource,
                            &chunk_key,
                            chunk_index,
                            priority,
                            control,
                        )
                    })
                    .await?;
                let file_length = self
                    .range_managed_size(session_id, &resource.id)
                    .map_err(HlsRangeError::Io)?;
                self.range_cache.note_file_length(resource_key, file_length);
                drop(flight);
                progress(durable_bytes);
                self.record_cdn_observation(
                    resource,
                    &final_url,
                    passive_cache_observation(
                        CdnObservationOutcome::Partial,
                        bytes.len() as u64,
                        Some(elapsed),
                        Some(elapsed),
                        Some(true),
                    ),
                );
                return Ok(());
            }
            tokio::select! {
                () = flight.notified() => {}
                () = tokio::time::sleep(Duration::from_millis(50)) => {
                    self.range_cache.check_priority_control(&chunk_key, priority, control)?;
                }
            }
            drop(flight);
        }
    }

    // Request control stays borrowed and is polled alongside the in-flight send future.
    #[allow(clippy::too_many_arguments)]
    async fn send_range_request<F>(
        &self,
        client: &reqwest::Client,
        resource: &HlsMediaResource,
        url: &str,
        range: Range<u64>,
        key: &RangeChunkKey,
        priority: HlsRangePriority,
        control: &F,
    ) -> Result<reqwest::Response, HlsRangeError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        let end = range.end.checked_sub(1).ok_or_else(|| {
            HlsRangeError::InvalidResponse("empty upstream byte range".to_owned())
        })?;
        let mut request = client.get(url).header(
            reqwest::header::RANGE,
            format!("bytes={}-{}", range.start, end),
        );
        for header in &resource.request.headers {
            if header.name.eq_ignore_ascii_case("range")
                || header.name.eq_ignore_ascii_case("if-range")
                || !should_forward_media_request_header(&header.name, &resource.request.url, url)
            {
                continue;
            }
            request = request.header(header.name.as_str(), header.value.as_str());
        }
        let send = request.send();
        tokio::pin!(send);
        loop {
            self.range_cache
                .check_priority_control(key, priority, control)?;
            tokio::select! {
                response = &mut send => return response.map_err(|error| HlsRangeError::Network(error.without_url())),
                () = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
    }

    async fn read_range_body<F>(
        &self,
        response: reqwest::Response,
        expected_length: u64,
        key: &RangeChunkKey,
        priority: HlsRangePriority,
        control: &F,
    ) -> Result<Vec<u8>, HlsRangeError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        if response
            .content_length()
            .is_some_and(|length| length != expected_length)
        {
            return Err(HlsRangeError::InvalidResponse(
                "upstream range Content-Length mismatch".to_owned(),
            ));
        }
        let capacity = usize::try_from(expected_length).map_err(|_| {
            HlsRangeError::InvalidResponse("upstream range is too large".to_owned())
        })?;
        let mut bytes = Vec::with_capacity(capacity);
        let mut stream = response.bytes_stream();
        loop {
            self.range_cache
                .check_priority_control(key, priority, control)?;
            let next = tokio::select! {
                next = stream.next() => next,
                () = tokio::time::sleep(Duration::from_millis(50)) => continue,
            };
            let Some(next) = next else { break };
            let chunk = next.map_err(|error| HlsRangeError::Network(error.without_url()))?;
            if bytes.len().saturating_add(chunk.len()) > capacity {
                return Err(HlsRangeError::InvalidResponse(
                    "upstream range body exceeds requested length".to_owned(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() != capacity {
            return Err(HlsRangeError::InvalidResponse(
                "upstream range body is shorter than requested".to_owned(),
            ));
        }
        Ok(bytes)
    }

    // Candidate ranking depends on both chunk index and key while control remains caller-owned.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_range_chunk<F>(
        &self,
        client: &reqwest::Client,
        session_id: &str,
        resource: &HlsMediaResource,
        chunk_key: &RangeChunkKey,
        chunk_index: u64,
        priority: HlsRangePriority,
        control: &F,
    ) -> Result<RangeChunkFetch, HlsRangeError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        let candidates = self
            .cdn_history
            .rank_request_for_range(&resource.request, chunk_index);
        let mut last_error = None;
        let mut range_unsupported_seen = false;
        for url in candidates.into_iter().filter(|url| !url.trim().is_empty()) {
            self.range_cache
                .check_priority_control(chunk_key, priority, control)?;
            let origin = media_url_origin(&url).ok_or_else(|| {
                HlsRangeError::InvalidResponse("invalid CDN candidate origin".to_owned())
            })?;
            let Some(mut loaded) = self
                .load_range_manifest_serialized(session_id, resource)
                .await?
            else {
                return Err(identity_error_at("chunk-checkpoint-missing"));
            };
            let existing_origin = loaded
                .manifest
                .validated_origins
                .iter()
                .find(|entry| entry.origin == origin)
                .cloned();
            if loaded.manifest.prefix_sha256.is_some() && existing_origin.is_none() {
                let (validated_origin, candidate_etag) = match self
                    .verify_candidate_prefix(
                        client,
                        session_id,
                        resource,
                        &url,
                        None,
                        chunk_key,
                        &loaded.manifest,
                        priority,
                        control,
                    )
                    .await
                {
                    Ok(binding) => binding,
                    Err(CandidateVerificationError::Fallback(HlsRangeError::RangeUnsupported)) => {
                        range_unsupported_seen = true;
                        last_error = Some(HlsRangeError::RangeUnsupported);
                        continue;
                    }
                    Err(CandidateVerificationError::Fallback(error)) => {
                        self.record_cdn_observation(
                            resource,
                            &url,
                            passive_cache_observation(
                                range_outcome_for_error(&error),
                                0,
                                None,
                                None,
                                Some(false),
                            ),
                        );
                        if matches!(error, HlsRangeError::RangeUnsupported) {
                            range_unsupported_seen = true;
                        }
                        last_error = Some(error);
                        continue;
                    }
                    Err(CandidateVerificationError::Mismatch(error)) => {
                        last_error = Some(error);
                        continue;
                    }
                    Err(CandidateVerificationError::Abort(error)) => {
                        return Err(error);
                    }
                };
                loaded = self
                    .publish_validated_origin(
                        session_id,
                        resource,
                        loaded,
                        validated_origin,
                        candidate_etag,
                    )
                    .await?;
            }

            let started = Instant::now();
            let response = match self
                .send_range_request(
                    client,
                    resource,
                    &url,
                    chunk_key.range.clone(),
                    chunk_key,
                    priority,
                    control,
                )
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    if !is_retryable_range_candidate_error(&error) {
                        return Err(error);
                    }
                    self.record_cdn_observation(
                        resource,
                        &url,
                        passive_cache_observation(
                            range_outcome_for_error(&error),
                            0,
                            None,
                            None,
                            None,
                        ),
                    );
                    last_error = Some(error);
                    continue;
                }
            };
            let final_url = response.url().as_str().to_owned();
            if response.status() == StatusCode::OK {
                self.record_cdn_observation(
                    resource,
                    &final_url,
                    passive_cache_observation(
                        CdnObservationOutcome::Partial,
                        0,
                        None,
                        None,
                        Some(false),
                    ),
                );
                last_error = Some(HlsRangeError::RangeUnsupported);
                range_unsupported_seen = true;
                continue;
            }
            if response.status() != StatusCode::PARTIAL_CONTENT {
                let status = response.status();
                self.record_cdn_observation(
                    resource,
                    &final_url,
                    passive_cache_observation(
                        cache_outcome_for_status(status),
                        0,
                        None,
                        None,
                        None,
                    ),
                );
                let error = HlsRangeError::UpstreamStatus(status);
                if is_retryable_range_candidate_error(&error) {
                    last_error = Some(error);
                    continue;
                }
                return Err(error);
            }
            let headers = response.headers().clone();
            let expected_start = chunk_key.range.start;
            let expected_end = chunk_key.range.end - 1;
            let parsed = parse_content_range_header(&headers);
            if parsed != Some((expected_start, expected_end, chunk_key.total_length)) {
                self.record_cdn_observation(
                    resource,
                    &final_url,
                    passive_cache_observation(
                        CdnObservationOutcome::IntegrityMismatch,
                        0,
                        None,
                        None,
                        Some(false),
                    ),
                );
                return Err(HlsRangeError::InvalidResponse(
                    "upstream Content-Range does not match the request".to_owned(),
                ));
            }
            let length = chunk_key.range.end - chunk_key.range.start;
            let body = match self
                .read_range_body(response, length, chunk_key, priority, control)
                .await
            {
                Ok(body) => body,
                Err(error) => {
                    if !is_retryable_range_candidate_error(&error) {
                        return Err(error);
                    }
                    self.record_cdn_observation(
                        resource,
                        &final_url,
                        passive_cache_observation(
                            range_outcome_for_error(&error),
                            0,
                            Some(started.elapsed()),
                            None,
                            Some(false),
                        ),
                    );
                    last_error = Some(error);
                    continue;
                }
            };
            let response_etag = strong_etag(&headers);
            let observed_origin = media_url_origin(&final_url)
                .ok_or_else(|| HlsRangeError::InvalidResponse("invalid CDN origin".to_owned()))?;
            let observed_binding = loaded
                .manifest
                .validated_origins
                .iter()
                .find(|binding| binding.origin == observed_origin);
            let baseline_etag = observed_binding.and_then(|binding| binding.strong_etag.clone());
            let validator_changed =
                observed_binding.is_some_and(|binding| binding.strong_etag != response_etag);
            let origin_unvalidated = observed_binding.is_none();
            let must_revalidate = loaded.manifest.prefix_sha256.is_some()
                && (origin_unvalidated || validator_changed);
            if validator_changed {
                let baseline_etag = baseline_etag.as_deref().unwrap_or_default();
                if ETAG_MISMATCH_DIAGNOSTIC_USED
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    let mut probe = self
                        .diagnose_etag_mismatch_prefix(
                            client,
                            resource,
                            &url,
                            &observed_origin,
                            chunk_key,
                            &loaded.manifest,
                            baseline_etag,
                            priority,
                            control,
                        )
                        .await;
                    probe.response_etag_syntax_valid = etag_header_syntax_valid(&headers);
                    eprintln!(
                        "HLS range ETag mismatch probe: same_origin={} total_matches={} prefix_hash_matches={} baseline_etag_matches={} response_etag_syntax_valid={} prefix_etag_syntax_valid={}",
                        probe.same_origin,
                        probe.total_matches,
                        probe.prefix_hash_matches,
                        probe.baseline_etag_matches,
                        probe.response_etag_syntax_valid,
                        probe.prefix_etag_syntax_valid,
                    );
                }
            }
            if must_revalidate {
                match self
                    .verify_candidate_prefix(
                        client,
                        session_id,
                        resource,
                        &url,
                        Some(&observed_origin),
                        chunk_key,
                        &loaded.manifest,
                        priority,
                        control,
                    )
                    .await
                {
                    Ok((verified_origin, _prefix_etag)) if verified_origin == observed_origin => {
                        loaded = self
                            .publish_validated_origin(
                                session_id,
                                resource,
                                loaded,
                                observed_origin.clone(),
                                response_etag.clone(),
                            )
                            .await?;
                    }
                    Ok((_, _)) => {
                        last_error = Some(HlsRangeError::InvalidResponse(
                            "candidate origin changed during content revalidation".to_owned(),
                        ));
                        continue;
                    }
                    Err(CandidateVerificationError::Fallback(error)) => {
                        self.record_cdn_observation(
                            resource,
                            &final_url,
                            passive_cache_observation(
                                range_outcome_for_error(&error),
                                length,
                                Some(started.elapsed()),
                                None,
                                Some(false),
                            ),
                        );
                        if matches!(error, HlsRangeError::RangeUnsupported) {
                            range_unsupported_seen = true;
                        }
                        last_error = Some(error);
                        continue;
                    }
                    Err(CandidateVerificationError::Mismatch(error)) => {
                        last_error = Some(error);
                        continue;
                    }
                    Err(CandidateVerificationError::Abort(error)) => {
                        return Err(error);
                    }
                }
            }
            let last_modified = headers
                .get(reqwest::header::LAST_MODIFIED)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let etag = response_etag;
            let mut modified = last_modified;
            if loaded.manifest.prefix_sha256.is_none()
                && chunk_key.range.start == 0
                && modified.is_none()
            {
                modified = loaded.manifest.last_modified.clone();
            }
            return Ok((
                body,
                final_url,
                etag,
                modified,
                started.elapsed(),
                loaded.manifest,
            ));
        }
        Err(if range_unsupported_seen {
            HlsRangeError::RangeUnsupported
        } else {
            last_error.unwrap_or(HlsRangeError::RangeUnsupported)
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn diagnose_etag_mismatch_prefix<F>(
        &self,
        client: &reqwest::Client,
        resource: &HlsMediaResource,
        url: &str,
        expected_origin: &str,
        chunk_key: &RangeChunkKey,
        manifest: &PersistedRangeManifest,
        baseline_etag: &str,
        priority: HlsRangePriority,
        control: &F,
    ) -> EtagMismatchProbe
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        let mut probe = EtagMismatchProbe::default();
        let prefix_length = manifest.prefix_length;
        let Some(total_length) = manifest.total_length else {
            return probe;
        };
        if prefix_length == 0
            || prefix_length > RANGE_STARTUP_CHUNK_BYTES
            || prefix_length > total_length
        {
            return probe;
        }
        let response = match self
            .send_range_request(
                client,
                resource,
                url,
                0..prefix_length,
                chunk_key,
                priority,
                control,
            )
            .await
        {
            Ok(response) => response,
            Err(_) => return probe,
        };
        let final_origin = media_url_origin(response.url().as_str());
        probe.same_origin = final_origin.as_deref() == Some(expected_origin);
        let headers = response.headers().clone();
        probe.prefix_etag_syntax_valid = etag_header_syntax_valid(&headers);
        probe.baseline_etag_matches = strong_etag(&headers).as_deref() == Some(baseline_etag);
        probe.total_matches = response.status() == StatusCode::PARTIAL_CONTENT
            && parse_content_range_header(&headers) == Some((0, prefix_length - 1, total_length));
        if !probe.total_matches {
            return probe;
        }
        let bytes = match self
            .read_range_body(response, prefix_length, chunk_key, priority, control)
            .await
        {
            Ok(bytes) => bytes,
            Err(_) => return probe,
        };
        probe.prefix_hash_matches = manifest
            .prefix_sha256
            .as_deref()
            .is_some_and(|digest| sha256_hex(&bytes) == digest);
        probe
    }

    // Prefix verification shares candidate request control without requiring a 'static task.
    #[allow(clippy::too_many_arguments)]
    async fn verify_candidate_prefix<F>(
        &self,
        client: &reqwest::Client,
        session_id: &str,
        resource: &HlsMediaResource,
        url: &str,
        expected_origin: Option<&str>,
        chunk_key: &RangeChunkKey,
        manifest: &PersistedRangeManifest,
        priority: HlsRangePriority,
        control: &F,
    ) -> Result<(String, Option<String>), CandidateVerificationError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        let prefix_length = manifest.prefix_length;
        if prefix_length == 0 || prefix_length > chunk_key.total_length {
            return Err(CandidateVerificationError::Abort(identity_error_at(
                "prefix-length-binding",
            )));
        }
        let response = self
            .send_range_request(
                client,
                resource,
                url,
                0..prefix_length,
                chunk_key,
                priority,
                control,
            )
            .await?;
        let final_url = response.url().as_str().to_owned();
        let final_origin = media_url_origin(&final_url)
            .ok_or_else(|| HlsRangeError::InvalidResponse("invalid CDN origin".to_owned()))?;
        if expected_origin.is_some_and(|expected| final_origin != expected) {
            return Err(CandidateVerificationError::Fallback(
                HlsRangeError::InvalidResponse(
                    "prefix response redirected to an unvalidated origin".to_owned(),
                ),
            ));
        }
        if response.status() == StatusCode::OK {
            self.record_cdn_observation(
                resource,
                &final_url,
                passive_cache_observation(
                    CdnObservationOutcome::Partial,
                    0,
                    None,
                    None,
                    Some(false),
                ),
            );
            return Err(CandidateVerificationError::Fallback(
                HlsRangeError::RangeUnsupported,
            ));
        }
        if response.status() != StatusCode::PARTIAL_CONTENT {
            self.record_cdn_observation(
                resource,
                &final_url,
                passive_cache_observation(
                    cache_outcome_for_status(response.status()),
                    0,
                    None,
                    None,
                    None,
                ),
            );
            return Err(CandidateVerificationError::Fallback(
                HlsRangeError::UpstreamStatus(response.status()),
            ));
        }
        if parse_content_range_header(response.headers())
            != Some((0, prefix_length - 1, chunk_key.total_length))
        {
            self.record_cdn_observation(
                resource,
                &final_url,
                passive_cache_observation(
                    CdnObservationOutcome::IntegrityMismatch,
                    0,
                    None,
                    None,
                    Some(false),
                ),
            );
            return Err(CandidateVerificationError::Fallback(
                HlsRangeError::InvalidResponse("upstream prefix range is inconsistent".to_owned()),
            ));
        }
        let etag = strong_etag(response.headers());
        let bytes = match self
            .read_range_body(response, prefix_length, chunk_key, priority, control)
            .await
        {
            Ok(bytes) => bytes,
            Err(error) => {
                if !matches!(
                    &error,
                    HlsRangeError::Cancelled
                        | HlsRangeError::Preempted
                        | HlsRangeError::SessionRemoving
                ) {
                    self.record_cdn_observation(
                        resource,
                        &final_url,
                        passive_cache_observation(
                            range_outcome_for_error(&error),
                            0,
                            None,
                            None,
                            None,
                        ),
                    );
                }
                return Err(error.into());
            }
        };
        if Some(sha256_hex(&bytes)) != manifest.prefix_sha256 {
            self.record_cdn_observation(
                resource,
                &final_url,
                passive_cache_observation(
                    CdnObservationOutcome::IntegrityMismatch,
                    bytes.len() as u64,
                    None,
                    None,
                    Some(false),
                ),
            );
            return Err(CandidateVerificationError::Mismatch(
                HlsRangeError::InvalidResponse(
                    "candidate prefix content does not match the durable prefix".to_owned(),
                ),
            ));
        }
        self.verify_candidate_samples(
            client,
            session_id,
            resource,
            url,
            &final_origin,
            chunk_key,
            manifest,
            priority,
            control,
        )
        .await?;
        self.record_cdn_observation(
            resource,
            &final_url,
            passive_cache_observation(
                CdnObservationOutcome::Partial,
                bytes.len() as u64,
                None,
                None,
                Some(true),
            ),
        );
        Ok((final_origin, etag))
    }

    #[allow(clippy::too_many_arguments)]
    async fn verify_candidate_samples<F>(
        &self,
        client: &reqwest::Client,
        session_id: &str,
        resource: &HlsMediaResource,
        url: &str,
        expected_origin: &str,
        chunk_key: &RangeChunkKey,
        manifest: &PersistedRangeManifest,
        priority: HlsRangePriority,
        control: &F,
    ) -> Result<(), CandidateVerificationError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        for sample in select_cross_cdn_sample_ranges(&manifest.extents, manifest.prefix_length) {
            let Some(durable_bytes) = self
                .read_durable_range(session_id, resource, sample.clone())
                .await
                .map_err(CandidateVerificationError::Abort)?
            else {
                return Err(CandidateVerificationError::Abort(identity_error_at(
                    "sample-no-longer-durable",
                )));
            };
            let response = self
                .send_range_request(
                    client,
                    resource,
                    url,
                    sample.clone(),
                    chunk_key,
                    priority,
                    control,
                )
                .await?;
            if response.status() == StatusCode::OK {
                return Err(CandidateVerificationError::Fallback(
                    HlsRangeError::RangeUnsupported,
                ));
            }
            if response.status() != StatusCode::PARTIAL_CONTENT {
                return Err(CandidateVerificationError::Fallback(
                    HlsRangeError::UpstreamStatus(response.status()),
                ));
            }
            let response_origin = media_url_origin(response.url().as_str()).ok_or_else(|| {
                HlsRangeError::InvalidResponse("invalid sample origin".to_owned())
            })?;
            if response_origin != expected_origin {
                return Err(CandidateVerificationError::Fallback(
                    HlsRangeError::InvalidResponse(
                        "sample response redirected to an unvalidated origin".to_owned(),
                    ),
                ));
            }
            if parse_content_range_header(response.headers())
                != Some((sample.start, sample.end - 1, chunk_key.total_length))
            {
                return Err(CandidateVerificationError::Fallback(
                    HlsRangeError::InvalidResponse(
                        "upstream sample range does not match the request".to_owned(),
                    ),
                ));
            }
            let sample_bytes = self
                .read_range_body(
                    response,
                    sample.end - sample.start,
                    chunk_key,
                    priority,
                    control,
                )
                .await?;
            if sample_bytes != durable_bytes {
                self.record_cdn_observation(
                    resource,
                    url,
                    passive_cache_observation(
                        CdnObservationOutcome::IntegrityMismatch,
                        sample_bytes.len() as u64,
                        None,
                        None,
                        Some(false),
                    ),
                );
                return Err(CandidateVerificationError::Mismatch(
                    HlsRangeError::InvalidResponse(
                        "candidate sample content does not match the durable extent".to_owned(),
                    ),
                ));
            }
        }
        Ok(())
    }
}

impl HlsCacheSessionDirectoryScan {
    pub(crate) fn next_session_id(&mut self) -> Option<io::Result<Option<String>>> {
        let entry = match self.entries.as_mut()?.next()? {
            Ok(entry) => entry,
            Err(error) => return Some(Err(error)),
        };
        let Some(session_id) = entry.file_name().to_str().map(str::to_owned) else {
            return Some(Ok(None));
        };
        if validate_cache_id(&session_id).is_err() {
            return Some(Ok(None));
        }
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => Some(Ok(Some(session_id))),
            Ok(_) => Some(Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("HLS cache session entry {session_id} is not a directory"),
            ))),
            Err(error) => Some(Err(error)),
        }
    }
}

struct HlsTranscodingCommitGuard {
    store: HlsCacheStore,
    session_id: String,
    active: bool,
    last_refresh: Mutex<Option<Instant>>,
}

impl HlsTranscodingCommitGuard {
    fn create_if_needed(store: &HlsCacheStore, session: &HlsPlaybackSession) -> io::Result<Self> {
        let mut guard = Self {
            store: store.clone(),
            session_id: session.id.clone(),
            active: false,
            last_refresh: Mutex::new(None),
        };
        if session.transcoding.state != HlsTranscodingPlanState::Ready {
            return Ok(guard);
        }

        let marker_path = store.transcoding_commit_marker_path(&session.id)?;
        store.prepare_temp_path(&marker_path)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker_path)?;
        file.write_all(b"active\n")?;
        file.sync_all()?;
        guard.active = true;
        *guard
            .last_refresh
            .lock()
            .map_err(|_| io::Error::other("HLS transcoding marker refresh state was poisoned"))? =
            Some(Instant::now());
        Ok(guard)
    }

    fn refresh(&self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }

        let marker_path = self
            .store
            .transcoding_commit_marker_path(&self.session_id)?;
        self.store.reject_cache_path_symlink(&marker_path)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&marker_path)?;
        file.write_all(b"active\n")?;
        file.sync_all()?;
        *self
            .last_refresh
            .lock()
            .map_err(|_| io::Error::other("HLS transcoding marker refresh state was poisoned"))? =
            Some(Instant::now());
        Ok(())
    }

    fn refresh_if_due(&self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }

        let now = Instant::now();
        let should_refresh = self
            .last_refresh
            .lock()
            .map_err(|_| io::Error::other("HLS transcoding marker refresh state was poisoned"))?
            .is_none_or(|last_refresh| {
                now.duration_since(last_refresh) >= HLS_TRANSCODING_COMMIT_MARKER_REFRESH_INTERVAL
            });
        if should_refresh {
            self.refresh()?;
        }
        Ok(())
    }

    fn finish(mut self) {
        if self.active
            && let Err(error) = self
                .store
                .remove_transcoding_commit_marker_if_exists(&self.session_id)
        {
            eprintln!("Failed to remove HLS transcoding commit marker: {error}");
        }
        self.active = false;
    }
}

impl Drop for HlsTranscodingCommitGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = self
                .store
                .remove_transcoding_commit_marker_if_exists(&self.session_id);
        }
    }
}

fn referenced_session_managed_file_names(session: &HlsPlaybackSession) -> HashSet<String> {
    let mut retained = HashSet::from(["session.json".to_owned()]);
    for resource in session_unique_media_resources(session) {
        insert_resource_managed_file_names(&mut retained, &resource.id);
    }
    retained
}

fn session_unique_media_resources(session: &HlsPlaybackSession) -> Vec<&HlsMediaResource> {
    let mut seen = HashSet::new();
    session_media_resources(session)
        .filter(|resource| seen.insert(resource.id.clone()))
        .collect()
}

fn session_media_resources(
    session: &HlsPlaybackSession,
) -> impl Iterator<Item = &HlsMediaResource> {
    std::iter::once(&session.variant)
        .chain(session.alternate_variants.iter())
        .flat_map(variant_media_resources)
}

fn variant_media_resources(variant: &HlsVariant) -> impl Iterator<Item = &HlsMediaResource> {
    variant.audio.iter().chain(std::iter::once(&variant.video))
}

fn insert_resource_managed_file_names(retained: &mut HashSet<String>, resource_id: &str) {
    retained.insert(resource_id.to_owned());
    retained.insert(format!("{resource_id}.json"));
    retained.insert(format!("{resource_id}.prewarm"));
    retained.insert(format!("{resource_id}.prewarm.json"));
    retained.insert(format!("{resource_id}.range.data"));
    retained.insert(format!("{resource_id}.range.json"));
    retained.insert(format!("{resource_id}.range.tmp"));
    retained.insert(format!("{resource_id}.range-full.tmp"));
}

fn is_managed_resource_file_name(file_name: &str) -> bool {
    if is_transcoding_temp_file_name(file_name) {
        return true;
    }
    if file_name.ends_with(".tmp")
        && !file_name.ends_with(".range.tmp")
        && !file_name.ends_with(".range-full.tmp")
    {
        return false;
    }
    managed_resource_id_from_file_name(file_name)
        .is_some_and(|resource_id| validate_cache_id(resource_id).is_ok())
}

fn is_transcoding_temp_file_name(file_name: &str) -> bool {
    file_name.ends_with(HLS_TRANSCODING_TEMP_FILE_SUFFIX)
        && file_name == transcoding_temp_file_name(HLS_TRANSCODED_RESOURCE_ID)
}

fn transcoding_temp_path_for_output(output_path: &Path) -> PathBuf {
    output_path.with_extension("transcode.tmp")
}

fn transcoding_temp_file_name(resource_id: &str) -> String {
    Path::new(resource_id)
        .with_extension("transcode.tmp")
        .to_string_lossy()
        .into_owned()
}

fn managed_resource_id_from_file_name(file_name: &str) -> Option<&str> {
    for suffix in [
        ".range-full.tmp",
        ".range.data",
        ".range.json",
        ".range.tmp",
        ".prewarm.json",
        ".prewarm",
        ".json",
    ] {
        if let Some(resource_id) = file_name.strip_suffix(suffix) {
            return Some(resource_id);
        }
    }
    Some(file_name)
}

fn hls_segments_from_mp4_ranges(ranges: Vec<Mp4SegmentRange>) -> Vec<HlsMediaSegment> {
    ranges
        .into_iter()
        .map(|range| HlsMediaSegment {
            byte_range_offset: range.offset,
            byte_range_length: range.length,
            duration_millis: range.duration_millis,
        })
        .collect()
}

fn validated_cached_segments(
    segments: Vec<PersistedHlsMediaSegment>,
    initialization_length: u64,
    total_length: u64,
) -> Vec<HlsMediaSegment> {
    if segments.is_empty() {
        return Vec::new();
    }
    let mut previous_end = initialization_length;
    let mut validated = Vec::with_capacity(segments.len());
    for segment in segments {
        if segment.byte_range_length == 0
            || segment.duration_millis == 0
            || segment.byte_range_offset != previous_end
        {
            return Vec::new();
        }
        let end = segment
            .byte_range_offset
            .checked_add(segment.byte_range_length);
        let Some(end) = end else {
            return Vec::new();
        };
        if end > total_length {
            return Vec::new();
        }
        previous_end = end;
        validated.push(HlsMediaSegment {
            byte_range_offset: segment.byte_range_offset,
            byte_range_length: segment.byte_range_length,
            duration_millis: segment.duration_millis,
        });
    }
    if previous_end == total_length {
        validated
    } else {
        Vec::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CachedHlsResource {
    pub(crate) path: PathBuf,
    pub(crate) content_type: String,
    pub(crate) initialization_length: u64,
    pub(crate) total_length: u64,
    pub(crate) segments: Vec<HlsMediaSegment>,
    pub(crate) last_modified: SystemTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrewarmedHlsResource {
    pub(crate) path: PathBuf,
    pub(crate) content_type: String,
    pub(crate) initialization_length: u64,
    pub(crate) prefix_length: u64,
    pub(crate) target_prefix_length: u64,
    pub(crate) target_window_seconds: u64,
    pub(crate) total_length: u64,
    pub(crate) last_modified: SystemTime,
}

pub(crate) struct OpenedPrewarmedHlsResource {
    pub(crate) file: std::fs::File,
    pub(crate) content_type: String,
    pub(crate) last_modified: SystemTime,
    pub(crate) prefix_length: u64,
    pub(crate) total_length: u64,
}

#[derive(Debug)]
pub(crate) enum HlsCacheError {
    Io(io::Error),
    Network(reqwest::Error),
    UpstreamStatus(StatusCode),
    InvalidResource(String),
    Range(HlsRangeError),
    Cancelled,
    Preempted,
}

impl From<io::Error> for HlsCacheError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<reqwest::Error> for HlsCacheError {
    fn from(error: reqwest::Error) -> Self {
        Self::Network(error)
    }
}

impl std::fmt::Display for HlsCacheError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Network(error) => write!(formatter, "network error: {error}"),
            Self::UpstreamStatus(status) => write!(formatter, "upstream returned {status}"),
            Self::InvalidResource(message) => formatter.write_str(message),
            Self::Range(error) => write!(formatter, "{error}"),
            Self::Cancelled => formatter.write_str("HLS cache finalization was cancelled"),
            Self::Preempted => formatter.write_str("HLS cache finalization was preempted"),
        }
    }
}

impl std::error::Error for HlsCacheError {}

async fn send_request_with_control(
    request: reqwest::RequestBuilder,
    control: &(impl Fn() -> HlsCacheFillControl + Send + Sync),
) -> Result<reqwest::Response, HlsCacheError> {
    let send = request.send();
    tokio::pin!(send);
    loop {
        check_fill_control(control)?;
        let response = tokio::select! {
            response = &mut send => response,
            () = tokio::time::sleep(Duration::from_millis(100)) => {
                continue;
            }
        };
        return response.map_err(HlsCacheError::from);
    }
}

struct DownloadedResourcePrefix {
    prefix_length: u64,
    target_prefix_length: u64,
    target_window_seconds: u64,
    total_length: u64,
    initialization_length: u64,
    final_url: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct HlsPlaybackPrefetchContext {
    position_seconds: f64,
    duration_seconds: Option<f64>,
    last_intent: PlaybackProgressIntent,
}

impl HlsPlaybackPrefetchContext {
    fn should_extend_prefetch_window(self) -> bool {
        matches!(self.last_intent, PlaybackProgressIntent::Seek)
            || self.position_seconds >= HLS_FIRST_WINDOW_PREFETCH_SECONDS as f64
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HlsPrefetchPrefixTarget {
    prefix_bytes: u64,
    window_seconds: u64,
}

async fn download_resource_prefix(
    client: &reqwest::Client,
    resource: &HlsMediaResource,
    url: &str,
    temp_path: &Path,
    target: HlsPrefetchPrefixTarget,
    control: &(impl Fn() -> HlsCacheFillControl + Send + Sync),
    history: &CdnHistory,
) -> Result<DownloadedResourcePrefix, HlsCacheError> {
    check_fill_control(control)?;
    let target_prefix_length = target.prefix_bytes;
    let mut request = client.get(url).header(
        reqwest::header::RANGE,
        format!("bytes=0-{}", target_prefix_length - 1),
    );
    let mut requested_range = false;
    for header in &resource.request.headers {
        if header.name.eq_ignore_ascii_case("range") {
            requested_range = true;
            continue;
        }
        if !should_forward_media_request_header(&header.name, &resource.request.url, url) {
            continue;
        }
        request = request.header(header.name.as_str(), header.value.as_str());
    }
    if requested_range {
        return Err(HlsCacheError::InvalidResource(
            "offline HLS cache prewarm does not support range-only media requests".to_owned(),
        ));
    }

    let response = match send_request_with_control(request, control).await {
        Ok(response) => response,
        Err(HlsCacheError::Network(error)) => {
            if let Some(error_url) = error.url() {
                record_response_observation(
                    history,
                    resource,
                    error_url.as_str(),
                    passive_cache_observation(
                        cache_outcome_for_transport(&error),
                        0,
                        None,
                        None,
                        Some(false),
                    ),
                );
            }
            return Err(error.into());
        }
        Err(error) => return Err(error),
    };
    let final_url = response.url().as_str().to_owned();
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if status != StatusCode::PARTIAL_CONTENT {
        record_response_observation(
            history,
            resource,
            &final_url,
            passive_cache_observation(
                if status.is_success() {
                    CdnObservationOutcome::IntegrityMismatch
                } else {
                    cache_outcome_for_status(status)
                },
                0,
                None,
                None,
                Some(false),
            ),
        );
        return Err(HlsCacheError::InvalidResource(format!(
            "HLS prewarm expected partial content, got {status}"
        )));
    }
    let headers = response.headers().clone();
    let Some((start, end, total_length)) = parse_content_range_header(&headers) else {
        record_response_observation(
            history,
            resource,
            &final_url,
            passive_cache_observation(
                CdnObservationOutcome::IntegrityMismatch,
                0,
                None,
                None,
                Some(false),
            ),
        );
        return Err(HlsCacheError::InvalidResource(
            "HLS prewarm response did not include Content-Range".to_owned(),
        ));
    };
    if start != 0 || end < start || end >= total_length {
        record_response_observation(
            history,
            resource,
            &final_url,
            passive_cache_observation(
                CdnObservationOutcome::IntegrityMismatch,
                0,
                None,
                None,
                Some(false),
            ),
        );
        return Err(HlsCacheError::InvalidResource(
            "HLS prewarm Content-Range was invalid".to_owned(),
        ));
    }
    let prefix_length = end.saturating_add(1);
    if resource
        .request
        .size
        .is_some_and(|expected_total| expected_total != total_length)
    {
        record_response_observation(
            history,
            resource,
            &final_url,
            passive_cache_observation(
                CdnObservationOutcome::IntegrityMismatch,
                0,
                None,
                None,
                Some(false),
            ),
        );
        return Err(HlsCacheError::InvalidResource(
            "HLS prewarm Content-Range total did not match expected size".to_owned(),
        ));
    }
    if prefix_length > target_prefix_length {
        record_response_observation(
            history,
            resource,
            &final_url,
            passive_cache_observation(
                CdnObservationOutcome::IntegrityMismatch,
                0,
                None,
                None,
                Some(false),
            ),
        );
        return Err(HlsCacheError::InvalidResource(
            "HLS prewarm response exceeded bounded prefix length".to_owned(),
        ));
    }
    if let Some(declared_length) = response.content_length()
        && declared_length != prefix_length
    {
        record_response_observation(
            history,
            resource,
            &final_url,
            passive_cache_observation(
                CdnObservationOutcome::IntegrityMismatch,
                0,
                None,
                None,
                Some(false),
            ),
        );
        return Err(HlsCacheError::InvalidResource(format!(
            "HLS prewarm Content-Length {declared_length} did not match prefix length {prefix_length}"
        )));
    }

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)
        .await?;
    let mut bytes = Vec::with_capacity(prefix_length.try_into().unwrap_or(usize::MAX));
    let mut stream = response.bytes_stream();
    loop {
        if let Err(error) = check_fill_control(control) {
            record_response_observation(
                history,
                resource,
                &final_url,
                passive_cache_observation(
                    CdnObservationOutcome::Cancelled,
                    u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                    None,
                    None,
                    Some(true),
                ),
            );
            return Err(error);
        }
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            () = tokio::time::sleep(Duration::from_millis(100)) => {
                continue;
            }
        };
        let chunk = match chunk {
            Some(Ok(chunk)) => chunk,
            Some(Err(error)) => {
                record_response_observation(
                    history,
                    resource,
                    &final_url,
                    passive_cache_observation(
                        cache_outcome_for_transport(&error),
                        u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                        None,
                        None,
                        Some(true),
                    ),
                );
                return Err(error.into());
            }
            None => break,
        };
        let next_len = bytes.len().checked_add(chunk.len()).ok_or_else(|| {
            HlsCacheError::InvalidResource("HLS prewarm prefix is too large".to_owned())
        })?;
        if u64::try_from(next_len).unwrap_or(u64::MAX) > prefix_length {
            record_response_observation(
                history,
                resource,
                &final_url,
                passive_cache_observation(
                    CdnObservationOutcome::IntegrityMismatch,
                    u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                    None,
                    None,
                    Some(false),
                ),
            );
            return Err(HlsCacheError::InvalidResource(
                "HLS prewarm body exceeded Content-Range length".to_owned(),
            ));
        }
        if let Err(error) = check_fill_control(control) {
            record_response_observation(
                history,
                resource,
                &final_url,
                passive_cache_observation(
                    CdnObservationOutcome::Cancelled,
                    u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                    None,
                    None,
                    Some(true),
                ),
            );
            return Err(error);
        }
        file.write_all(&chunk).await?;
        bytes.extend_from_slice(&chunk);
    }
    file.sync_all().await?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != prefix_length {
        record_response_observation(
            history,
            resource,
            &final_url,
            passive_cache_observation(
                CdnObservationOutcome::IntegrityMismatch,
                u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                None,
                None,
                Some(false),
            ),
        );
        return Err(HlsCacheError::InvalidResource(format!(
            "HLS prewarm body length {} did not match Content-Range length {prefix_length}",
            bytes.len()
        )));
    }
    let Some(initialization_length) = mp4_initialization_length(&bytes) else {
        record_response_observation(
            history,
            resource,
            &final_url,
            passive_cache_observation(
                CdnObservationOutcome::IntegrityMismatch,
                prefix_length,
                None,
                None,
                Some(false),
            ),
        );
        return Err(HlsCacheError::InvalidResource(
            "prewarmed HLS MP4 init box not found".to_owned(),
        ));
    };

    Ok(DownloadedResourcePrefix {
        prefix_length,
        target_prefix_length,
        target_window_seconds: target.window_seconds,
        total_length,
        initialization_length,
        final_url,
    })
}

#[cfg(test)]
fn hls_first_window_prefetch_prefix_bytes(resource: &HlsMediaResource) -> u64 {
    hls_prefetch_prefix_target(resource, None).prefix_bytes
}

fn hls_prefetch_prefix_target(
    resource: &HlsMediaResource,
    playback: Option<&HlsPlaybackPrefetchContext>,
) -> HlsPrefetchPrefixTarget {
    let window_seconds = hls_prefetch_window_seconds(resource, playback);
    let max_bytes = if window_seconds > HLS_FIRST_WINDOW_PREFETCH_SECONDS {
        HLS_PLAYBACK_POSITION_PREFETCH_MAX_BYTES
    } else {
        HLS_FIRST_WINDOW_PREFETCH_MAX_BYTES
    };
    let bitrate_window_bytes = resource
        .request
        .bandwidth
        .map(|bandwidth_bits_per_second| {
            bandwidth_bits_per_second
                .saturating_mul(window_seconds)
                .saturating_add(7)
                / 8
        })
        .unwrap_or_default();
    let target = HLS_PREWARM_HEAD_BYTES
        .saturating_add(bitrate_window_bytes)
        .clamp(HLS_PREWARM_HEAD_BYTES, max_bytes);

    let prefix_bytes = resource
        .request
        .size
        .filter(|size| *size > 0)
        .map(|size| target.min(size))
        .unwrap_or(target)
        .max(1);

    HlsPrefetchPrefixTarget {
        prefix_bytes,
        window_seconds,
    }
}

fn hls_prefetch_window_seconds(
    resource: &HlsMediaResource,
    playback: Option<&HlsPlaybackPrefetchContext>,
) -> u64 {
    let Some(playback) = playback else {
        return HLS_FIRST_WINDOW_PREFETCH_SECONDS;
    };
    let Some(position_seconds) = ceil_seconds(playback.position_seconds) else {
        return HLS_FIRST_WINDOW_PREFETCH_SECONDS;
    };
    if !playback.should_extend_prefetch_window() {
        return HLS_FIRST_WINDOW_PREFETCH_SECONDS;
    }
    let playback_window_end = position_seconds.saturating_add(HLS_FIRST_WINDOW_PREFETCH_SECONDS);
    let duration_seconds = playback
        .duration_seconds
        .and_then(ceil_positive_seconds)
        .or_else(|| resource.request.duration_seconds.map(u64::from));
    let window_end = duration_seconds
        .filter(|duration_seconds| *duration_seconds > 0)
        .map(|duration_seconds| playback_window_end.min(duration_seconds))
        .unwrap_or(playback_window_end);
    window_end.max(HLS_FIRST_WINDOW_PREFETCH_SECONDS)
}

fn hls_playback_prefetch_context(
    session: &HlsPlaybackSession,
    snapshot: Option<&HlsPlaybackProgressSnapshot>,
) -> Option<HlsPlaybackPrefetchContext> {
    let snapshot = snapshot?;
    if snapshot.state != HlsPlaybackActivityState::Active || snapshot.session_id != session.id {
        return None;
    }
    let variant_id = snapshot.variant_id.trim();
    if !variant_id.is_empty() && variant_id != session.variant.id {
        return None;
    }
    if !snapshot.position_seconds.is_finite() || snapshot.position_seconds < 0.0 {
        return None;
    }

    Some(HlsPlaybackPrefetchContext {
        position_seconds: snapshot.position_seconds,
        duration_seconds: snapshot
            .duration_seconds
            .filter(|duration_seconds| duration_seconds.is_finite() && *duration_seconds > 0.0),
        last_intent: snapshot.last_intent,
    })
}

fn ceil_seconds(value: f64) -> Option<u64> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    if value >= u64::MAX as f64 {
        return Some(u64::MAX);
    }
    Some(value.ceil() as u64)
}

fn ceil_positive_seconds(value: f64) -> Option<u64> {
    let seconds = ceil_seconds(value)?;
    (seconds > 0).then_some(seconds)
}

fn range_representation_digest(resource: &HlsMediaResource) -> String {
    let request = &resource.request;
    sha256_hex(
        format!(
            "{}\n{}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}",
            resource.id,
            request.cache_key.content_id,
            request.kind,
            request.stream_id,
            request.cache_key.media_kind,
            request.cache_key.stream_id,
            request.cache_key.codecs,
            request.codecs,
            request.mime_type,
            request.bandwidth,
            request.width.zip(request.height),
            request.size.zip(request.duration_seconds),
        )
        .as_bytes(),
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn validate_range_manifest(
    manifest: &PersistedRangeManifest,
    resource: &HlsMediaResource,
) -> Result<(), HlsRangeError> {
    if manifest.schema_version != crate::hls_range_cache::RANGE_MANIFEST_SCHEMA_VERSION
        || manifest.generation == 0
        || manifest.resource_id != resource.id
        || manifest.representation_digest != range_representation_digest(resource)
        || manifest.extents.len() > RANGE_MAX_CHUNKS as usize
        || manifest.validated_origins.len() > 64
        || manifest
            .strong_etag
            .as_deref()
            .is_some_and(|etag| !strong_etag_string_is_valid(etag))
    {
        return Err(identity_error_at("manifest-schema-resource-binding"));
    }
    let total = manifest.total_length;
    let mut previous_end = 0;
    let mut durable_bytes = 0_u64;
    for extent in &manifest.extents {
        if extent.start >= extent.end
            || extent.start < previous_end
            || total.is_some_and(|length| extent.end > length)
            || extent.sha256.len() != 64
            || !extent.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(HlsRangeError::InvalidResponse(
                "invalid range checkpoint extents".to_owned(),
            ));
        }
        previous_end = extent.end;
        durable_bytes = durable_bytes.saturating_add(extent.end - extent.start);
    }
    if durable_bytes != manifest.durable_bytes
        || manifest.prefix_length > RANGE_STARTUP_CHUNK_BYTES
        || (manifest.prefix_length == 0) != manifest.prefix_sha256.is_none()
        || (manifest.prefix_length > 0
            && (!range_is_covered(&manifest.extents, 0..manifest.prefix_length)
                || manifest
                    .prefix_sha256
                    .as_ref()
                    .is_none_or(|digest| digest.len() != 64)))
    {
        return Err(HlsRangeError::InvalidResponse(
            "invalid range checkpoint summary".to_owned(),
        ));
    }
    if manifest.validated_origins.iter().any(|entry| {
        entry.origin.len() > 512
            || entry.prefix_sha256.len() != 64
            || entry
                .strong_etag
                .as_deref()
                .is_some_and(|etag| !strong_etag_string_is_valid(etag))
            || media_url_origin(&entry.origin).as_deref() != Some(entry.origin.as_str())
    }) {
        return Err(HlsRangeError::InvalidResponse(
            "invalid range checkpoint origin".to_owned(),
        ));
    }
    Ok(())
}

fn range_manifest_rebase_compatible(
    expected: &PersistedRangeManifest,
    current: &PersistedRangeManifest,
) -> bool {
    range_manifest_rebase_compatible_ignoring_origin(expected, current, None, false)
}

fn range_manifest_rebase_compatible_for_origin_validation(
    expected: &PersistedRangeManifest,
    current: &PersistedRangeManifest,
    target_origin: &str,
) -> bool {
    range_manifest_rebase_compatible_ignoring_origin(expected, current, Some(target_origin), true)
}

fn range_manifest_rebase_compatible_for_extent_publication(
    expected: &PersistedRangeManifest,
    current: &PersistedRangeManifest,
    target_origin: &str,
) -> bool {
    if !range_manifest_rebase_compatible_ignoring_origin(expected, current, None, true) {
        return false;
    }
    let binding_state = |manifest: &PersistedRangeManifest| {
        manifest
            .validated_origins
            .iter()
            .find(|binding| binding.origin == target_origin)
            .map(|binding| (binding.prefix_sha256.clone(), binding.strong_etag.clone()))
    };
    binding_state(expected) == binding_state(current)
}

fn range_manifest_target_validator_snapshot_is_stale(
    expected: &PersistedRangeManifest,
    current: &PersistedRangeManifest,
    target_origin: &str,
) -> bool {
    if !range_manifest_rebase_compatible_ignoring_origin(
        expected,
        current,
        Some(target_origin),
        false,
    ) {
        return false;
    }
    let expected_binding = expected
        .validated_origins
        .iter()
        .find(|binding| binding.origin == target_origin);
    let current_binding = current
        .validated_origins
        .iter()
        .find(|binding| binding.origin == target_origin);
    matches!(
        (expected_binding, current_binding),
        (Some(expected), Some(current))
            if expected.prefix_sha256 == current.prefix_sha256
                && matches!(
                    (&expected.strong_etag, &current.strong_etag),
                    (Some(expected), Some(current)) if expected != current
                )
    )
}

// This mirrors the extent rebase predicate only to name its first failed signal.
fn range_manifest_extent_rebase_failure_stage(
    expected: &PersistedRangeManifest,
    current: &PersistedRangeManifest,
    target_origin: &str,
) -> Option<&'static str> {
    if expected.resource_id != current.resource_id {
        return Some("extent-publish-rebase-resource-id");
    }
    if expected.representation_digest != current.representation_digest {
        return Some("extent-publish-rebase-representation");
    }
    if expected.data_identity != current.data_identity {
        return Some("extent-publish-rebase-data-object-identity");
    }
    if current.generation < expected.generation {
        return Some("extent-publish-rebase-generation-regression");
    }
    if expected
        .total_length
        .is_some_and(|total| current.total_length != Some(total))
    {
        return Some("extent-publish-rebase-total-length");
    }
    if expected
        .prefix_sha256
        .as_ref()
        .is_some_and(|prefix| current.prefix_sha256.as_ref() != Some(prefix))
    {
        return Some("extent-publish-rebase-prefix-digest");
    }
    for old in &expected.validated_origins {
        let Some(new) = current
            .validated_origins
            .iter()
            .find(|new| new.origin == old.origin)
        else {
            return Some(if old.origin == target_origin {
                "extent-publish-rebase-target-origin-removed"
            } else {
                "extent-publish-rebase-existing-origin-prefix"
            });
        };
        if new.prefix_sha256 != old.prefix_sha256 {
            return Some(if old.origin == target_origin {
                "extent-publish-rebase-target-origin-prefix"
            } else {
                "extent-publish-rebase-existing-origin-prefix"
            });
        }
    }
    if expected.extents.iter().any(|old| {
        !current
            .extents
            .iter()
            .any(|new| new.start == old.start && new.end == old.end && new.sha256 == old.sha256)
    }) {
        return Some("extent-publish-rebase-existing-extent");
    }

    let expected_binding = expected
        .validated_origins
        .iter()
        .find(|binding| binding.origin == target_origin);
    let current_binding = current
        .validated_origins
        .iter()
        .find(|binding| binding.origin == target_origin);
    match (expected_binding, current_binding) {
        (Some(expected), Some(current)) if expected.prefix_sha256 != current.prefix_sha256 => {
            Some("extent-publish-rebase-target-origin-prefix")
        }
        (Some(expected), Some(current)) if expected.strong_etag != current.strong_etag => {
            Some("extent-publish-rebase-target-origin-strong-etag")
        }
        (None, None) | (Some(_), Some(_)) => None,
        (None, Some(_)) => Some("extent-publish-rebase-target-origin-added"),
        (Some(_), None) => Some("extent-publish-rebase-target-origin-removed"),
    }
}

fn range_manifest_rebase_compatible_ignoring_origin(
    expected: &PersistedRangeManifest,
    current: &PersistedRangeManifest,
    ignored_origin: Option<&str>,
    allow_validator_update: bool,
) -> bool {
    if expected.resource_id != current.resource_id
        || expected.representation_digest != current.representation_digest
        || expected.data_identity != current.data_identity
        || current.generation < expected.generation
        || expected
            .total_length
            .is_some_and(|total| current.total_length != Some(total))
        || expected
            .prefix_sha256
            .as_ref()
            .is_some_and(|prefix| current.prefix_sha256.as_ref() != Some(prefix))
    {
        return false;
    }
    for old in &expected.validated_origins {
        if ignored_origin == Some(old.origin.as_str()) {
            continue;
        }
        if current
            .validated_origins
            .iter()
            .find(|new| new.origin == old.origin)
            .is_none_or(|new| {
                new.prefix_sha256 != old.prefix_sha256
                    || (!allow_validator_update && new.strong_etag != old.strong_etag)
            })
        {
            return false;
        }
    }
    expected.extents.iter().all(|old| {
        current
            .extents
            .iter()
            .any(|new| new.start == old.start && new.end == old.end && new.sha256 == old.sha256)
    })
}

fn range_is_covered(extents: &[PersistedRangeExtent], requested: Range<u64>) -> bool {
    if requested.start >= requested.end {
        return false;
    }
    let mut cursor = requested.start;
    for extent in extents {
        if extent.end <= cursor {
            continue;
        }
        if extent.start > cursor {
            return false;
        }
        cursor = cursor.max(extent.end);
        if cursor >= requested.end {
            return true;
        }
    }
    false
}

const HLS_CROSS_CDN_SAMPLE_BYTES: u64 = 16 * 1024;
const HLS_CROSS_CDN_MAX_SAMPLES: usize = 3;

fn select_cross_cdn_sample_ranges(
    extents: &[PersistedRangeExtent],
    prefix_length: u64,
) -> Vec<Range<u64>> {
    let intervals: Vec<_> = extents
        .iter()
        .filter_map(|extent| {
            let start = extent.start.max(prefix_length);
            (start < extent.end).then_some(start..extent.end)
        })
        .collect();
    let available_bytes = intervals
        .iter()
        .map(|range| range.end - range.start)
        .sum::<u64>();
    if available_bytes == 0 {
        return Vec::new();
    }

    let mut samples = Vec::with_capacity(HLS_CROSS_CDN_MAX_SAMPLES);
    for target in [0, (available_bytes - 1) / 2, available_bytes - 1] {
        let mut offset = target;
        let interval = intervals.iter().find(|range| {
            let length = range.end - range.start;
            if offset < length {
                true
            } else {
                offset -= length;
                false
            }
        });
        let Some(interval) = interval else {
            continue;
        };
        let length = (interval.end - interval.start).min(HLS_CROSS_CDN_SAMPLE_BYTES);
        let center = interval.start + offset;
        let start = center
            .saturating_sub(length / 2)
            .clamp(interval.start, interval.end - length);
        let sample = start..start + length;
        if !samples
            .iter()
            .any(|existing: &Range<u64>| existing.start < sample.end && sample.start < existing.end)
        {
            samples.push(sample);
        }
    }
    samples
}

fn durable_extent_bytes(extents: &[PersistedRangeExtent]) -> u64 {
    extents
        .iter()
        .map(|extent| extent.end.saturating_sub(extent.start))
        .fold(0, u64::saturating_add)
}

fn missing_ranges(extents: &[PersistedRangeExtent], total: u64) -> Vec<Range<u64>> {
    let mut ranges = Vec::new();
    let mut cursor = 0_u64;
    for extent in extents {
        if cursor < extent.start {
            ranges.push(cursor..extent.start);
        }
        cursor = cursor.max(extent.end);
    }
    if cursor < total {
        ranges.push(cursor..total);
    }
    ranges
}

fn planned_range_chunks(total: u64) -> Vec<Range<u64>> {
    if total == 0 {
        return Vec::new();
    }
    let first_end = total.min(RANGE_STARTUP_CHUNK_BYTES);
    let mut chunks = Vec::with_capacity(RANGE_MAX_CHUNKS as usize);
    chunks.push(0..first_end);
    if first_end == total {
        return chunks;
    }
    let remaining = total - first_end;
    let target_count = RANGE_MAX_CHUNKS.saturating_sub(1).max(1);
    let chunk_size = remaining
        .div_ceil(target_count)
        .clamp(RANGE_MIN_CHUNK_BYTES, RANGE_MAX_CHUNK_BYTES);
    let mut start = first_end;
    while start < total {
        let end = start.saturating_add(chunk_size).min(total);
        chunks.push(start..end);
        start = end;
    }
    chunks
}

fn range_size_is_resumable(total: u64) -> bool {
    total > 0 && total <= RANGE_MAX_SIZE
}

fn media_url_origin(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    let host = url.host_str()?;
    Some(format!(
        "{}://{}:{}",
        url.scheme(),
        host,
        url.port_or_known_default()?
    ))
}

fn strong_etag(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let value = headers.get(reqwest::header::ETAG)?;
    if parse_entity_tag(value.as_bytes()) != Some(false) || value.as_bytes().len() > 512 {
        return None;
    }
    std::str::from_utf8(value.as_bytes())
        .ok()
        .map(str::to_owned)
}

fn etag_header_syntax_valid(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(reqwest::header::ETAG)
        .is_some_and(|value| parse_entity_tag(value.as_bytes()).is_some())
}

fn strong_etag_string_is_valid(value: &str) -> bool {
    value.len() <= 512 && parse_entity_tag(value.as_bytes()) == Some(false)
}

fn parse_entity_tag(value: &[u8]) -> Option<bool> {
    let (weak, opaque) = if let Some(opaque) = value.strip_prefix(b"W/") {
        (true, opaque)
    } else {
        (false, value)
    };
    if opaque.len() < 2 || opaque.first() != Some(&b'"') || opaque.last() != Some(&b'"') {
        return None;
    }
    let tag = &opaque[1..opaque.len() - 1];
    tag.iter()
        .all(|byte| *byte == 0x21 || (0x23..=0x7e).contains(byte) || *byte >= 0x80)
        .then_some(weak)
}

fn file_object_identity(metadata: &fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    {
        format!("{}:{:?}", metadata.len(), metadata.created().ok())
    }
}

fn safe_file_identity(store: &HlsCacheStore, path: &Path) -> io::Result<Option<String>> {
    store.reject_cache_path_symlink(path)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "managed cache object is not a regular file",
        ));
    }
    Ok(Some(file_object_identity(&metadata)))
}

fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

fn open_range_file(options: &mut fs::OpenOptions, path: &Path) -> io::Result<fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "managed range object is not a regular file",
        ));
    }
    Ok(file)
}

fn range_outcome_for_error(error: &HlsRangeError) -> CdnObservationOutcome {
    match error {
        HlsRangeError::Network(error) => cache_outcome_for_transport(error),
        HlsRangeError::UpstreamStatus(status) => cache_outcome_for_status(*status),
        HlsRangeError::IdentityChanged => CdnObservationOutcome::IntegrityMismatch,
        HlsRangeError::InvalidResponse(reason) => {
            let local_or_checkpoint_error = reason.starts_with("invalid CDN")
                || reason.starts_with("range is too large")
                || reason.starts_with("requested HLS range")
                || reason.starts_with("invalid requested HLS chunk")
                || reason.starts_with("empty upstream byte range")
                || reason.starts_with("malformed range checkpoint")
                || reason.starts_with("invalid range checkpoint")
                || reason.starts_with("HLS range manifest exceeds");
            if local_or_checkpoint_error {
                CdnObservationOutcome::Partial
            } else {
                CdnObservationOutcome::IntegrityMismatch
            }
        }
        HlsRangeError::RangeUnsupported => CdnObservationOutcome::Partial,
        HlsRangeError::Io(_)
        | HlsRangeError::QuotaExceeded
        | HlsRangeError::Cancelled
        | HlsRangeError::Preempted
        | HlsRangeError::SessionRemoving => CdnObservationOutcome::Partial,
    }
}

fn is_retryable_range_candidate_error(error: &HlsRangeError) -> bool {
    match error {
        HlsRangeError::Network(_) => true,
        HlsRangeError::UpstreamStatus(status) => {
            status.is_server_error()
                || matches!(
                    *status,
                    StatusCode::UNAUTHORIZED
                        | StatusCode::FORBIDDEN
                        | StatusCode::NOT_FOUND
                        | StatusCode::GONE
                        | StatusCode::REQUEST_TIMEOUT
                        | StatusCode::TOO_MANY_REQUESTS
                )
        }
        _ => false,
    }
}

fn hls_range_error_to_io(error: HlsRangeError) -> io::Error {
    let kind = match error {
        HlsRangeError::QuotaExceeded => io::ErrorKind::StorageFull,
        HlsRangeError::Cancelled | HlsRangeError::Preempted => io::ErrorKind::Interrupted,
        HlsRangeError::SessionRemoving | HlsRangeError::IdentityChanged => {
            io::ErrorKind::InvalidData
        }
        HlsRangeError::Io(error) => return error,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, error.to_string())
}

static IDENTITY_DIAGNOSTIC_COUNT: AtomicUsize = AtomicUsize::new(0);
const IDENTITY_DIAGNOSTIC_BUDGET: usize = 32;

fn identity_error_at(stage: &'static str) -> HlsRangeError {
    if IDENTITY_DIAGNOSTIC_COUNT.fetch_add(1, Ordering::Relaxed) < IDENTITY_DIAGNOSTIC_BUDGET {
        eprintln!("HLS cache identity check failed at {stage}");
    }
    HlsRangeError::IdentityChanged
}

fn hls_cache_error_from_range(error: HlsRangeError) -> HlsCacheError {
    match error {
        HlsRangeError::Io(error) => HlsCacheError::Io(error),
        HlsRangeError::Network(error) => HlsCacheError::Network(error.without_url()),
        HlsRangeError::UpstreamStatus(status) => HlsCacheError::UpstreamStatus(status),
        HlsRangeError::Cancelled => HlsCacheError::Cancelled,
        HlsRangeError::Preempted => HlsCacheError::Preempted,
        other => HlsCacheError::Range(other),
    }
}

fn parse_content_range_header(headers: &reqwest::header::HeaderMap) -> Option<(u64, u64, u64)> {
    let value = headers.get(reqwest::header::CONTENT_RANGE)?.to_str().ok()?;
    let spec = value.strip_prefix("bytes ")?;
    let (range, total) = spec.rsplit_once('/')?;
    if total == "*" {
        return None;
    }
    let total = total.parse().ok()?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    (total > 0 && start <= end && end < total).then_some((start, end, total))
}

async fn cached_mp4_initialization_length(path: &Path) -> Result<u64, HlsCacheError> {
    let file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::new();
    file.take(HLS_INITIALIZATION_SCAN_BYTES)
        .read_to_end(&mut bytes)
        .await?;
    mp4_initialization_length(&bytes).ok_or_else(|| {
        HlsCacheError::InvalidResource("cached HLS MP4 init box not found".to_owned())
    })
}

fn check_fill_control(
    control: &(impl Fn() -> HlsCacheFillControl + Send + Sync),
) -> Result<(), HlsCacheError> {
    match control() {
        HlsCacheFillControl::Continue => Ok(()),
        HlsCacheFillControl::Cancel => Err(HlsCacheError::Cancelled),
        HlsCacheFillControl::Preempt => Err(HlsCacheError::Preempted),
    }
}

async fn acquire_transcoding_permit<F>(
    config: &HlsTranscodingExecutionConfig,
    control: &F,
) -> Result<OwnedSemaphorePermit, HlsCacheError>
where
    F: Fn() -> HlsCacheFillControl + Send + Sync,
{
    check_fill_control(control)?;
    let permit = Arc::clone(&config.permits).acquire_owned();
    tokio::pin!(permit);
    loop {
        check_fill_control(control)?;
        let result = tokio::select! {
            permit = &mut permit => permit,
            () = tokio::time::sleep(Duration::from_millis(100)) => {
                continue;
            }
        };
        return result.map_err(|_| {
            HlsCacheError::InvalidResource(
                "LAN transcoding worker limiter is unavailable".to_owned(),
            )
        });
    }
}

struct ActiveTranscodingJob {
    active_job_count: Arc<AtomicUsize>,
}

impl ActiveTranscodingJob {
    fn start(active_job_count: Arc<AtomicUsize>) -> Self {
        active_job_count.fetch_add(1, Ordering::SeqCst);
        Self { active_job_count }
    }
}

impl Drop for ActiveTranscodingJob {
    fn drop(&mut self) {
        self.active_job_count.fetch_sub(1, Ordering::SeqCst);
    }
}

fn hls_cache_error_from_transcoding(error: LanTranscodingError) -> HlsCacheError {
    match error {
        LanTranscodingError::Cancelled => HlsCacheError::Cancelled,
        LanTranscodingError::Preempted => HlsCacheError::Preempted,
        LanTranscodingError::Io(error) => HlsCacheError::Io(error),
        LanTranscodingError::Failed { .. } => HlsCacheError::InvalidResource(error.to_string()),
    }
}

fn transcoded_completed_session(
    session: &HlsPlaybackSession,
    output_size: u64,
) -> HlsPlaybackSession {
    let mut completed = session.clone();
    let mut hidden_lookup_variants = vec![session.variant.clone()];
    hidden_lookup_variants.extend(session.alternate_variants.clone());
    let had_audio = session.variant.audio.is_some();
    let codecs = transcoded_codecs(had_audio);
    let cache_key = transcoded_cache_key(session, &codecs);
    let source_video = &session.variant.video.request;
    let (width, height) = transcoded_dimensions(session.variant.width, session.variant.height);
    let frame_rate = transcoded_frame_rate(source_video.frame_rate.as_deref());
    let bandwidth = transcoded_bandwidth(had_audio);
    let resource = HlsMediaResource {
        id: HLS_TRANSCODED_RESOURCE_ID.to_owned(),
        request: BilibiliMediaRequest {
            kind: BilibiliMediaRequestKind::Video,
            stream_id: source_video.stream_id,
            url: String::new(),
            backup_urls: Vec::new(),
            headers: Vec::new(),
            mime_type: Some("video/mp4".to_owned()),
            codecs: Some(codecs.join(",")),
            bandwidth: Some(bandwidth),
            width,
            height,
            frame_rate: frame_rate.clone(),
            size: Some(output_size),
            duration_seconds: Some(session.variant.duration_seconds),
            cache_key: cache_key.clone(),
        },
    };
    completed.variant = HlsVariant {
        id: session.variant.id.clone(),
        bandwidth,
        codecs: codecs.clone(),
        width,
        height,
        duration_seconds: session.variant.duration_seconds,
        video: resource,
        audio: None,
    };
    completed.alternate_variants = hidden_lookup_variants;
    completed.advertise_alternate_variants = false;
    completed.abr = HlsAbrMetadata::default();
    completed.variants = vec![HlsVariantMetadata {
        id: completed.variant.id.clone(),
        kind: BilibiliPlaybackVariantKind::Dash,
        content_id: source_video.cache_key.content_id.clone(),
        bandwidth: Some(bandwidth),
        codecs: codecs.clone(),
        mime_types: vec!["video/mp4".to_owned()],
        width,
        height,
        frame_rate: frame_rate.clone(),
        duration_seconds: Some(completed.variant.duration_seconds),
        abr: None,
        media: vec![HlsMediaResourceMetadata {
            kind: BilibiliMediaRequestKind::Video,
            stream_id: source_video.stream_id,
            mime_type: Some("video/mp4".to_owned()),
            codecs: Some(codecs.join(",")),
            bandwidth: Some(bandwidth),
            width,
            height,
            frame_rate,
            size: Some(output_size),
            duration_seconds: Some(completed.variant.duration_seconds),
            cache_key,
        }],
    }];
    completed.transcoding = HlsTranscodingPlan::with_state(
        HlsTranscodingPlanState::NotRequired,
        session.variant.id.clone(),
        "LAN transcoding completed; serving generated AVPlayer-compatible HLS/fMP4 output.",
    );
    completed
}

fn transcoded_dimensions(width: Option<u32>, height: Option<u32>) -> (Option<u32>, Option<u32>) {
    match (width, height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => {
            let width_scale = f64::from(LAN_TRANSCODING_MAX_WIDTH) / f64::from(width);
            let height_scale = f64::from(LAN_TRANSCODING_MAX_HEIGHT) / f64::from(height);
            let scale = width_scale.min(height_scale).min(1.0);
            (
                Some(round_to_even_dimension(
                    f64::from(width) * scale,
                    width.min(LAN_TRANSCODING_MAX_WIDTH),
                )),
                Some(round_to_even_dimension(
                    f64::from(height) * scale,
                    height.min(LAN_TRANSCODING_MAX_HEIGHT),
                )),
            )
        }
        (width, height) => (
            width.map(|width| {
                let bound = width.min(LAN_TRANSCODING_MAX_WIDTH);
                round_to_even_dimension(f64::from(bound), bound)
            }),
            height.map(|height| {
                let bound = height.min(LAN_TRANSCODING_MAX_HEIGHT);
                round_to_even_dimension(f64::from(bound), bound)
            }),
        ),
    }
}

fn round_to_even_dimension(value: f64, bound: u32) -> u32 {
    let rounded = ((value / 2.0).round() as u32).saturating_mul(2);
    let max_even = if bound.is_multiple_of(2) {
        bound
    } else {
        bound.saturating_sub(1)
    };
    rounded.min(max_even).max(2)
}

fn transcoded_frame_rate(frame_rate: Option<&str>) -> Option<String> {
    let frame_rate = frame_rate?.trim();
    if frame_rate.is_empty() {
        return None;
    }
    let parsed = parse_frame_rate(frame_rate)?;
    if parsed > LAN_TRANSCODING_MAX_FRAME_RATE {
        return Some(LAN_TRANSCODING_MAX_FRAME_RATE.to_string());
    }
    Some(frame_rate.to_owned())
}

fn parse_frame_rate(frame_rate: &str) -> Option<f64> {
    if let Some((numerator, denominator)) = frame_rate.split_once('/') {
        let numerator = numerator.trim().parse::<f64>().ok()?;
        let denominator = denominator.trim().parse::<f64>().ok()?;
        if denominator <= 0.0 {
            return None;
        }
        let parsed = numerator / denominator;
        return parsed.is_finite().then_some(parsed);
    }
    frame_rate
        .parse::<f64>()
        .ok()
        .filter(|rate| rate.is_finite())
}

fn transcoded_bandwidth(had_audio: bool) -> u64 {
    LAN_TRANSCODING_MAX_VIDEO_BANDWIDTH_BPS
        + if had_audio {
            LAN_TRANSCODING_AUDIO_BANDWIDTH_BPS
        } else {
            0
        }
}

fn transcoded_codecs(had_audio: bool) -> Vec<String> {
    let mut codecs = vec![HLS_TRANSCODED_VIDEO_CODEC.to_owned()];
    if had_audio {
        codecs.push(HLS_TRANSCODED_AUDIO_CODEC.to_owned());
    }
    codecs
}

fn transcoded_cache_key(session: &HlsPlaybackSession, codecs: &[String]) -> BilibiliMediaCacheKey {
    let source_video = &session.variant.video.request.cache_key;
    let audio_hash = session
        .variant
        .audio
        .as_ref()
        .map(|audio| audio.request.cache_key.source_hash.as_str())
        .unwrap_or("no-audio");
    BilibiliMediaCacheKey {
        content_id: source_video.content_id.clone(),
        media_kind: BilibiliMediaRequestKind::Video,
        stream_id: source_video.stream_id,
        codecs: Some(codecs.join(",")),
        source_hash: format!(
            "lan-transcoded:{}:{}:{}",
            session.transcoding.profile_id, source_video.source_hash, audio_hash
        ),
    }
}

fn response_candidate_url(request: &BilibiliMediaRequest, final_url: &str) -> Option<String> {
    let final_origin = reqwest::Url::parse(final_url).ok()?.origin();
    std::iter::once(request.url.as_str())
        .chain(request.backup_urls.iter().map(String::as_str))
        .find(|candidate| {
            reqwest::Url::parse(candidate)
                .ok()
                .is_some_and(|candidate| candidate.origin() == final_origin)
        })
        .map(str::to_owned)
}

fn record_response_observation(
    history: &CdnHistory,
    resource: &HlsMediaResource,
    final_url: &str,
    observation: CdnObservation,
) {
    let Some(url) = response_candidate_url(&resource.request, final_url) else {
        return;
    };
    history.record_request(&resource.request, &url, observation);
}

fn passive_cache_observation(
    outcome: CdnObservationOutcome,
    bytes: u64,
    elapsed: Option<Duration>,
    first_byte_latency: Option<Duration>,
    range_supported: Option<bool>,
) -> CdnObservation {
    CdnObservation {
        source: CdnObservationSource::Playback,
        outcome,
        bytes,
        elapsed,
        first_byte_latency,
        range_supported,
    }
}

fn cache_outcome_for_status(status: StatusCode) -> CdnObservationOutcome {
    if status.is_server_error() {
        CdnObservationOutcome::ServerFailure
    } else if status == StatusCode::REQUEST_TIMEOUT || status == StatusCode::GATEWAY_TIMEOUT {
        CdnObservationOutcome::Timeout
    } else {
        CdnObservationOutcome::SourceUnavailable
    }
}

fn cache_outcome_for_transport(error: &reqwest::Error) -> CdnObservationOutcome {
    if error.is_timeout() {
        CdnObservationOutcome::Timeout
    } else if error.is_connect() {
        CdnObservationOutcome::ConnectionFailure
    } else if error.is_body() || error.is_decode() {
        CdnObservationOutcome::IntegrityMismatch
    } else {
        CdnObservationOutcome::SourceUnavailable
    }
}

fn completed_variant_audio_codec(variant: &HlsVariant) -> String {
    variant
        .audio
        .as_ref()
        .and_then(|audio| audio.request.codecs.clone())
        .or_else(|| {
            variant
                .codecs
                .iter()
                .find(|codec| codec.trim().starts_with("mp4a."))
                .cloned()
        })
        .unwrap_or_default()
}

pub(crate) fn hls_session_declared_size_bytes(session: &HlsPlaybackSession) -> Option<u64> {
    let mut total = 0_u64;
    for resource in session
        .variant
        .audio
        .iter()
        .chain(std::iter::once(&session.variant.video))
    {
        total = total.checked_add(resource.request.size?)?;
    }
    Some(total)
}

pub(crate) fn sanitized_completed_session(session: &HlsPlaybackSession) -> HlsPlaybackSession {
    let mut session = completed_runtime_session(session);
    session.accepted_identity = None;
    sanitize_completed_variant(&mut session.variant);
    for variant in &mut session.alternate_variants {
        sanitize_completed_variant(variant);
    }
    session
}

fn invalid_session_publication(stage: &'static str) -> io::Error {
    eprintln!("HLS session publication rejected at {stage}");
    io::Error::new(
        io::ErrorKind::InvalidData,
        "HLS session publication does not match its authorized cache identity",
    )
}

fn invalid_completed_resource_data() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "completed HLS cache resource failed binding validation",
    )
}

fn session_refresh_matches(existing: &HlsPlaybackSession, incoming: &HlsPlaybackSession) -> bool {
    session_refresh_mismatch_stage(existing, incoming).is_none()
}

fn session_refresh_mismatch_stage(
    existing: &HlsPlaybackSession,
    incoming: &HlsPlaybackSession,
) -> Option<&'static str> {
    if existing.id != incoming.id {
        return Some("session-id");
    }
    if existing.effective_policy != incoming.effective_policy {
        return Some("authorization-policy");
    }
    let mut existing = normalized_session_bindings(existing);
    let mut incoming = normalized_session_bindings(incoming);
    clear_session_request_headers(&mut existing);
    clear_session_request_headers(&mut incoming);
    if existing.variant != incoming.variant {
        return Some(
            variant_binding_mismatch_stage(&existing.variant, &incoming.variant)
                .unwrap_or("selected-resource"),
        );
    }
    if existing.alternate_variants != incoming.alternate_variants {
        return Some("alternate-resources");
    }
    if existing.abr != incoming.abr {
        return Some("abr-bindings");
    }
    if existing.variants != incoming.variants {
        return Some("variant-metadata");
    }
    if existing.transcoding != incoming.transcoding {
        return Some("transcoding-plan");
    }
    (existing != incoming).then_some("other-session-binding")
}

fn variant_binding_mismatch_stage(left: &HlsVariant, right: &HlsVariant) -> Option<&'static str> {
    if left.id != right.id {
        return Some("selected-variant-id");
    }
    if left.bandwidth != right.bandwidth
        || left.codecs != right.codecs
        || left.width != right.width
        || left.height != right.height
        || left.duration_seconds != right.duration_seconds
    {
        return Some("selected-variant-properties");
    }
    if let Some(stage) = media_resource_binding_mismatch_stage(&left.video, &right.video) {
        return Some(stage);
    }
    match (&left.audio, &right.audio) {
        (Some(left), Some(right)) => media_resource_binding_mismatch_stage(left, right),
        (None, None) => None,
        _ => Some("selected-audio-presence"),
    }
}

fn media_resource_binding_mismatch_stage(
    left: &HlsMediaResource,
    right: &HlsMediaResource,
) -> Option<&'static str> {
    if left.id != right.id {
        return Some("resource-id");
    }
    if left.request.cache_key != right.request.cache_key {
        return Some("resource-cache-key");
    }
    if left.request.kind != right.request.kind
        || left.request.stream_id != right.request.stream_id
        || left.request.mime_type != right.request.mime_type
        || left.request.codecs != right.request.codecs
        || left.request.bandwidth != right.request.bandwidth
        || left.request.width != right.request.width
        || left.request.height != right.request.height
        || left.request.frame_rate != right.request.frame_rate
        || left.request.size != right.request.size
        || left.request.duration_seconds != right.request.duration_seconds
        || left.request.headers != right.request.headers
    {
        return Some("resource-properties-or-auth-headers");
    }
    None
}

fn completed_publication_matches(
    store: &HlsCacheStore,
    existing: &HlsPlaybackSession,
    completed: &HlsPlaybackSession,
) -> bool {
    session_completion_matches(existing, completed)
        || store.completed_transcode_matches(existing, completed)
}

fn session_completion_matches(
    existing: &HlsPlaybackSession,
    completed: &HlsPlaybackSession,
) -> bool {
    if existing.id != completed.id || existing.effective_policy != completed.effective_policy {
        return false;
    }
    let mut existing = normalized_session_bindings(existing);
    let mut completed = normalized_session_bindings(completed);
    existing.accepted_identity = None;
    completed.accepted_identity = None;
    clear_session_request_headers(&mut existing);
    clear_session_request_headers(&mut completed);
    existing == completed
}

fn clear_session_request_headers(session: &mut HlsPlaybackSession) {
    for variant in std::iter::once(&mut session.variant).chain(&mut session.alternate_variants) {
        variant.video.request.headers.clear();
        if let Some(audio) = variant.audio.as_mut() {
            audio.request.headers.clear();
        }
    }
}

fn normalized_session_bindings(session: &HlsPlaybackSession) -> HlsPlaybackSession {
    let mut normalized = session.clone();
    normalized.title.clear();
    normalized.advertise_alternate_variants = false;
    normalize_variant_bindings(&mut normalized.variant);
    for variant in &mut normalized.alternate_variants {
        normalize_variant_bindings(variant);
    }
    normalized
        .alternate_variants
        .sort_by(|left, right| left.id.cmp(&right.id));
    normalized
}

fn preserve_persisted_request_candidates(
    existing: &HlsPlaybackSession,
    incoming: &HlsPlaybackSession,
) -> HlsPlaybackSession {
    let mut preserved = incoming.clone();
    preserve_variant_request_candidates(&mut preserved.variant, &existing.variant);
    for variant in &mut preserved.alternate_variants {
        if let Some(existing_variant) = existing
            .alternate_variants
            .iter()
            .find(|existing_variant| existing_variant.id == variant.id)
        {
            preserve_variant_request_candidates(variant, existing_variant);
        }
    }
    preserved
}

fn preserve_variant_request_candidates(incoming: &mut HlsVariant, existing: &HlsVariant) {
    preserve_request_candidates(&mut incoming.video.request, &existing.video.request);
    if let (Some(incoming), Some(existing)) = (incoming.audio.as_mut(), existing.audio.as_ref()) {
        preserve_request_candidates(&mut incoming.request, &existing.request);
    }
}

fn preserve_request_candidates(
    incoming: &mut BilibiliMediaRequest,
    existing: &BilibiliMediaRequest,
) {
    incoming.url.clone_from(&existing.url);
    incoming.backup_urls.clone_from(&existing.backup_urls);
    incoming.headers.clone_from(&existing.headers);
}

fn normalize_variant_bindings(variant: &mut HlsVariant) {
    normalize_resource_binding(&mut variant.video);
    if let Some(audio) = variant.audio.as_mut() {
        normalize_resource_binding(audio);
    }
}

fn normalize_resource_binding(resource: &mut HlsMediaResource) {
    resource.request.url.clear();
    resource.request.backup_urls.clear();
    for header in &mut resource.request.headers {
        header.name.make_ascii_lowercase();
        header.value.clear();
    }
    resource
        .request
        .headers
        .sort_by(|left, right| left.name.cmp(&right.name));
}

fn variants_preserve_resource_bindings(left: &HlsVariant, right: &HlsVariant) -> bool {
    left.id == right.id
        && resource_bindings_match_after_header_scrub(&left.video, &right.video)
        && match (&left.audio, &right.audio) {
            (Some(left), Some(right)) => resource_bindings_match_after_header_scrub(left, right),
            (None, None) => true,
            _ => false,
        }
}

fn resource_bindings_match_after_header_scrub(
    left: &HlsMediaResource,
    right: &HlsMediaResource,
) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    left.request.headers.clear();
    right.request.headers.clear();
    media_resource_binding_mismatch_stage(&left, &right).is_none()
}

pub(crate) fn source_completed_session_for_restore(
    session: &HlsPlaybackSession,
) -> HlsPlaybackSession {
    let mut session = session.clone();
    if session.transcoding.state == HlsTranscodingPlanState::Ready {
        session.transcoding = HlsTranscodingPlan::with_state(
            HlsTranscodingPlanState::Disabled,
            session.transcoding.source_variant_id.clone(),
            "LAN transcoding was unavailable during restore; serving the completed source HLS cache.",
        );
    }
    session
}

fn source_restore_completion_matches(
    store: &HlsCacheStore,
    existing: &HlsPlaybackSession,
    incoming: &HlsPlaybackSession,
) -> bool {
    if existing.id != incoming.id
        || existing.transcoding.state != HlsTranscodingPlanState::Ready
        || !store.source_session_resources_are_complete(existing)
    {
        return false;
    }
    sanitized_completed_session(&source_completed_session_for_restore(existing)) == *incoming
}

pub(crate) fn completed_runtime_session(session: &HlsPlaybackSession) -> HlsPlaybackSession {
    let mut session = session.clone();
    session.advertise_alternate_variants = false;
    sanitize_completed_variant(&mut session.variant);
    session
}

fn sanitize_completed_variant(variant: &mut HlsVariant) {
    sanitize_completed_resource(&mut variant.video);
    if let Some(audio) = variant.audio.as_mut() {
        sanitize_completed_resource(audio);
    }
}

fn sanitize_completed_resource(resource: &mut HlsMediaResource) {
    resource.request.url.clear();
    resource.request.backup_urls.clear();
    resource.request.headers.clear();
}

fn session_id_from_library_item_id(item_id: &str) -> Option<String> {
    item_id
        .strip_prefix(HLS_LIBRARY_ITEM_PREFIX)
        .map(str::to_owned)
        .filter(|session_id| validate_cache_id(session_id).is_ok())
}

fn validate_cache_id(value: &str) -> io::Result<()> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid HLS cache identifier",
        ));
    }

    Ok(())
}

fn invalid_data(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn non_empty_or(value: String, fallback: String) -> String {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

fn cache_path_contains_symlink(root_path: &Path, candidate_path: &Path) -> io::Result<bool> {
    let root_path = absolute_path(root_path);
    let candidate_path = absolute_path(candidate_path);
    if !is_within_root(&root_path, &candidate_path) {
        return Ok(true);
    }

    if path_contains_symlink_component(&root_path)? {
        return Ok(true);
    }

    let Ok(relative_path) = candidate_path.strip_prefix(&root_path) else {
        return Ok(true);
    };
    let mut current_path = root_path;
    for component in relative_path.components() {
        current_path.push(component.as_os_str());
        match fs::symlink_metadata(&current_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
    }

    Ok(false)
}

fn path_contains_symlink_component(path: &Path) -> io::Result<bool> {
    let mut current_path = PathBuf::new();
    for component in absolute_path(path).components() {
        current_path.push(component.as_os_str());
        if matches!(
            component,
            std::path::Component::Prefix(_) | std::path::Component::RootDir
        ) {
            continue;
        }
        if path_is_symlink(&current_path)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn path_is_symlink(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_symlink()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn is_within_root(root_path: &Path, candidate_path: &Path) -> bool {
    candidate_path == root_path || candidate_path.starts_with(root_path)
}

fn created_time_for_path(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.created().ok()
}

pub(crate) fn timestamp_from_system_time(time: SystemTime) -> Timestamp {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    Timestamp {
        seconds: duration.as_secs().try_into().unwrap_or(i64::MAX),
        nanos: duration.subsec_nanos().try_into().unwrap_or(i32::MAX),
    }
}

fn percentage_bytes(bytes: u64, percent: u8) -> u64 {
    let value = u128::from(bytes) * u128::from(percent) / 100;
    value.try_into().unwrap_or(u64::MAX)
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedHlsSession {
    schema_version: u32,
    id: String,
    title: String,
    #[serde(default)]
    accepted_identity: Option<crate::bilibili_playback::BilibiliContentIdentity>,
    variant: PersistedHlsVariant,
    #[serde(default)]
    alternate_variants: Vec<PersistedHlsVariant>,
    #[serde(default = "default_advertise_alternate_variants")]
    advertise_alternate_variants: bool,
    #[serde(default)]
    abr: PersistedHlsAbrMetadata,
    #[serde(default)]
    variants: Vec<PersistedHlsVariantMetadata>,
    #[serde(default)]
    transcoding: PersistedHlsTranscodingPlan,
    #[serde(default)]
    effective_policy: PlaybackPolicy,
}

impl From<HlsPlaybackSession> for PersistedHlsSession {
    fn from(session: HlsPlaybackSession) -> Self {
        Self {
            schema_version: HLS_CACHE_SCHEMA_VERSION,
            id: session.id,
            title: session.title,
            accepted_identity: session.accepted_identity,
            variant: PersistedHlsVariant::from(session.variant),
            alternate_variants: session
                .alternate_variants
                .into_iter()
                .map(PersistedHlsVariant::from)
                .collect(),
            advertise_alternate_variants: session.advertise_alternate_variants,
            abr: PersistedHlsAbrMetadata::from(session.abr),
            variants: session
                .variants
                .into_iter()
                .map(PersistedHlsVariantMetadata::from)
                .collect(),
            transcoding: PersistedHlsTranscodingPlan::from(session.transcoding),
            effective_policy: session.effective_policy,
        }
    }
}

impl TryFrom<PersistedHlsSession> for HlsPlaybackSession {
    type Error = ();

    fn try_from(session: PersistedHlsSession) -> Result<Self, Self::Error> {
        validate_cache_id(&session.id).map_err(|_| ())?;
        Ok(Self {
            id: session.id,
            title: session.title,
            accepted_identity: session.accepted_identity,
            variant: HlsVariant::try_from(session.variant)?,
            alternate_variants: session
                .alternate_variants
                .into_iter()
                .map(HlsVariant::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            advertise_alternate_variants: session.advertise_alternate_variants,
            abr: HlsAbrMetadata::from(session.abr),
            variants: session
                .variants
                .into_iter()
                .map(HlsVariantMetadata::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            transcoding: HlsTranscodingPlan::from(session.transcoding),
            effective_policy: session.effective_policy,
        })
    }
}

fn default_advertise_alternate_variants() -> bool {
    true
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct PersistedHlsTranscodingPlan {
    #[serde(default)]
    state: PersistedHlsTranscodingPlanState,
    #[serde(default)]
    profile_id: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    source_variant_id: String,
    #[serde(default)]
    target_container: String,
    #[serde(default)]
    target_video_codec: String,
    #[serde(default)]
    target_audio_codec: String,
    #[serde(default)]
    output_protocol: String,
}

impl From<HlsTranscodingPlan> for PersistedHlsTranscodingPlan {
    fn from(plan: HlsTranscodingPlan) -> Self {
        Self {
            state: PersistedHlsTranscodingPlanState::from(plan.state),
            profile_id: plan.profile_id,
            reason: plan.reason,
            source_variant_id: plan.source_variant_id,
            target_container: plan.target_container,
            target_video_codec: plan.target_video_codec,
            target_audio_codec: plan.target_audio_codec,
            output_protocol: plan.output_protocol,
        }
    }
}

impl From<PersistedHlsTranscodingPlan> for HlsTranscodingPlan {
    fn from(plan: PersistedHlsTranscodingPlan) -> Self {
        let defaults = HlsTranscodingPlan::default();
        Self {
            state: HlsTranscodingPlanState::from(plan.state),
            profile_id: non_empty_or(plan.profile_id, defaults.profile_id),
            reason: non_empty_or(plan.reason, defaults.reason),
            source_variant_id: plan.source_variant_id,
            target_container: non_empty_or(plan.target_container, defaults.target_container),
            target_video_codec: non_empty_or(plan.target_video_codec, defaults.target_video_codec),
            target_audio_codec: non_empty_or(plan.target_audio_codec, defaults.target_audio_codec),
            output_protocol: non_empty_or(plan.output_protocol, defaults.output_protocol),
        }
    }
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedHlsTranscodingPlanState {
    #[default]
    Disabled,
    NotRequired,
    Ready,
    Unsupported,
}

impl From<HlsTranscodingPlanState> for PersistedHlsTranscodingPlanState {
    fn from(state: HlsTranscodingPlanState) -> Self {
        match state {
            HlsTranscodingPlanState::Disabled => Self::Disabled,
            HlsTranscodingPlanState::NotRequired => Self::NotRequired,
            HlsTranscodingPlanState::Ready => Self::Ready,
            HlsTranscodingPlanState::Unsupported => Self::Unsupported,
        }
    }
}

impl From<PersistedHlsTranscodingPlanState> for HlsTranscodingPlanState {
    fn from(state: PersistedHlsTranscodingPlanState) -> Self {
        match state {
            PersistedHlsTranscodingPlanState::Disabled => Self::Disabled,
            PersistedHlsTranscodingPlanState::NotRequired => Self::NotRequired,
            PersistedHlsTranscodingPlanState::Ready => Self::Ready,
            PersistedHlsTranscodingPlanState::Unsupported => Self::Unsupported,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedHlsVariant {
    id: String,
    bandwidth: u64,
    #[serde(default)]
    codecs: Vec<String>,
    width: Option<u32>,
    height: Option<u32>,
    duration_seconds: u32,
    video: PersistedHlsMediaResource,
    audio: Option<PersistedHlsMediaResource>,
}

impl From<HlsVariant> for PersistedHlsVariant {
    fn from(variant: HlsVariant) -> Self {
        Self {
            id: variant.id,
            bandwidth: variant.bandwidth,
            codecs: variant.codecs,
            width: variant.width,
            height: variant.height,
            duration_seconds: variant.duration_seconds,
            video: PersistedHlsMediaResource::from(variant.video),
            audio: variant.audio.map(PersistedHlsMediaResource::from),
        }
    }
}

impl TryFrom<PersistedHlsVariant> for HlsVariant {
    type Error = ();

    fn try_from(variant: PersistedHlsVariant) -> Result<Self, Self::Error> {
        Ok(Self {
            id: variant.id,
            bandwidth: variant.bandwidth,
            codecs: variant.codecs,
            width: variant.width,
            height: variant.height,
            duration_seconds: variant.duration_seconds,
            video: HlsMediaResource::try_from(variant.video)?,
            audio: variant.audio.map(HlsMediaResource::try_from).transpose()?,
        })
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct PersistedHlsAbrMetadata {
    #[serde(default)]
    groups: Vec<PersistedHlsAbrGroup>,
}

impl From<HlsAbrMetadata> for PersistedHlsAbrMetadata {
    fn from(metadata: HlsAbrMetadata) -> Self {
        Self {
            groups: metadata
                .groups
                .into_iter()
                .map(PersistedHlsAbrGroup::from)
                .collect(),
        }
    }
}

impl From<PersistedHlsAbrMetadata> for HlsAbrMetadata {
    fn from(metadata: PersistedHlsAbrMetadata) -> Self {
        Self {
            groups: metadata.groups.into_iter().map(HlsAbrGroup::from).collect(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedHlsAbrGroup {
    id: String,
    kind: PersistedHlsAbrGroupKind,
    #[serde(default)]
    variant_ids: Vec<String>,
    level_count: u32,
    min_bandwidth: Option<u64>,
    max_bandwidth: Option<u64>,
}

impl From<HlsAbrGroup> for PersistedHlsAbrGroup {
    fn from(group: HlsAbrGroup) -> Self {
        Self {
            id: group.id,
            kind: PersistedHlsAbrGroupKind::from(group.kind),
            variant_ids: group.variant_ids,
            level_count: group.level_count,
            min_bandwidth: group.min_bandwidth,
            max_bandwidth: group.max_bandwidth,
        }
    }
}

impl From<PersistedHlsAbrGroup> for HlsAbrGroup {
    fn from(group: PersistedHlsAbrGroup) -> Self {
        Self {
            id: group.id,
            kind: HlsAbrGroupKind::from(group.kind),
            variant_ids: group.variant_ids,
            level_count: group.level_count,
            min_bandwidth: group.min_bandwidth,
            max_bandwidth: group.max_bandwidth,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedHlsAbrGroupKind {
    DashVideo,
    DashAudioOnly,
}

impl From<HlsAbrGroupKind> for PersistedHlsAbrGroupKind {
    fn from(kind: HlsAbrGroupKind) -> Self {
        match kind {
            HlsAbrGroupKind::DashVideo => Self::DashVideo,
            HlsAbrGroupKind::DashAudioOnly => Self::DashAudioOnly,
        }
    }
}

impl From<PersistedHlsAbrGroupKind> for HlsAbrGroupKind {
    fn from(kind: PersistedHlsAbrGroupKind) -> Self {
        match kind {
            PersistedHlsAbrGroupKind::DashVideo => Self::DashVideo,
            PersistedHlsAbrGroupKind::DashAudioOnly => Self::DashAudioOnly,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedHlsVariantMetadata {
    id: String,
    kind: PersistedHlsVariantKind,
    content_id: String,
    bandwidth: Option<u64>,
    #[serde(default)]
    codecs: Vec<String>,
    #[serde(default)]
    mime_types: Vec<String>,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<String>,
    duration_seconds: Option<u32>,
    abr: Option<PersistedHlsAbrLevel>,
    #[serde(default)]
    media: Vec<PersistedHlsMediaResourceMetadata>,
}

impl From<HlsVariantMetadata> for PersistedHlsVariantMetadata {
    fn from(variant: HlsVariantMetadata) -> Self {
        Self {
            id: variant.id,
            kind: PersistedHlsVariantKind::from(variant.kind),
            content_id: variant.content_id,
            bandwidth: variant.bandwidth,
            codecs: variant.codecs,
            mime_types: variant.mime_types,
            width: variant.width,
            height: variant.height,
            frame_rate: variant.frame_rate,
            duration_seconds: variant.duration_seconds,
            abr: variant.abr.map(PersistedHlsAbrLevel::from),
            media: variant
                .media
                .into_iter()
                .map(PersistedHlsMediaResourceMetadata::from)
                .collect(),
        }
    }
}

impl TryFrom<PersistedHlsVariantMetadata> for HlsVariantMetadata {
    type Error = ();

    fn try_from(variant: PersistedHlsVariantMetadata) -> Result<Self, Self::Error> {
        Ok(Self {
            id: variant.id,
            kind: BilibiliPlaybackVariantKind::from(variant.kind),
            content_id: variant.content_id,
            bandwidth: variant.bandwidth,
            codecs: variant.codecs,
            mime_types: variant.mime_types,
            width: variant.width,
            height: variant.height,
            frame_rate: variant.frame_rate,
            duration_seconds: variant.duration_seconds,
            abr: variant.abr.map(HlsAbrLevel::from),
            media: variant
                .media
                .into_iter()
                .map(HlsMediaResourceMetadata::from)
                .collect(),
        })
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedHlsVariantKind {
    Dash,
    Flv,
}

impl From<BilibiliPlaybackVariantKind> for PersistedHlsVariantKind {
    fn from(kind: BilibiliPlaybackVariantKind) -> Self {
        match kind {
            BilibiliPlaybackVariantKind::Dash => Self::Dash,
            BilibiliPlaybackVariantKind::Flv => Self::Flv,
        }
    }
}

impl From<PersistedHlsVariantKind> for BilibiliPlaybackVariantKind {
    fn from(kind: PersistedHlsVariantKind) -> Self {
        match kind {
            PersistedHlsVariantKind::Dash => Self::Dash,
            PersistedHlsVariantKind::Flv => Self::Flv,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedHlsAbrLevel {
    group_id: String,
    level_index: u32,
    level_count: u32,
    switchable: bool,
}

impl From<HlsAbrLevel> for PersistedHlsAbrLevel {
    fn from(level: HlsAbrLevel) -> Self {
        Self {
            group_id: level.group_id,
            level_index: level.level_index,
            level_count: level.level_count,
            switchable: level.switchable,
        }
    }
}

impl From<PersistedHlsAbrLevel> for HlsAbrLevel {
    fn from(level: PersistedHlsAbrLevel) -> Self {
        Self {
            group_id: level.group_id,
            level_index: level.level_index,
            level_count: level.level_count,
            switchable: level.switchable,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedHlsMediaResourceMetadata {
    kind: PersistedBilibiliMediaRequestKind,
    stream_id: Option<u32>,
    mime_type: Option<String>,
    codecs: Option<String>,
    bandwidth: Option<u64>,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<String>,
    size: Option<u64>,
    duration_seconds: Option<u32>,
    cache_key: PersistedBilibiliMediaCacheKey,
}

impl From<HlsMediaResourceMetadata> for PersistedHlsMediaResourceMetadata {
    fn from(resource: HlsMediaResourceMetadata) -> Self {
        Self {
            kind: PersistedBilibiliMediaRequestKind::from(resource.kind),
            stream_id: resource.stream_id,
            mime_type: resource.mime_type,
            codecs: resource.codecs,
            bandwidth: resource.bandwidth,
            width: resource.width,
            height: resource.height,
            frame_rate: resource.frame_rate,
            size: resource.size,
            duration_seconds: resource.duration_seconds,
            cache_key: PersistedBilibiliMediaCacheKey::from(resource.cache_key),
        }
    }
}

impl From<PersistedHlsMediaResourceMetadata> for HlsMediaResourceMetadata {
    fn from(resource: PersistedHlsMediaResourceMetadata) -> Self {
        Self {
            kind: BilibiliMediaRequestKind::from(resource.kind),
            stream_id: resource.stream_id,
            mime_type: resource.mime_type,
            codecs: resource.codecs,
            bandwidth: resource.bandwidth,
            width: resource.width,
            height: resource.height,
            frame_rate: resource.frame_rate,
            size: resource.size,
            duration_seconds: resource.duration_seconds,
            cache_key: BilibiliMediaCacheKey::from(resource.cache_key),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedHlsMediaResource {
    id: String,
    request: PersistedBilibiliMediaRequest,
}

impl From<HlsMediaResource> for PersistedHlsMediaResource {
    fn from(resource: HlsMediaResource) -> Self {
        Self {
            id: resource.id,
            request: PersistedBilibiliMediaRequest::from(resource.request),
        }
    }
}

impl TryFrom<PersistedHlsMediaResource> for HlsMediaResource {
    type Error = ();

    fn try_from(resource: PersistedHlsMediaResource) -> Result<Self, Self::Error> {
        validate_cache_id(&resource.id).map_err(|_| ())?;
        Ok(Self {
            id: resource.id,
            request: BilibiliMediaRequest::from(resource.request),
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedHlsCachedResource {
    schema_version: u32,
    id: String,
    content_type: String,
    total_length: u64,
    initialization_length: u64,
    #[serde(default)]
    segments: Vec<PersistedHlsMediaSegment>,
    cache_key: PersistedBilibiliMediaCacheKey,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedHlsMediaSegment {
    byte_range_offset: u64,
    byte_range_length: u64,
    #[serde(default)]
    duration_millis: u64,
}

impl From<HlsMediaSegment> for PersistedHlsMediaSegment {
    fn from(segment: HlsMediaSegment) -> Self {
        Self {
            byte_range_offset: segment.byte_range_offset,
            byte_range_length: segment.byte_range_length,
            duration_millis: segment.duration_millis,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedHlsPrewarmedResource {
    schema_version: u32,
    id: String,
    content_type: String,
    prefix_length: u64,
    #[serde(default)]
    target_prefix_length: Option<u64>,
    #[serde(default)]
    target_window_seconds: Option<u64>,
    total_length: u64,
    initialization_length: u64,
    cache_key: PersistedBilibiliMediaCacheKey,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliMediaRequest {
    kind: PersistedBilibiliMediaRequestKind,
    stream_id: Option<u32>,
    url: String,
    #[serde(default)]
    backup_urls: Vec<String>,
    #[serde(default)]
    headers: Vec<PersistedBilibiliHttpHeader>,
    mime_type: Option<String>,
    codecs: Option<String>,
    bandwidth: Option<u64>,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<String>,
    size: Option<u64>,
    duration_seconds: Option<u32>,
    cache_key: PersistedBilibiliMediaCacheKey,
}

impl From<BilibiliMediaRequest> for PersistedBilibiliMediaRequest {
    fn from(request: BilibiliMediaRequest) -> Self {
        Self {
            kind: PersistedBilibiliMediaRequestKind::from(request.kind),
            stream_id: request.stream_id,
            url: request.url,
            backup_urls: request.backup_urls,
            headers: request
                .headers
                .into_iter()
                .map(PersistedBilibiliHttpHeader::from)
                .collect(),
            mime_type: request.mime_type,
            codecs: request.codecs,
            bandwidth: request.bandwidth,
            width: request.width,
            height: request.height,
            frame_rate: request.frame_rate,
            size: request.size,
            duration_seconds: request.duration_seconds,
            cache_key: PersistedBilibiliMediaCacheKey::from(request.cache_key),
        }
    }
}

impl From<PersistedBilibiliMediaRequest> for BilibiliMediaRequest {
    fn from(request: PersistedBilibiliMediaRequest) -> Self {
        Self {
            kind: BilibiliMediaRequestKind::from(request.kind),
            stream_id: request.stream_id,
            url: request.url,
            backup_urls: request.backup_urls,
            headers: request
                .headers
                .into_iter()
                .map(BilibiliHttpHeader::from)
                .collect(),
            mime_type: request.mime_type,
            codecs: request.codecs,
            bandwidth: request.bandwidth,
            width: request.width,
            height: request.height,
            frame_rate: request.frame_rate,
            size: request.size,
            duration_seconds: request.duration_seconds,
            cache_key: BilibiliMediaCacheKey::from(request.cache_key),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedBilibiliHttpHeader {
    name: String,
    value: String,
}

impl From<BilibiliHttpHeader> for PersistedBilibiliHttpHeader {
    fn from(header: BilibiliHttpHeader) -> Self {
        Self {
            name: header.name,
            value: header.value,
        }
    }
}

impl From<PersistedBilibiliHttpHeader> for BilibiliHttpHeader {
    fn from(header: PersistedBilibiliHttpHeader) -> Self {
        Self {
            name: header.name,
            value: header.value,
        }
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
struct PersistedBilibiliMediaCacheKey {
    content_id: String,
    media_kind: PersistedBilibiliMediaRequestKind,
    stream_id: Option<u32>,
    codecs: Option<String>,
    source_hash: String,
}

impl From<BilibiliMediaCacheKey> for PersistedBilibiliMediaCacheKey {
    fn from(key: BilibiliMediaCacheKey) -> Self {
        Self {
            content_id: key.content_id,
            media_kind: PersistedBilibiliMediaRequestKind::from(key.media_kind),
            stream_id: key.stream_id,
            codecs: key.codecs,
            source_hash: key.source_hash,
        }
    }
}

impl From<PersistedBilibiliMediaCacheKey> for BilibiliMediaCacheKey {
    fn from(key: PersistedBilibiliMediaCacheKey) -> Self {
        Self {
            content_id: key.content_id,
            media_kind: BilibiliMediaRequestKind::from(key.media_kind),
            stream_id: key.stream_id,
            codecs: key.codecs,
            source_hash: key.source_hash,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedBilibiliMediaRequestKind {
    Video,
    Audio,
    FlvSegment,
}

impl From<BilibiliMediaRequestKind> for PersistedBilibiliMediaRequestKind {
    fn from(kind: BilibiliMediaRequestKind) -> Self {
        match kind {
            BilibiliMediaRequestKind::Video => Self::Video,
            BilibiliMediaRequestKind::Audio => Self::Audio,
            BilibiliMediaRequestKind::FlvSegment => Self::FlvSegment,
        }
    }
}

impl From<PersistedBilibiliMediaRequestKind> for BilibiliMediaRequestKind {
    fn from(kind: PersistedBilibiliMediaRequestKind) -> Self {
        match kind {
            PersistedBilibiliMediaRequestKind::Video => Self::Video,
            PersistedBilibiliMediaRequestKind::Audio => Self::Audio,
            PersistedBilibiliMediaRequestKind::FlvSegment => Self::FlvSegment,
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{
            HeaderMap, HeaderValue, Response, StatusCode, header::CONTENT_LENGTH,
            header::CONTENT_TYPE,
        },
        routing::get,
    };
    use tempfile::TempDir;
    use tokio::sync::Notify;

    use crate::{
        hls_playback_progress::PlaybackProgressIntent,
        playback_policy::{
            CompatibleVariantPreference, TranscodingPreference, WeakNetworkPreference,
        },
    };

    use super::*;

    fn temp_store(temp: &TempDir) -> HlsCacheStore {
        HlsCacheStore::new(
            temp.path()
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(temp.path())),
        )
        .with_range_budget(1024 * 1024 * 1024)
    }

    fn extent_rebase_test_manifest() -> (PersistedRangeManifest, String) {
        let target_origin = "https://target.example:443".to_owned();
        let manifest = PersistedRangeManifest {
            schema_version: crate::hls_range_cache::RANGE_MANIFEST_SCHEMA_VERSION,
            generation: 4,
            resource_id: "video-resource".to_owned(),
            representation_digest: "representation-digest".to_owned(),
            data_identity: Some("42:101".to_owned()),
            total_length: Some(64),
            strong_etag: Some("\"target-etag-v1\"".to_owned()),
            validator_origin: Some(target_origin.clone()),
            last_modified: None,
            prefix_length: 8,
            prefix_sha256: Some("startup-prefix-digest".to_owned()),
            durable_bytes: 8,
            validated_origins: vec![
                PersistedRangeOrigin {
                    origin: "https://seed.example:443".to_owned(),
                    prefix_sha256: "seed-prefix-digest".to_owned(),
                    strong_etag: Some("\"seed-etag-v1\"".to_owned()),
                },
                PersistedRangeOrigin {
                    origin: target_origin.clone(),
                    prefix_sha256: "target-prefix-digest".to_owned(),
                    strong_etag: Some("\"target-etag-v1\"".to_owned()),
                },
            ],
            extents: vec![PersistedRangeExtent {
                start: 0,
                end: 8,
                sha256: "startup-extent-digest".to_owned(),
            }],
        };
        (manifest, target_origin)
    }

    #[test]
    fn strong_etag_requires_rfc_entity_tag_syntax() {
        for valid in ["\"opaque\"", "\"\"", "\"!#$%&'()*+,-./:;<=>?@[]^_`{|}~\""] {
            let mut headers = HeaderMap::new();
            headers.insert(reqwest::header::ETAG, HeaderValue::from_static(valid));
            assert!(etag_header_syntax_valid(&headers), "{valid:?}");
            assert_eq!(Some(valid.to_owned()), strong_etag(&headers));
        }

        let mut weak = HeaderMap::new();
        weak.insert(
            reqwest::header::ETAG,
            HeaderValue::from_static("W/\"opaque\""),
        );
        assert!(etag_header_syntax_valid(&weak));
        assert_eq!(None, strong_etag(&weak));

        for invalid in ["opaque", "\"unterminated", "\"bad\"tag\""] {
            let mut headers = HeaderMap::new();
            headers.insert(
                reqwest::header::ETAG,
                HeaderValue::from_bytes(invalid.as_bytes()).expect("header value should be legal"),
            );
            assert!(!etag_header_syntax_valid(&headers), "{invalid:?}");
            assert_eq!(None, strong_etag(&headers));
        }
    }

    async fn seed_range_prefix(
        store: &HlsCacheStore,
        session: &HlsPlaybackSession,
        url: &str,
        body: &[u8],
        prefix_length: u64,
        etag: &str,
    ) -> RangeChunkKey {
        store
            .save_session(session)
            .expect("range session should be saved");
        let resource = &session.variant.video;
        let key = store
            .range_resource_key(&session.id, resource)
            .expect("range key should be valid");
        let loaded = store
            .ensure_range_manifest(&session.id, resource, &key)
            .expect("range manifest should be initialized");
        let loaded = store
            .publish_range_manifest(&session.id, resource, loaded, |manifest| {
                manifest.total_length = Some(body.len() as u64);
            })
            .await
            .expect("range total should be checkpointed");
        store
            .commit_range_extent(
                &session.id,
                resource,
                &loaded.manifest,
                0..prefix_length,
                &body[..prefix_length as usize],
                url,
                Some(etag.to_owned()),
                None,
            )
            .await
            .expect("prefix extent should be durable");
        RangeChunkKey {
            resource: key,
            total_length: body.len() as u64,
            range: prefix_length..body.len() as u64,
        }
    }

    #[tokio::test]
    async fn same_origin_changed_etag_revalidates_samples_and_commits_with_refreshed_manifest() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let body = Arc::new(
            (0..160 * 1024)
                .map(|byte| (byte % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let prefix_length = 16_u64;
        let client = reqwest::Client::new();

        let (url, _task) =
            start_etag_range_upstream(Arc::clone(&body), "\"prefix-v1\"", "\"tail-v2\"", false)
                .await;
        let session = sample_session("etag-resume-publish", &url);
        let mut key = seed_range_prefix(
            &store,
            &session,
            &url,
            &body,
            prefix_length,
            "\"prefix-v1\"",
        )
        .await;
        key.range = prefix_length..32 * 1024;
        let loaded = store
            .load_range_manifest(&session.id, &session.variant.video)
            .expect("prefix checkpoint should validate")
            .expect("prefix checkpoint should exist");
        store
            .commit_range_extent(
                &session.id,
                &session.variant.video,
                &loaded.manifest,
                64 * 1024..128 * 1024,
                &body[64 * 1024..128 * 1024],
                &url,
                Some("\"prefix-v1\"".to_owned()),
                None,
            )
            .await
            .expect("sample should use the currently accepted validator");
        let _activity = store
            .range_cache
            .enter(&session.id, HlsRangePriority::Foreground)
            .expect("foreground activity should be admitted");
        store
            .ensure_range_span(
                &client,
                &session.id,
                &session.variant.video,
                &key.resource,
                key.range.clone(),
                0,
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
                &|_| {},
            )
            .await
            .expect("verified tag update should publish the fetched chunk");
        assert_eq!(
            Some(body[prefix_length as usize..32 * 1024].to_vec()),
            store
                .read_durable_range(&session.id, &session.variant.video, key.range.clone())
                .await
                .expect("committed chunk should validate")
        );
        let loaded = store
            .load_range_manifest(&session.id, &session.variant.video)
            .expect("checkpoint should revalidate")
            .expect("prefix checkpoint should remain");
        assert_eq!(
            Some("\"tail-v2\""),
            loaded
                .manifest
                .validated_origins
                .iter()
                .find(|binding| binding.origin == media_url_origin(&url).unwrap())
                .and_then(|binding| binding.strong_etag.as_deref())
        );
    }

    #[tokio::test]
    async fn prefix_only_resume_accepts_later_strong_etag_without_remote_samples() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let body = Arc::new(fake_mp4());
        let (url, _task) =
            start_etag_range_upstream(Arc::clone(&body), "\"prefix-v1\"", "\"tail-v2\"", false)
                .await;
        let session = sample_session("prefix-only-revalidation", &url);
        let key = seed_range_prefix(&store, &session, &url, &body, 16, "\"prefix-v1\"").await;
        assert!(
            select_cross_cdn_sample_ranges(
                &store
                    .load_range_manifest(&session.id, &session.variant.video)
                    .expect("prefix checkpoint should validate")
                    .expect("prefix checkpoint should exist")
                    .manifest
                    .extents,
                16,
            )
            .is_empty()
        );
        let _activity = store
            .range_cache
            .enter(&session.id, HlsRangePriority::Foreground)
            .expect("foreground activity should be admitted");
        let (bytes, _, etag, _, _, _) = store
            .fetch_range_chunk(
                &reqwest::Client::new(),
                &session.id,
                &session.variant.video,
                &key,
                0,
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            )
            .await
            .expect("prefix-only content revalidation should accept the changed tag");
        assert_eq!(body[16..], bytes);
        assert_eq!(Some("\"tail-v2\"".to_owned()), etag);
    }

    #[tokio::test]
    async fn new_origin_sample_corruption_falls_back_without_identity_etag_assumptions() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let body = Arc::new(
            (0..160 * 1024)
                .map(|byte| (byte % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let prefix_length = 16_u64;
        let client = reqwest::Client::new();
        let (seed_url, _seed_task) =
            start_etag_range_upstream(Arc::clone(&body), "\"seed\"", "\"seed\"", false).await;
        let (primary_url, _primary_task, primary_requests) = start_etag_range_upstream_configured(
            Arc::clone(&body),
            "\"prefix-v1\"",
            "\"tail-v2\"",
            true,
            None,
            Some(64 * 1024),
        )
        .await;
        let (backup_url, _backup_task, backup_requests) = start_etag_range_upstream_configured(
            Arc::clone(&body),
            "\"backup-prefix\"",
            "\"backup-prefix\"",
            false,
            None,
            Some(64 * 1024),
        )
        .await;
        let mut session = sample_session("etag-backup", &primary_url);
        session
            .variant
            .video
            .request
            .backup_urls
            .push(backup_url.clone());
        let mut key = seed_range_prefix(
            &store,
            &session,
            &seed_url,
            &body,
            prefix_length,
            "\"seed\"",
        )
        .await;
        key.range = prefix_length..32 * 1024;
        let loaded = store
            .load_range_manifest(&session.id, &session.variant.video)
            .expect("prefix checkpoint should validate")
            .expect("prefix checkpoint should exist");
        store
            .commit_range_extent(
                &session.id,
                &session.variant.video,
                &loaded.manifest,
                64 * 1024..128 * 1024,
                &body[64 * 1024..128 * 1024],
                &seed_url,
                Some("\"seed\"".to_owned()),
                None,
            )
            .await
            .expect("non-prefix sample bytes should become durable");
        let _activity = store
            .range_cache
            .enter(&session.id, HlsRangePriority::Foreground)
            .expect("foreground activity should be admitted");
        let (bytes, final_url, etag, _, _, verified_manifest) = store
            .fetch_range_chunk(
                &client,
                &session.id,
                &session.variant.video,
                &key,
                0,
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            )
            .await
            .expect("verified backup should be accepted after sampled candidate mismatch");
        assert_eq!(body[prefix_length as usize..32 * 1024], bytes);
        assert_eq!(backup_url, final_url);
        assert_eq!(Some("\"backup-prefix\"".to_owned()), etag);
        assert!(primary_requests.load(Ordering::Relaxed) > 0);
        assert!(primary_requests.load(Ordering::Relaxed) <= 1 + HLS_CROSS_CDN_MAX_SAMPLES);
        assert!(backup_requests.load(Ordering::Relaxed) <= 2 + HLS_CROSS_CDN_MAX_SAMPLES);
        store
            .commit_range_extent(
                &session.id,
                &session.variant.video,
                &verified_manifest,
                key.range.clone(),
                &bytes,
                &final_url,
                etag,
                None,
            )
            .await
            .expect("verified backup bytes should publish");
        let loaded = store
            .load_range_manifest(&session.id, &session.variant.video)
            .expect("checkpoint should validate")
            .expect("checkpoint should exist");
        assert!(
            loaded
                .manifest
                .validated_origins
                .iter()
                .any(|origin| origin.strong_etag.as_deref() == Some("\"backup-prefix\""))
        );
    }

    #[tokio::test]
    async fn sample_short_body_and_wrong_content_range_reject_new_origin() {
        for (case, fault, expected_error) in [
            ("short", SampleResponseFault::ShortBody, "invalid-response"),
            (
                "wrong-range",
                SampleResponseFault::WrongRange,
                "invalid-response",
            ),
            (
                "ok-status",
                SampleResponseFault::Status(StatusCode::OK),
                "range-unsupported",
            ),
            (
                "server-error",
                SampleResponseFault::Status(StatusCode::SERVICE_UNAVAILABLE),
                "upstream-status",
            ),
        ] {
            let temp = TempDir::new().expect("temp dir should be created");
            let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
            let body = Arc::new(
                (0..160 * 1024)
                    .map(|byte| (byte % 251) as u8)
                    .collect::<Vec<_>>(),
            );
            let (seed_url, _seed_task) =
                start_etag_range_upstream(Arc::clone(&body), "\"seed\"", "\"seed\"", false).await;
            let (candidate_url, _candidate_task, requests) = start_etag_range_upstream_configured(
                Arc::clone(&body),
                "\"candidate\"",
                "\"candidate\"",
                false,
                Some(fault),
                Some(64 * 1024),
            )
            .await;
            let session = sample_session(&format!("sample-reject-{case}"), &candidate_url);
            let mut key =
                seed_range_prefix(&store, &session, &seed_url, &body, 16, "\"seed\"").await;
            key.range = 16..32 * 1024;
            let loaded = store
                .load_range_manifest(&session.id, &session.variant.video)
                .expect("prefix checkpoint should validate")
                .expect("prefix checkpoint should exist");
            store
                .commit_range_extent(
                    &session.id,
                    &session.variant.video,
                    &loaded.manifest,
                    64 * 1024..128 * 1024,
                    &body[64 * 1024..128 * 1024],
                    &seed_url,
                    Some("\"seed\"".to_owned()),
                    None,
                )
                .await
                .expect("sample extent should be durable");
            let _activity = store
                .range_cache
                .enter(&session.id, HlsRangePriority::Foreground)
                .expect("foreground activity should be admitted");
            let error = store
                .fetch_range_chunk(
                    &reqwest::Client::new(),
                    &session.id,
                    &session.variant.video,
                    &key,
                    0,
                    HlsRangePriority::Foreground,
                    &|| HlsCacheFillControl::Continue,
                )
                .await
                .expect_err("malformed sample should reject the candidate");
            match expected_error {
                "invalid-response" => assert!(matches!(error, HlsRangeError::InvalidResponse(_))),
                "range-unsupported" => {
                    assert!(matches!(error, HlsRangeError::RangeUnsupported))
                }
                "upstream-status" => assert!(matches!(
                    error,
                    HlsRangeError::UpstreamStatus(status)
                        if status == StatusCode::SERVICE_UNAVAILABLE
                )),
                _ => unreachable!("test case should declare an expected error"),
            }
            let after = store
                .load_range_manifest(&session.id, &session.variant.video)
                .expect("checkpoint should remain readable")
                .expect("checkpoint should remain present");
            assert!(
                !after
                    .manifest
                    .validated_origins
                    .iter()
                    .any(|origin| origin.origin == media_url_origin(&candidate_url).unwrap())
            );
            assert!(range_is_covered(
                &after.manifest.extents,
                64 * 1024..128 * 1024
            ));
            assert!(requests.load(Ordering::Relaxed) <= 1 + HLS_CROSS_CDN_MAX_SAMPLES);
        }
    }

    #[tokio::test]
    async fn weak_chunk_etag_revalidates_and_replaces_old_strong_binding() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let body = Arc::new(
            (0..160 * 1024)
                .map(|byte| (byte % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let (url, _task) =
            start_etag_range_upstream(Arc::clone(&body), "\"strong-v1\"", "W/\"weak-v2\"", false)
                .await;
        let session = sample_session("weak-etag-transition", &url);
        let mut key = seed_range_prefix(&store, &session, &url, &body, 16, "\"strong-v1\"").await;
        key.range = 16..32 * 1024;
        let loaded = store
            .load_range_manifest(&session.id, &session.variant.video)
            .expect("prefix checkpoint should validate")
            .expect("prefix checkpoint should exist");
        store
            .commit_range_extent(
                &session.id,
                &session.variant.video,
                &loaded.manifest,
                64 * 1024..128 * 1024,
                &body[64 * 1024..128 * 1024],
                &url,
                Some("\"strong-v1\"".to_owned()),
                None,
            )
            .await
            .expect("durable sample should use old accepted tag");
        let _activity = store
            .range_cache
            .enter(&session.id, HlsRangePriority::Foreground)
            .expect("foreground activity should be admitted");
        store
            .ensure_range_span(
                &reqwest::Client::new(),
                &session.id,
                &session.variant.video,
                &key.resource,
                key.range.clone(),
                0,
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
                &|_| {},
            )
            .await
            .expect("weak tag should be accepted after content revalidation");
        let loaded = store
            .load_range_manifest(&session.id, &session.variant.video)
            .expect("updated checkpoint should validate")
            .expect("updated checkpoint should exist");
        assert_eq!(
            None,
            loaded
                .manifest
                .validated_origins
                .iter()
                .find(|binding| binding.origin == media_url_origin(&url).unwrap())
                .and_then(|binding| binding.strong_etag.as_deref())
        );
    }

    #[tokio::test]
    async fn stable_redirected_origin_is_verified_and_used_for_chunk_publication() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let body = Arc::new(
            (0..160 * 1024)
                .map(|byte| (byte % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let (effective_url, _effective_task) =
            start_etag_range_upstream(Arc::clone(&body), "\"prefix\"", "\"tail\"", false).await;
        let target = effective_url
            .strip_suffix("/video.m4s")
            .expect("effective URL should have the test resource path");
        let (redirect_url, _redirect_task) = start_range_redirect(target).await;
        let session = sample_session("stable-range-redirect", &redirect_url);
        let mut key =
            seed_range_prefix(&store, &session, &redirect_url, &body, 16, "\"prefix\"").await;
        key.range = 16..32 * 1024;
        let loaded = store
            .load_range_manifest(&session.id, &session.variant.video)
            .expect("prefix checkpoint should validate")
            .expect("prefix checkpoint should exist");
        store
            .commit_range_extent(
                &session.id,
                &session.variant.video,
                &loaded.manifest,
                64 * 1024..128 * 1024,
                &body[64 * 1024..128 * 1024],
                &redirect_url,
                Some("\"prefix\"".to_owned()),
                None,
            )
            .await
            .expect("sample extent should use old accepted validator");
        let _activity = store
            .range_cache
            .enter(&session.id, HlsRangePriority::Foreground)
            .expect("foreground activity should be admitted");
        store
            .ensure_range_span(
                &reqwest::Client::new(),
                &session.id,
                &session.variant.video,
                &key.resource,
                key.range.clone(),
                0,
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
                &|_| {},
            )
            .await
            .expect("stable effective origin should verify before publication");
        let loaded = store
            .load_range_manifest(&session.id, &session.variant.video)
            .expect("updated checkpoint should validate")
            .expect("updated checkpoint should exist");
        assert!(loaded.manifest.validated_origins.iter().any(|binding| {
            binding.origin == media_url_origin(&effective_url).unwrap()
                && binding.strong_etag.as_deref() == Some("\"tail\"")
        }));
        assert_eq!(
            Some(body[key.range.start as usize..key.range.end as usize].to_vec()),
            store
                .read_durable_range(&session.id, &session.variant.video, key.range.clone())
                .await
                .expect("redirected chunk should validate")
        );
    }

    #[tokio::test]
    async fn local_sample_corruption_aborts_instead_of_falling_back_as_a_match() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let body = Arc::new(
            (0..160 * 1024)
                .map(|byte| (byte % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let (url, _task) =
            start_etag_range_upstream(Arc::clone(&body), "\"v1\"", "\"v2\"", false).await;
        let session = sample_session("local-sample-corruption", &url);
        let key = seed_range_prefix(&store, &session, &url, &body, 16, "\"v1\"").await;
        let loaded = store
            .load_range_manifest(&session.id, &session.variant.video)
            .expect("prefix checkpoint should validate")
            .expect("prefix checkpoint should exist");
        store
            .commit_range_extent(
                &session.id,
                &session.variant.video,
                &loaded.manifest,
                64 * 1024..128 * 1024,
                &body[64 * 1024..128 * 1024],
                &url,
                Some("\"v1\"".to_owned()),
                None,
            )
            .await
            .expect("sample should become durable");
        let data_path = store
            .resource_range_data_path(&session.id, &session.variant.video.id)
            .expect("partial data path should be valid");
        let mut data = fs::OpenOptions::new()
            .write(true)
            .open(data_path)
            .expect("partial data file should open");
        data.seek(SeekFrom::Start(64 * 1024))
            .expect("sample offset should seek");
        data.write_all(&[body[64 * 1024] ^ 1])
            .expect("sample byte should be altered");
        data.sync_all().expect("tampered byte should flush");
        let _activity = store
            .range_cache
            .enter(&session.id, HlsRangePriority::Foreground)
            .expect("foreground activity should be admitted");
        let error = store
            .fetch_range_chunk(
                &reqwest::Client::new(),
                &session.id,
                &session.variant.video,
                &key,
                0,
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            )
            .await
            .expect_err("local extent hash failure must abort revalidation");
        assert!(matches!(error, HlsRangeError::IdentityChanged));
    }

    #[tokio::test]
    async fn stale_validator_snapshot_cannot_publish_a_chunk_after_revalidation() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let body = Arc::new(
            (0..160 * 1024)
                .map(|byte| (byte % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let (url, _task) =
            start_etag_range_upstream(Arc::clone(&body), "\"v1\"", "\"v2\"", false).await;
        let session = sample_session("stale-validator-publish", &url);
        let key = seed_range_prefix(&store, &session, &url, &body, 16, "\"v1\"").await;
        let stale = store
            .load_range_manifest(&session.id, &session.variant.video)
            .expect("prefix checkpoint should validate")
            .expect("prefix checkpoint should exist");
        let updated = store
            .publish_validated_origin(
                &session.id,
                &session.variant.video,
                stale.clone(),
                media_url_origin(&url).expect("candidate origin should parse"),
                Some("\"v2\"".to_owned()),
            )
            .await
            .expect("verified binding update should publish");
        let same_binding = store
            .publish_validated_origin(
                &session.id,
                &session.variant.video,
                stale.clone(),
                media_url_origin(&url).expect("candidate origin should parse"),
                Some("\"v2\"".to_owned()),
            )
            .await
            .expect("the same proven target binding should be idempotent");
        assert_eq!(
            updated.manifest.generation,
            same_binding.manifest.generation
        );
        let conflict = match store
            .publish_validated_origin(
                &session.id,
                &session.variant.video,
                stale.clone(),
                media_url_origin(&url).expect("candidate origin should parse"),
                Some("\"v3\"".to_owned()),
            )
            .await
        {
            Ok(_) => panic!("a competing distinct tag must remain stale"),
            Err(error) => error,
        };
        assert!(matches!(conflict, HlsRangeError::IdentityChanged));
        let error = store
            .commit_range_extent(
                &session.id,
                &session.variant.video,
                &stale.manifest,
                key.range.start..key.range.start + 16,
                &body[key.range.start as usize..key.range.start as usize + 16],
                &url,
                Some("\"v1\"".to_owned()),
                None,
            )
            .await
            .expect_err("stale response snapshot must not publish");
        assert!(matches!(error, HlsRangeError::IdentityChanged));
    }

    #[tokio::test]
    async fn validated_origin_publication_rebases_over_another_origins_validator_update() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let body = Arc::new(
            (0..160 * 1024)
                .map(|byte| (byte % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let (url, _task) =
            start_etag_range_upstream(Arc::clone(&body), "\"v1\"", "\"tail\"", false).await;
        let session = sample_session("origin-validator-rebase", &url);
        seed_range_prefix(&store, &session, &url, &body, 16, "\"v1\"").await;
        let resource = &session.variant.video;
        let stale = store
            .load_range_manifest(&session.id, resource)
            .expect("prefix checkpoint should validate")
            .expect("prefix checkpoint should exist");
        let first_origin = media_url_origin(&url).expect("origin should parse");
        let other_origin = "https://cdn-b.example:443".to_owned();
        let target_origin = "https://cdn-c.example:443".to_owned();

        let with_other_origin = store
            .publish_validated_origin(
                &session.id,
                resource,
                stale.clone(),
                other_origin.clone(),
                Some("\"b-v1\"".to_owned()),
            )
            .await
            .expect("first content-validated origin binding should publish");
        let stale_with_other_origin = with_other_origin.clone();
        store
            .publish_validated_origin(
                &session.id,
                resource,
                with_other_origin,
                other_origin,
                Some("\"b-v2\"".to_owned()),
            )
            .await
            .expect("independent content revalidation should update its validator");

        store
            .publish_validated_origin(
                &session.id,
                resource,
                stale_with_other_origin.clone(),
                target_origin.clone(),
                Some("\"c-v1\"".to_owned()),
            )
            .await
            .expect("target binding should rebase over the verified other-origin update");

        let current = store
            .load_range_manifest(&session.id, resource)
            .expect("published checkpoint should validate")
            .expect("published checkpoint should exist");
        assert!(current.manifest.validated_origins.iter().any(|binding| {
            binding.origin == first_origin && binding.strong_etag.as_deref() == Some("\"v1\"")
        }));
        assert!(current.manifest.validated_origins.iter().any(|binding| {
            binding.origin == target_origin && binding.strong_etag.as_deref() == Some("\"c-v1\"")
        }));
        assert!(current.manifest.validated_origins.iter().any(|binding| {
            binding.origin == "https://cdn-b.example:443"
                && binding.strong_etag.as_deref() == Some("\"b-v2\"")
        }));

        let mut conflicting_extent = current.manifest.clone();
        conflicting_extent.extents[0].sha256 = "0".repeat(64);
        assert!(!range_manifest_rebase_compatible_for_origin_validation(
            &current.manifest,
            &conflicting_extent,
            "https://cdn-c.example:443",
        ));

        let validator_conflict = match store
            .publish_validated_origin(
                &session.id,
                resource,
                stale_with_other_origin.clone(),
                target_origin,
                Some("\"c-v2\"".to_owned()),
            )
            .await
        {
            Ok(_) => panic!("a competing target-origin validator must fail its CAS"),
            Err(error) => error,
        };
        assert!(matches!(validator_conflict, HlsRangeError::IdentityChanged));

        let mut conflicting_content = stale_with_other_origin;
        conflicting_content
            .manifest
            .validated_origins
            .iter_mut()
            .find(|binding| binding.origin == "https://cdn-b.example:443")
            .expect("the independently validated origin should be present")
            .prefix_sha256 = "0".repeat(64);
        let content_conflict = match store
            .publish_validated_origin(
                &session.id,
                resource,
                conflicting_content,
                "https://cdn-d.example:443".to_owned(),
                Some("\"d-v1\"".to_owned()),
            )
            .await
        {
            Ok(_) => panic!("a conflicting validated prefix must remain incompatible"),
            Err(error) => error,
        };
        assert!(matches!(content_conflict, HlsRangeError::IdentityChanged));
    }

    #[test]
    fn extent_publication_rebase_diagnostics_allow_unrelated_additions() {
        let (expected, target_origin) = extent_rebase_test_manifest();
        let mut current = expected.clone();
        current.generation += 1;
        current.extents.push(PersistedRangeExtent {
            start: 16,
            end: 24,
            sha256: "concurrent-extent-digest".to_owned(),
        });
        current.durable_bytes = 16;
        current.validated_origins.push(PersistedRangeOrigin {
            origin: "https://new-origin.example:443".to_owned(),
            prefix_sha256: "new-origin-prefix-digest".to_owned(),
            strong_etag: Some("\"new-origin-etag\"".to_owned()),
        });

        assert!(range_manifest_rebase_compatible_for_extent_publication(
            &expected,
            &current,
            &target_origin,
        ));
        assert_eq!(
            None,
            range_manifest_extent_rebase_failure_stage(&expected, &current, &target_origin)
        );
    }

    #[test]
    fn extent_publication_rebase_diagnostics_classify_rejections() {
        let (expected, target_origin) = extent_rebase_test_manifest();
        let assert_reason = |expected: &PersistedRangeManifest,
                             current: &PersistedRangeManifest,
                             stage: &'static str| {
            assert!(!range_manifest_rebase_compatible_for_extent_publication(
                expected,
                current,
                &target_origin,
            ));
            assert!(!range_manifest_target_validator_snapshot_is_stale(
                expected,
                current,
                &target_origin,
            ));
            assert_eq!(
                Some(stage),
                range_manifest_extent_rebase_failure_stage(expected, current, &target_origin)
            );
        };

        let mut current = expected.clone();
        current.resource_id.push_str("-changed");
        assert_reason(&expected, &current, "extent-publish-rebase-resource-id");

        let mut current = expected.clone();
        current.representation_digest.push_str("-changed");
        assert_reason(&expected, &current, "extent-publish-rebase-representation");

        let mut current = expected.clone();
        current.data_identity = Some("42:102".to_owned());
        assert_reason(
            &expected,
            &current,
            "extent-publish-rebase-data-object-identity",
        );

        let mut current = expected.clone();
        current.generation -= 1;
        assert_reason(
            &expected,
            &current,
            "extent-publish-rebase-generation-regression",
        );

        let mut current = expected.clone();
        current.total_length = Some(65);
        assert_reason(&expected, &current, "extent-publish-rebase-total-length");

        let mut current = expected.clone();
        current.prefix_sha256 = Some("different-startup-prefix".to_owned());
        assert_reason(&expected, &current, "extent-publish-rebase-prefix-digest");

        let mut current = expected.clone();
        current
            .validated_origins
            .iter_mut()
            .find(|binding| binding.origin == "https://seed.example:443")
            .expect("seed origin should exist")
            .prefix_sha256 = "different-seed-prefix".to_owned();
        assert_reason(
            &expected,
            &current,
            "extent-publish-rebase-existing-origin-prefix",
        );

        let mut current = expected.clone();
        current.extents.clear();
        assert_reason(&expected, &current, "extent-publish-rebase-existing-extent");

        let mut current = expected.clone();
        current.extents[0].sha256 = "different-extent-digest".to_owned();
        assert_reason(&expected, &current, "extent-publish-rebase-existing-extent");

        let mut current = expected.clone();
        current
            .validated_origins
            .iter_mut()
            .find(|binding| binding.origin == target_origin)
            .expect("target origin should exist")
            .prefix_sha256 = "different-target-prefix".to_owned();
        assert_reason(
            &expected,
            &current,
            "extent-publish-rebase-target-origin-prefix",
        );

        let mut current = expected.clone();
        current
            .validated_origins
            .iter_mut()
            .find(|binding| binding.origin == target_origin)
            .expect("target origin should exist")
            .strong_etag = Some("\"target-etag-v2\"".to_owned());
        assert!(!range_manifest_rebase_compatible_for_extent_publication(
            &expected,
            &current,
            &target_origin,
        ));
        assert!(range_manifest_target_validator_snapshot_is_stale(
            &expected,
            &current,
            &target_origin,
        ));
        assert_eq!(
            Some("extent-publish-rebase-target-origin-strong-etag"),
            range_manifest_extent_rebase_failure_stage(&expected, &current, &target_origin)
        );

        let mut current = expected.clone();
        current
            .validated_origins
            .iter_mut()
            .find(|binding| binding.origin == "https://seed.example:443")
            .expect("seed origin should exist")
            .strong_etag = Some("\"seed-etag-v2\"".to_owned());
        assert!(!range_manifest_target_validator_snapshot_is_stale(
            &expected,
            &current,
            &target_origin,
        ));

        let mut expected_without_target = expected.clone();
        expected_without_target
            .validated_origins
            .retain(|binding| binding.origin != target_origin);
        let mut current_with_target = expected_without_target.clone();
        current_with_target.validated_origins.push(
            expected
                .validated_origins
                .iter()
                .find(|binding| binding.origin == target_origin)
                .expect("target origin should exist")
                .clone(),
        );
        assert_reason(
            &expected_without_target,
            &current_with_target,
            "extent-publish-rebase-target-origin-added",
        );

        let mut current_without_target = expected.clone();
        current_without_target
            .validated_origins
            .retain(|binding| binding.origin != target_origin);
        assert_reason(
            &expected,
            &current_without_target,
            "extent-publish-rebase-target-origin-removed",
        );
    }

    #[tokio::test]
    async fn stale_target_validator_discards_old_response_and_publishes_refetch() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let body = Arc::new(
            (0..160 * 1024)
                .map(|byte| (byte % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let (url, _task, etag_epoch, request_count) =
            start_switchable_etag_range_upstream(Arc::clone(&body)).await;
        let client = reqwest::Client::new();
        let session = sample_session("stale-target-validator-refetch", &url);
        let seeded_key = seed_range_prefix(&store, &session, &url, &body, 16, "\"v1\"").await;
        let chunk_key = RangeChunkKey {
            resource: seeded_key.resource,
            total_length: seeded_key.total_length,
            range: 16..32,
        };
        let resource = &session.variant.video;
        let origin = media_url_origin(&url).expect("fixture origin should parse");
        let _activity = store
            .range_cache
            .enter(&session.id, HlsRangePriority::Foreground)
            .expect("foreground activity should be admitted");
        let old_response_fetched = Arc::new(Notify::new());
        let release_old_response = Arc::new(Notify::new());
        let fetch_attempts = Arc::new(AtomicUsize::new(0));
        let control = || HlsCacheFillControl::Continue;
        let fill = store.fetch_and_commit_range_extent(
            &session.id,
            resource,
            chunk_key.range.clone(),
            || {
                let attempt = fetch_attempts.fetch_add(1, Ordering::Relaxed);
                let pending = store.fetch_range_chunk(
                    &client,
                    &session.id,
                    resource,
                    &chunk_key,
                    0,
                    HlsRangePriority::Foreground,
                    &control,
                );
                let old_response_fetched = Arc::clone(&old_response_fetched);
                let release_old_response = Arc::clone(&release_old_response);
                async move {
                    let response = pending.await?;
                    if attempt == 0 {
                        assert!(response.2.as_deref() == Some("\"v1\""));
                        old_response_fetched.notify_one();
                        release_old_response.notified().await;
                    } else {
                        assert!(response.2.as_deref() == Some("\"v2\""));
                    }
                    Ok(response)
                }
            },
        );
        tokio::pin!(fill);
        tokio::select! {
            _ = &mut fill => panic!("fill must wait at the old-response barrier"),
            () = old_response_fetched.notified() => {}
        }

        etag_epoch.store(true, Ordering::Relaxed);
        let snapshot = store
            .load_range_manifest(&session.id, resource)
            .expect("current checkpoint should validate")
            .expect("current checkpoint should exist");
        let (verified_origin, verified_etag) = store
            .verify_candidate_prefix(
                &client,
                &session.id,
                resource,
                &url,
                Some(&origin),
                &chunk_key,
                &snapshot.manifest,
                HlsRangePriority::Foreground,
                &control,
            )
            .await
            .unwrap_or_else(|_| panic!("new validator must pass bounded prefix validation"));
        assert!(verified_origin == origin);
        assert!(verified_etag.as_deref() == Some("\"v2\""));
        store
            .publish_validated_origin(
                &session.id,
                resource,
                snapshot,
                verified_origin,
                verified_etag,
            )
            .await
            .expect("new validator should publish after byte validation");

        release_old_response.notify_one();
        let (durable_bytes, (published, _, published_etag, _, _, _)) = fill
            .await
            .expect("one fresh response should publish after the stale response is discarded");
        assert!(published == body[16..32]);
        assert!(published_etag.as_deref() == Some("\"v2\""));
        assert_eq!(2, fetch_attempts.load(Ordering::Relaxed));
        assert_eq!(3, request_count.load(Ordering::Relaxed));
        assert_eq!(32, durable_bytes);
        assert_eq!(
            Some(body[16..32].to_vec()),
            store
                .read_durable_range(&session.id, resource, 16..32)
                .await
                .expect("published range should validate")
        );
    }

    #[tokio::test]
    async fn extent_publication_rebases_over_non_target_validator_update_after_fetch() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let body = Arc::new(
            (0..160 * 1024)
                .map(|byte| (byte % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let (url, _task) =
            start_etag_range_upstream(Arc::clone(&body), "\"prefix-v1\"", "\"tail-v2\"", false)
                .await;
        let session = sample_session("extent-origin-validator-rebase", &url);
        let seeded = seed_range_prefix(&store, &session, &url, &body, 16, "\"prefix-v1\"").await;
        let resource = &session.variant.video;
        let other_origin = "https://cdn-b.example:443".to_owned();
        let initial = store
            .load_range_manifest(&session.id, resource)
            .expect("prefix checkpoint should validate")
            .expect("prefix checkpoint should exist");
        store
            .publish_validated_origin(
                &session.id,
                resource,
                initial,
                other_origin.clone(),
                Some("\"b-v1\"".to_owned()),
            )
            .await
            .expect("other origin binding should publish");

        let chunk_key = RangeChunkKey {
            resource: seeded.resource.clone(),
            total_length: body.len() as u64,
            range: 16..32,
        };
        let _activity = store
            .range_cache
            .enter(&session.id, HlsRangePriority::Foreground)
            .expect("foreground activity should be admitted");
        let (bytes, final_url, etag, last_modified, _, fetched_manifest) = store
            .fetch_range_chunk(
                &reqwest::Client::new(),
                &session.id,
                resource,
                &chunk_key,
                0,
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            )
            .await
            .expect("range fetch and content revalidation should succeed");
        let fetched_snapshot = store
            .load_range_manifest(&session.id, resource)
            .expect("fetched checkpoint should validate")
            .expect("fetched checkpoint should exist");
        assert_eq!(
            fetched_manifest.generation,
            fetched_snapshot.manifest.generation
        );
        let origin_update = store
            .publish_validated_origin(
                &session.id,
                resource,
                fetched_snapshot,
                other_origin,
                Some("\"b-v2\"".to_owned()),
            )
            .await
            .expect("independent verified origin validator update should publish");

        let committed = store
            .commit_range_extent(
                &session.id,
                resource,
                &fetched_manifest,
                chunk_key.range.clone(),
                &bytes,
                &final_url,
                etag,
                last_modified,
            )
            .await;
        assert!(
            committed.is_ok(),
            "extent should publish over unrelated verified validator update: {committed:?}; generation={}",
            origin_update.manifest.generation
        );
    }

    #[tokio::test]
    async fn extent_publication_rejects_selected_origin_some_none_validator_races() {
        for (case, tail_etag, competing_etag) in [
            ("some-to-none", "\"tail-v2\"", None),
            ("none-to-some", "W/\"weak-tail\"", Some("\"competing\"")),
        ] {
            let temp = TempDir::new().expect("temp dir should be created");
            let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
            let body = Arc::new(
                (0..160 * 1024)
                    .map(|byte| (byte % 251) as u8)
                    .collect::<Vec<_>>(),
            );
            let (url, _task) =
                start_etag_range_upstream(Arc::clone(&body), "\"prefix-v1\"", tail_etag, false)
                    .await;
            let session = sample_session(&format!("extent-target-validator-{case}"), &url);
            let seeded =
                seed_range_prefix(&store, &session, &url, &body, 16, "\"prefix-v1\"").await;
            let resource = &session.variant.video;
            let chunk_key = RangeChunkKey {
                resource: seeded.resource,
                total_length: body.len() as u64,
                range: 16..32,
            };
            let _activity = store
                .range_cache
                .enter(&session.id, HlsRangePriority::Foreground)
                .expect("foreground activity should be admitted");
            let (bytes, final_url, etag, last_modified, _, fetched_manifest) = store
                .fetch_range_chunk(
                    &reqwest::Client::new(),
                    &session.id,
                    resource,
                    &chunk_key,
                    0,
                    HlsRangePriority::Foreground,
                    &|| HlsCacheFillControl::Continue,
                )
                .await
                .expect("range fetch and validator revalidation should succeed");
            let fetched_snapshot = store
                .load_range_manifest(&session.id, resource)
                .expect("fetched checkpoint should validate")
                .expect("fetched checkpoint should exist");
            assert_eq!(
                fetched_manifest.generation,
                fetched_snapshot.manifest.generation
            );
            let target_origin = media_url_origin(&url).expect("target origin should parse");
            store
                .publish_validated_origin(
                    &session.id,
                    resource,
                    fetched_snapshot,
                    target_origin,
                    competing_etag.map(str::to_owned),
                )
                .await
                .expect("independent target-origin revalidation should publish");

            let error = match store
                .commit_range_extent(
                    &session.id,
                    resource,
                    &fetched_manifest,
                    chunk_key.range,
                    &bytes,
                    &final_url,
                    etag,
                    last_modified,
                )
                .await
            {
                Ok(_) => {
                    panic!("selected-origin validator race must reject extent publication: {case}")
                }
                Err(error) => error,
            };
            assert!(
                matches!(error, HlsRangeError::IdentityChanged),
                "{case}: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn same_origin_none_to_strong_etag_transition_revalidates_before_extent_commit() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let body = Arc::new(
            (0..160 * 1024)
                .map(|byte| (byte % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let (url, _task) =
            start_etag_range_upstream(Arc::clone(&body), "\"prefix-v1\"", "\"tail-v2\"", false)
                .await;
        let session = sample_session("extent-validator-none-to-strong", &url);
        let mut key = seed_range_prefix(&store, &session, &url, &body, 16, "\"prefix-v1\"").await;
        key.range = 16..32;
        let resource = &session.variant.video;
        let initial = store
            .load_range_manifest(&session.id, resource)
            .expect("prefix checkpoint should validate")
            .expect("prefix checkpoint should exist");
        store
            .publish_validated_origin(
                &session.id,
                resource,
                initial,
                media_url_origin(&url).expect("target origin should parse"),
                None,
            )
            .await
            .expect("strong validator should be cleared after content validation");

        let _activity = store
            .range_cache
            .enter(&session.id, HlsRangePriority::Foreground)
            .expect("foreground activity should be admitted");
        let (bytes, final_url, etag, last_modified, _, fetched_manifest) = store
            .fetch_range_chunk(
                &reqwest::Client::new(),
                &session.id,
                resource,
                &key,
                0,
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            )
            .await
            .expect("new strong validator should trigger content revalidation");
        assert_eq!(Some("\"tail-v2\""), etag.as_deref());
        assert_eq!(
            Some("\"tail-v2\""),
            fetched_manifest
                .validated_origins
                .iter()
                .find(|binding| binding.origin == media_url_origin(&url).unwrap())
                .and_then(|binding| binding.strong_etag.as_deref())
        );
        store
            .commit_range_extent(
                &session.id,
                resource,
                &fetched_manifest,
                key.range,
                &bytes,
                &final_url,
                etag,
                last_modified,
            )
            .await
            .expect("extent should commit against the revalidated strong binding");
    }

    #[test]
    fn cross_cdn_samples_are_bounded_nonoverlapping_and_skip_gaps_and_prefix() {
        let prefix_only = vec![PersistedRangeExtent {
            start: 0,
            end: 128 * 1024,
            sha256: String::new(),
        }];
        assert!(select_cross_cdn_sample_ranges(&prefix_only, 128 * 1024).is_empty());

        let extents = vec![
            PersistedRangeExtent {
                start: 0,
                end: 64 * 1024,
                sha256: String::new(),
            },
            PersistedRangeExtent {
                start: 128 * 1024,
                end: 192 * 1024,
                sha256: String::new(),
            },
            PersistedRangeExtent {
                start: 512 * 1024,
                end: 576 * 1024,
                sha256: String::new(),
            },
        ];
        let samples = select_cross_cdn_sample_ranges(&extents, 16 * 1024);
        assert!(!samples.is_empty());
        assert!(samples.len() <= HLS_CROSS_CDN_MAX_SAMPLES);
        assert!(
            samples
                .iter()
                .map(|range| range.end - range.start)
                .sum::<u64>()
                <= 48 * 1024
        );
        for sample in &samples {
            assert!(sample.start >= 16 * 1024);
            assert!(range_is_covered(&extents, sample.clone()));
        }
        for pair in samples.windows(2) {
            assert!(pair[0].end <= pair[1].start || pair[1].end <= pair[0].start);
        }
    }

    #[tokio::test]
    async fn authorized_source_restore_requires_task_item_and_complete_source_files() {
        let (url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut source = sample_session("source-restore-proof", &url);
        source.variant.video.request.size = Some(fake_mp4().len() as u64);
        source.transcoding = HlsTranscodingPlan::with_state(
            HlsTranscodingPlanState::Ready,
            source.variant.id.clone(),
            "generated transcode was restored from the completed source cache",
        );
        store
            .save_session(&source)
            .expect("source session should be saved");
        store
            .cache_resource(
                &reqwest::Client::new(),
                &source.id,
                &source.variant.video,
                &|| HlsCacheFillControl::Continue,
                |_| {},
            )
            .await
            .expect("source resource should be fully cached");

        let restored = source_completed_session_for_restore(&source);
        let item_id = HlsCacheStore::completed_library_item_id(&source.id);
        let manifest_path = store
            .session_dir(&source.id)
            .expect("source session path should be valid")
            .join("session.json");
        let original_manifest = fs::read(&manifest_path).expect("source manifest should exist");
        let metadata_path = store
            .resource_metadata_path(&source.id, &source.variant.video.id)
            .expect("source metadata path should be valid");
        let original_metadata = fs::read(&metadata_path).expect("source metadata should exist");
        let assert_rejections_preserve_admission = || {
            assert_eq!(
                original_manifest,
                fs::read(&manifest_path).expect("source manifest should remain readable")
            );
            let persisted = store
                .playback_session(&source.id)
                .expect("old Ready session should remain readable");
            assert_eq!(HlsTranscodingPlanState::Ready, persisted.transcoding.state);
            drop(
                store
                    .range_cache
                    .enter(&source.id, HlsRangePriority::Foreground)
                    .expect("failed restore must preserve session admission"),
            );
        };

        assert!(
            store
                .save_restored_source_completed_session(&restored, "bilibili.hls.wrong-item")
                .is_err()
        );
        let mut changed_key = restored.clone();
        changed_key
            .variant
            .video
            .request
            .cache_key
            .content_id
            .push_str("-changed");
        assert!(
            store
                .save_restored_source_completed_session(&changed_key, &item_id)
                .is_err()
        );
        let mut changed_size = restored.clone();
        changed_size.variant.video.request.size = Some(fake_mp4().len() as u64 + 1);
        assert!(
            store
                .save_restored_source_completed_session(&changed_size, &item_id)
                .is_err()
        );
        assert!(store.save_completed_session(&restored).is_err());
        assert_rejections_preserve_admission();

        fs::write(&metadata_path, b"{").expect("metadata corruption fixture should be written");
        assert!(
            store
                .save_restored_source_completed_session(&restored, &item_id)
                .is_err()
        );
        assert_rejections_preserve_admission();
        fs::write(&metadata_path, &original_metadata)
            .expect("source metadata fixture should be restored");
        fs::remove_file(&metadata_path).expect("source metadata should be removable");
        assert!(
            store
                .save_restored_source_completed_session(&restored, &item_id)
                .is_err()
        );
        assert_rejections_preserve_admission();

        fs::write(&metadata_path, &original_metadata)
            .expect("source metadata should be restored before authorized commit");
        store
            .save_restored_source_completed_session(&restored, &item_id)
            .expect("task-bound completed source should restore");

        let persisted = store
            .playback_session(&source.id)
            .expect("restored source session should remain available");
        assert_eq!(
            HlsTranscodingPlanState::Disabled,
            persisted.transcoding.state
        );
        assert_eq!(
            source.transcoding.source_variant_id,
            persisted.transcoding.source_variant_id
        );
        assert_eq!(
            source.variant.video.request.cache_key,
            persisted.variant.video.request.cache_key
        );
        assert_eq!(
            source.variant.video.request.size,
            persisted.variant.video.request.size
        );
        assert!(store.source_resources_are_complete(&persisted));
    }

    #[tokio::test]
    async fn completed_primary_resource_bytes_uses_selected_completed_metadata_only() {
        let (url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session_with_audio("completed-byte-aggregate", &url);
        assert_eq!(None, session.variant.video.request.size);
        assert_eq!(None, session.variant.audio.as_ref().unwrap().request.size);
        store
            .save_session(&session)
            .expect("session should be saved");
        for resource in [
            &session.variant.video,
            session.variant.audio.as_ref().unwrap(),
        ] {
            store
                .cache_resource(
                    &reqwest::Client::new(),
                    &session.id,
                    resource,
                    &|| HlsCacheFillControl::Continue,
                    |_| {},
                )
                .await
                .expect("selected source resource should complete");
        }

        let expected = (fake_mp4().len() as u64) * 2;
        assert_eq!(
            expected,
            store.completed_primary_resource_bytes(&session).unwrap()
        );

        let mut wrong_declared_size = session.clone();
        wrong_declared_size.variant.video.request.size = Some(fake_mp4().len() as u64 + 1);
        assert_eq!(
            io::ErrorKind::InvalidData,
            store
                .completed_primary_resource_bytes(&wrong_declared_size)
                .expect_err("changed known size should fail binding validation")
                .kind()
        );

        let audio_metadata_path = store
            .resource_metadata_path(&session.id, "audio.m4s")
            .expect("audio metadata path should be valid");
        let audio_metadata = fs::read(&audio_metadata_path).expect("audio metadata should exist");
        fs::remove_file(&audio_metadata_path).expect("audio metadata should be removable");
        assert_eq!(
            io::ErrorKind::NotFound,
            store
                .completed_primary_resource_bytes(&session)
                .expect_err("missing selected audio metadata should fail")
                .kind()
        );
        fs::write(&audio_metadata_path, &audio_metadata)
            .expect("audio metadata should be restored");

        let video_metadata_path = store
            .resource_metadata_path(&session.id, "video.m4s")
            .expect("video metadata path should be valid");
        let video_metadata = fs::read(&video_metadata_path).expect("video metadata should exist");
        let mut altered: serde_json::Value =
            serde_json::from_slice(&video_metadata).expect("metadata should be JSON");
        altered["total_length"] = serde_json::Value::from(fake_mp4().len() as u64 + 1);
        fs::write(
            &video_metadata_path,
            serde_json::to_vec(&altered).expect("altered metadata should serialize"),
        )
        .expect("altered metadata should be written");
        assert_eq!(
            io::ErrorKind::InvalidData,
            store
                .completed_primary_resource_bytes(&session)
                .expect_err("metadata length inconsistent with file should fail")
                .kind()
        );
    }

    struct FullGetGate {
        started: Notify,
        release: Notify,
        body: Vec<u8>,
    }

    #[derive(Clone)]
    struct RangeEtagFixture {
        body: Arc<Vec<u8>>,
        prefix_etag: &'static str,
        tail_etag: &'static str,
        etag_epoch: Option<Arc<AtomicBool>>,
        corrupt_tail: bool,
        sample_fault: Option<SampleResponseFault>,
        sample_start: Option<usize>,
        request_count: Arc<AtomicUsize>,
    }

    #[derive(Clone, Copy)]
    enum SampleResponseFault {
        ShortBody,
        WrongRange,
        Status(StatusCode),
    }

    async fn upstream_range_with_etags(
        State(fixture): State<RangeEtagFixture>,
        headers: HeaderMap,
    ) -> Response<Body> {
        let Some(value) = headers
            .get(reqwest::header::RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes="))
        else {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::empty())
                .expect("bad request response should build");
        };
        let Some((start, end)) = value.split_once('-').and_then(|(start, end)| {
            Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?))
        }) else {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::empty())
                .expect("bad range response should build");
        };
        fixture.request_count.fetch_add(1, Ordering::Relaxed);
        if start > end || end >= fixture.body.len() {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .body(Body::empty())
                .expect("unsatisfiable response should build");
        }
        let mut body = fixture.body[start..=end].to_vec();
        if fixture.corrupt_tail && start > 0 {
            body[0] ^= 0x01;
        }
        let is_sample = fixture.sample_start == Some(start);
        if let Some(SampleResponseFault::Status(status)) = fixture.sample_fault
            && is_sample
        {
            return Response::builder()
                .status(status)
                .body(Body::empty())
                .expect("sample status response should build");
        }
        if is_sample && matches!(fixture.sample_fault, Some(SampleResponseFault::ShortBody)) {
            body.pop();
        }
        let (reported_start, reported_end) =
            if is_sample && matches!(fixture.sample_fault, Some(SampleResponseFault::WrongRange)) {
                (start + 1, end)
            } else {
                (start, end)
            };
        let (prefix_etag, tail_etag) = if fixture
            .etag_epoch
            .as_ref()
            .is_some_and(|epoch| epoch.load(Ordering::Relaxed))
        {
            ("\"v2\"", "\"v2\"")
        } else {
            (fixture.prefix_etag, fixture.tail_etag)
        };
        let etag = if start == 0 { prefix_etag } else { tail_etag };
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_LENGTH, body.len().to_string())
            .header(
                reqwest::header::CONTENT_RANGE,
                format!(
                    "bytes {reported_start}-{reported_end}/{}",
                    fixture.body.len()
                ),
            )
            .header(reqwest::header::ETAG, etag)
            .body(Body::from(body))
            .expect("range response should build")
    }

    async fn upstream_range_redirect(State(target): State<String>) -> Response<Body> {
        Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header(reqwest::header::LOCATION, format!("{target}/video.m4s"))
            .body(Body::empty())
            .expect("range redirect response should build")
    }

    async fn start_range_redirect(target: &str) -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(
            Router::new()
                .route("/video.m4s", get(upstream_range_redirect))
                .with_state(target.to_owned()),
        )
        .await
    }

    async fn start_etag_range_upstream(
        body: Arc<Vec<u8>>,
        prefix_etag: &'static str,
        tail_etag: &'static str,
        corrupt_tail: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let (url, task, _) = start_etag_range_upstream_configured(
            body,
            prefix_etag,
            tail_etag,
            corrupt_tail,
            None,
            None,
        )
        .await;
        (url, task)
    }

    async fn start_etag_range_upstream_configured(
        body: Arc<Vec<u8>>,
        prefix_etag: &'static str,
        tail_etag: &'static str,
        corrupt_tail: bool,
        sample_fault: Option<SampleResponseFault>,
        sample_start: Option<usize>,
    ) -> (String, tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
        let request_count = Arc::new(AtomicUsize::new(0));
        let (url, task) = start_hls_cache_upstream(
            Router::new()
                .route("/video.m4s", get(upstream_range_with_etags))
                .with_state(RangeEtagFixture {
                    body,
                    prefix_etag,
                    tail_etag,
                    etag_epoch: None,
                    corrupt_tail,
                    sample_fault,
                    sample_start,
                    request_count: Arc::clone(&request_count),
                }),
        )
        .await;
        (url, task, request_count)
    }

    async fn start_switchable_etag_range_upstream(
        body: Arc<Vec<u8>>,
    ) -> (
        String,
        tokio::task::JoinHandle<()>,
        Arc<AtomicBool>,
        Arc<AtomicUsize>,
    ) {
        let etag_epoch = Arc::new(AtomicBool::new(false));
        let request_count = Arc::new(AtomicUsize::new(0));
        let (url, task) = start_hls_cache_upstream(
            Router::new()
                .route("/video.m4s", get(upstream_range_with_etags))
                .with_state(RangeEtagFixture {
                    body,
                    prefix_etag: "\"v1\"",
                    tail_etag: "\"v1\"",
                    etag_epoch: Some(Arc::clone(&etag_epoch)),
                    corrupt_tail: false,
                    sample_fault: None,
                    sample_start: None,
                    request_count: Arc::clone(&request_count),
                }),
        )
        .await;
        (url, task, etag_epoch, request_count)
    }

    async fn upstream_ignores_range_and_holds_full_get(
        State(gate): State<Arc<FullGetGate>>,
        headers: HeaderMap,
    ) -> Response<Body> {
        if headers.contains_key(reqwest::header::RANGE) {
            return Response::builder()
                .status(StatusCode::OK)
                .body(Body::empty())
                .expect("ignored-range response should build");
        }

        gate.started.notify_one();
        let release = Arc::clone(&gate);
        let body = gate.body.clone();
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_LENGTH, body.len().to_string())
            .body(Body::from_stream(futures_util::stream::once(async move {
                release.release.notified().await;
                Ok::<_, std::convert::Infallible>(body)
            })))
            .expect("held full-resource response should build")
    }

    #[cfg(unix)]
    fn set_file_modified_time(path: &Path, modified: SystemTime) {
        let modified = modified
            .duration_since(UNIX_EPOCH)
            .expect("test mtime should be after UNIX epoch");
        let c_path = CString::new(path.as_os_str().as_bytes())
            .expect("test path should not contain interior nul bytes");
        let times = [
            libc::timeval {
                tv_sec: modified.as_secs() as libc::time_t,
                tv_usec: modified.subsec_micros() as libc::suseconds_t,
            },
            libc::timeval {
                tv_sec: modified.as_secs() as libc::time_t,
                tv_usec: modified.subsec_micros() as libc::suseconds_t,
            },
        ];
        let result = unsafe { libc::utimes(c_path.as_ptr(), times.as_ptr()) };
        assert_eq!(0, result, "test should update file mtime");
    }

    #[cfg(unix)]
    #[test]
    fn range_file_open_rejects_fifo_without_blocking() {
        let temp = TempDir::new().expect("temp dir should be created");
        let path = temp.path().join("range-fifo");
        let c_path = CString::new(path.as_os_str().as_bytes())
            .expect("test path should not contain interior nul bytes");
        let result = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(0, result, "test fifo should be created");

        let error = open_range_file(fs::OpenOptions::new().read(true), &path)
            .expect_err("fifo should not be accepted as a range file");
        assert_eq!(io::ErrorKind::PermissionDenied, error.kind());
    }

    #[test]
    fn resumable_size_cap_matches_the_extent_limit() {
        let chunks = planned_range_chunks(RANGE_MAX_SIZE);
        assert_eq!(RANGE_MAX_CHUNKS as usize, chunks.len());
        assert_eq!(0..RANGE_STARTUP_CHUNK_BYTES, chunks[0]);
        assert_eq!(RANGE_MAX_SIZE, chunks.last().unwrap().end);
        assert!(range_size_is_resumable(RANGE_MAX_SIZE));

        assert_eq!(
            RANGE_MAX_CHUNKS as usize + 1,
            planned_range_chunks(RANGE_MAX_SIZE + 1).len()
        );
        assert!(!range_size_is_resumable(RANGE_MAX_SIZE + 1));
        assert!(!range_size_is_resumable(64 * 1024 * 1024 * 1024));
    }

    #[test]
    fn refreshing_live_session_keeps_range_admission_open_during_publication() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = Arc::new(temp_store(&temp));
        let session = sample_session("session-refresh-barrier", "https://cdn.example/video.m4s");
        store
            .save_session(&session)
            .expect("initial session should save");
        let mut refreshed = session.clone();
        refreshed.variant.video.request.url = "https://edge.example/rotated-token.m4s".to_owned();
        refreshed.variant.video.request.headers[0].value =
            "https://www.bilibili.com/new".to_owned();

        let (guarded_tx, guarded_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let writer_store = Arc::clone(&store);
        let writer_session = refreshed;
        let writer = std::thread::spawn(move || {
            writer_store.save_session_with_publication_hook(&writer_session, || {
                guarded_tx
                    .send(())
                    .expect("test should observe the publication barrier");
                release_rx
                    .recv()
                    .expect("test should release the publication barrier");
            })
        });

        guarded_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("refresh should reach the publication barrier");
        let admission = store
            .range_cache
            .enter(&session.id, HlsRangePriority::Foreground);
        release_tx.send(()).expect("writer should still be waiting");
        writer
            .join()
            .expect("session publication thread should join")
            .expect("session refresh should complete");

        assert!(
            admission.is_ok(),
            "normal refresh must not mark an already-live session as retired"
        );
    }

    #[test]
    fn failed_session_refresh_preserves_old_manifest_and_admission() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-refresh-failure", "https://cdn.example/video.m4s");
        store
            .save_session(&session)
            .expect("initial session should save");
        let manifest_path = store
            .session_dir(&session.id)
            .expect("session path should be valid")
            .join("session.json");
        let original = fs::read(&manifest_path).expect("original manifest should be readable");
        fs::create_dir(manifest_path.with_extension("tmp"))
            .expect("publication temp path should be blocked");

        assert!(store.save_session(&session).is_err());
        assert_eq!(original, fs::read(&manifest_path).unwrap());
        assert!(
            store
                .range_cache
                .enter(&session.id, HlsRangePriority::Foreground)
                .is_ok()
        );
    }

    #[test]
    fn ordinary_save_cannot_reopen_a_retired_session() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-retired-save", "https://cdn.example/video.m4s");
        store
            .save_session(&session)
            .expect("initial session should save");
        let manifest_path = store
            .session_dir(&session.id)
            .expect("session path should be valid")
            .join("session.json");
        let original = fs::read(&manifest_path).expect("original manifest should be readable");
        store
            .try_begin_session_removal(&session.id)
            .expect("removal should begin")
            .expect("no activity should block removal")
            .commit();

        assert!(store.save_session(&session).is_err());
        assert_eq!(original, fs::read(&manifest_path).unwrap());
        assert!(matches!(
            store
                .range_cache
                .enter(&session.id, HlsRangePriority::Foreground),
            Err(HlsRangeError::SessionRemoving)
        ));
    }

    #[test]
    fn failed_fresh_publication_stays_closed_and_can_retry_fresh() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-fresh-retry", "https://cdn.example/video.m4s");
        let session_dir = store
            .session_dir(&session.id)
            .expect("session path should be valid");
        fs::create_dir_all(&session_dir).expect("session directory should exist");
        fs::create_dir(session_dir.join("session.tmp"))
            .expect("publication temp path should be blocked");

        assert!(store.save_session(&session).is_err());
        assert!(matches!(
            store
                .range_cache
                .enter(&session.id, HlsRangePriority::Foreground),
            Err(HlsRangeError::SessionRemoving)
        ));
        fs::remove_dir(session_dir.join("session.tmp")).expect("test temp path should be removed");
        store
            .save_session(&session)
            .expect("a fresh publication should be retryable after failed first write");
        assert!(
            store
                .range_cache
                .enter(&session.id, HlsRangePriority::Foreground)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn range_probe_retries_backup_after_primary_502() {
        let (failed_url, _failed_task) =
            start_hls_cache_upstream(Router::new().route("/video.m4s", get(upstream_bad_gateway)))
                .await;
        let (backup_url, _backup_task) =
            start_hls_cache_upstream(Router::new().route("/video.m4s", get(upstream_range_probe)))
                .await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let mut session = sample_session("range-probe-failover", &failed_url);
        session.variant.video.request.backup_urls.push(backup_url);
        store
            .save_session(&session)
            .expect("session should be saved");
        let key = store
            .range_resource_key(&session.id, &session.variant.video)
            .expect("range key should be valid");
        let total = store
            .discover_range_total(
                &reqwest::Client::new(),
                &session.variant.video,
                &key,
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            )
            .await
            .expect("range discovery should use the backup after a 502");
        assert_eq!(fake_mp4().len() as u64, total);
    }

    #[tokio::test]
    async fn durable_range_read_hashes_returned_bytes_and_ignores_mtime_churn() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let mut session = sample_session("durable-range-read", "https://cdn.example/video.m4s");
        let body = fake_mp4();
        session.variant.video.request.size = Some(body.len() as u64);
        store
            .save_session(&session)
            .expect("session should be saved");
        let resource = &session.variant.video;
        let key = store
            .range_resource_key(&session.id, resource)
            .expect("range key should be valid");
        let loaded = store
            .ensure_range_manifest(&session.id, resource, &key)
            .expect("range manifest should be initialized");
        let loaded = store
            .publish_range_manifest(&session.id, resource, loaded, |manifest| {
                manifest.total_length = Some(body.len() as u64);
            })
            .await
            .expect("range total should be checkpointed");
        store
            .commit_range_extent(
                &session.id,
                resource,
                &loaded.manifest,
                0..body.len() as u64,
                &body,
                "https://cdn.example/video.m4s",
                None,
                None,
            )
            .await
            .expect("range extent should be durable");
        let data_path = store
            .resource_range_data_path(&session.id, &resource.id)
            .expect("range data path should be valid");

        let selected = 2..7;
        assert_eq!(
            body[selected.start as usize..selected.end as usize],
            store
                .read_durable_range(&session.id, resource, selected.clone())
                .await
                .expect("verified bytes should read")
                .expect("range should be durable")
        );
        #[cfg(unix)]
        set_file_modified_time(&data_path, SystemTime::now() - Duration::from_secs(60));
        assert_eq!(
            body[selected.start as usize..selected.end as usize],
            store
                .read_durable_range(&session.id, resource, selected.clone())
                .await
                .expect("benign mtime change should not invalidate the object")
                .expect("range should remain durable")
        );

        let mut data = fs::OpenOptions::new()
            .write(true)
            .open(&data_path)
            .expect("range data should open for mutation");
        data.seek(SeekFrom::Start(selected.start)).unwrap();
        data.write_all(b"X").unwrap();
        data.sync_all().unwrap();
        assert!(matches!(
            store
                .read_durable_range(&session.id, resource, selected)
                .await,
            Err(HlsRangeError::IdentityChanged)
        ));
    }

    #[tokio::test]
    async fn durable_range_read_allows_uncheckpointed_disjoint_extent_growth() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let mut session =
            sample_session("durable-range-peer-growth", "https://cdn.example/video.m4s");
        let body = fake_mp4();
        session.variant.video.request.size = Some(body.len() as u64 + 64 * 1024);
        store
            .save_session(&session)
            .expect("session should be saved");
        let resource = &session.variant.video;
        let key = store
            .range_resource_key(&session.id, resource)
            .expect("range key should be valid");
        let loaded = store
            .ensure_range_manifest(&session.id, resource, &key)
            .expect("range manifest should be initialized");
        let total_length = body.len() as u64 + 64 * 1024;
        let loaded = store
            .publish_range_manifest(&session.id, resource, loaded, |manifest| {
                manifest.total_length = Some(total_length);
            })
            .await
            .expect("range total should be checkpointed");
        store
            .commit_range_extent(
                &session.id,
                resource,
                &loaded.manifest,
                0..body.len() as u64,
                &body,
                "https://cdn.example/video.m4s",
                None,
                None,
            )
            .await
            .expect("initial range extent should be durable");
        let data_path = store
            .resource_range_data_path(&session.id, &resource.id)
            .expect("range data path should be valid");
        let selected = 2..7;

        let bytes = store
            .read_durable_range_with_hook(&session.id, resource, selected.clone(), || {
                let mut writer = fs::OpenOptions::new()
                    .write(true)
                    .open(&data_path)
                    .expect("peer range writer should open the same data object");
                writer
                    .seek(SeekFrom::Start(total_length - 1))
                    .expect("peer writer should seek to a disjoint range");
                writer
                    .write_all(b"X")
                    .expect("peer writer should grow the same data object");
            })
            .await
            .expect("disjoint peer extent growth should not invalidate selected bytes")
            .expect("selected range should be durable");

        assert_eq!(body[selected.start as usize..selected.end as usize], bytes);
    }

    #[tokio::test]
    async fn finalization_recovers_after_data_rename_before_metadata_publication() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let body = fake_mp4();
        let mut session =
            sample_session("range-finalize-recovery", "https://cdn.example/video.m4s");
        session.variant.video.request.size = Some(body.len() as u64);
        store
            .save_session(&session)
            .expect("session should be saved");

        let resource = &session.variant.video;
        let key = store
            .range_resource_key(&session.id, resource)
            .expect("range key should be valid");
        let loaded = store
            .ensure_range_manifest(&session.id, resource, &key)
            .expect("range manifest should be initialized");
        let loaded = store
            .publish_range_manifest(&session.id, resource, loaded, |manifest| {
                manifest.total_length = Some(body.len() as u64);
            })
            .await
            .expect("range total should be checkpointed");
        store
            .commit_range_extent(
                &session.id,
                resource,
                &loaded.manifest,
                0..body.len() as u64,
                &body,
                "https://cdn.example/video.m4s",
                None,
                None,
            )
            .await
            .expect("complete MP4 extent should be durable");

        let partial_path = store
            .resource_range_data_path(&session.id, &resource.id)
            .expect("partial data path should be valid");
        let final_path = store
            .resource_path(&session.id, &resource.id)
            .expect("final resource path should be valid");
        let manifest_path = store
            .resource_range_manifest_path(&session.id, &resource.id)
            .expect("range manifest path should be valid");
        let metadata_path = store
            .resource_metadata_path(&session.id, &resource.id)
            .expect("resource metadata path should be valid");
        let checkpoint_bytes = fs::read(&manifest_path).expect("checkpoint should be readable");
        assert!(
            store
                .completed_range_ready(&session.id, resource, 0..1)
                .expect("initial cache miss should be readable")
                .is_none()
        );

        fs::rename(&partial_path, &final_path).expect("simulate crash after durable data rename");
        assert!(manifest_path.exists(), "range checkpoint should remain");
        assert!(!metadata_path.exists(), "metadata publication has not run");

        store
            .finalize_range_resource(&session.id, resource, body.len() as u64)
            .await
            .expect("finalization should recover the renamed data object");

        let cached = store
            .cached_resource(&session.id, &resource.id)
            .expect("validated resource metadata should be published");
        assert_eq!(body.len() as u64, cached.total_length);
        assert!(cached.initialization_length > 0);
        assert!(cached.initialization_length < cached.total_length);
        assert!(!manifest_path.exists(), "completed checkpoint is removed");
        assert_eq!(body, fs::read(final_path).expect("final MP4 should remain"));

        let resumed = store
            .ensure_range_state_after_initial_miss(&session.id, resource, 0..1)
            .await
            .expect("ensure should recheck completion after its initial miss");
        assert!(matches!(
            resumed,
            RangeEnsureState::Completed(HlsReadyRange { total_length, .. })
                if total_length == body.len() as u64
        ));
        assert!(
            !partial_path.exists(),
            "ensure must not recreate range data"
        );
        assert!(
            !manifest_path.exists(),
            "ensure must not recreate its checkpoint"
        );
        let mut completed_resource = resource.clone();
        completed_resource.request.url = "http://127.0.0.1:0/unused".to_owned();
        let ready = store
            .ensure_resource_range(
                &reqwest::Client::new(),
                &session.id,
                &completed_resource,
                0..1,
                HlsRangePriority::Foreground,
                &|| HlsCacheFillControl::Continue,
            )
            .await
            .expect("completed cache should avoid another upstream request");
        assert_eq!(body.len() as u64, ready.total_length);
        assert_eq!(
            body,
            fs::read(&cached.path).expect("completed MP4 should remain")
        );

        let mut different_identity = resource.clone();
        different_identity
            .request
            .cache_key
            .source_hash
            .push_str("-changed");
        assert!(matches!(
            store.completed_range_ready(&session.id, &different_identity, 0..1),
            Err(HlsRangeError::IdentityChanged)
        ));
        assert!(!partial_path.exists());
        assert!(!manifest_path.exists());

        let removal = store
            .begin_session_removal(&session.id)
            .await
            .expect("idle session should enter removal");
        assert!(matches!(
            store
                .ensure_resource_range(
                    &reqwest::Client::new(),
                    &session.id,
                    resource,
                    0..1,
                    HlsRangePriority::Foreground,
                    &|| HlsCacheFillControl::Continue,
                )
                .await,
            Err(HlsRangeError::SessionRemoving)
        ));
        drop(removal);
        assert!(!partial_path.exists());
        assert!(!manifest_path.exists());

        fs::write(&manifest_path, checkpoint_bytes)
            .expect("simulate crash after metadata publication before checkpoint cleanup");
        store
            .finalize_range_resource(&session.id, resource, body.len() as u64)
            .await
            .expect("recovery should clean a checkpoint after metadata publication");
        assert!(store.cached_resource(&session.id, &resource.id).is_some());
        assert!(
            !manifest_path.exists(),
            "stale range checkpoint should not survive completed publication"
        );
    }

    #[test]
    fn saves_and_loads_hls_session_manifest() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-1", "https://example.test/video.m4s");

        store
            .save_session(&session)
            .expect("session manifest should save");
        let sessions = store.load_sessions().expect("session manifest should load");

        assert_eq!(vec![session], sessions);
    }

    #[test]
    fn saves_and_loads_hls_session_effective_policy() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-policy", "https://example.test/video.m4s");
        session.effective_policy = PlaybackPolicy {
            transcoding_preference: TranscodingPreference::Force,
            compatible_variant_preference: CompatibleVariantPreference::PreferRequested,
            weak_network_preference: WeakNetworkPreference::HoldDowngrade,
        };

        store
            .save_session(&session)
            .expect("session manifest should save");
        let sessions = store.load_sessions().expect("session manifest should load");

        assert_eq!(vec![session], sessions);
    }

    #[test]
    fn saves_and_loads_hls_session_manifest_with_abr_metadata() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-abr", "https://example.test/video.m4s");
        attach_sample_abr_metadata(&mut session);
        session.abr.groups[0].variant_ids[1] = "hevc:1080p".to_owned();
        session.variants[1].id = "hevc:1080p".to_owned();

        store
            .save_session(&session)
            .expect("session manifest should save");
        let sessions = store.load_sessions().expect("session manifest should load");

        assert_eq!(1, sessions.len());
        assert_eq!(session.abr, sessions[0].abr);
        assert_eq!(session.variants, sessions[0].variants);
        assert_eq!(vec![session], sessions);
    }

    #[test]
    fn saves_and_loads_hls_session_manifest_with_alternate_variants() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-alternates", "https://example.test/video.m4s");
        attach_sample_alternate_variant(&mut session, "https://example.test/720p-video.m4s");
        session.alternate_variants[0].id = "h264:720p".to_owned();

        store
            .save_session(&session)
            .expect("session manifest should save");
        let sessions = store.load_sessions().expect("session manifest should load");

        assert_eq!(1, sessions.len());
        assert_eq!(session.alternate_variants, sessions[0].alternate_variants);
        assert_eq!(vec![session], sessions);
    }

    #[test]
    fn saves_and_loads_hls_session_manifest_with_transcoding_plan() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-transcoding", "https://example.test/video.m4s");
        session.transcoding = HlsTranscodingPlan::with_state(
            HlsTranscodingPlanState::Ready,
            "hevc".to_owned(),
            "HEVC source can be converted for AVPlayer.",
        );

        store
            .save_session(&session)
            .expect("session manifest should save");
        let sessions = store.load_sessions().expect("session manifest should load");

        assert_eq!(1, sessions.len());
        assert_eq!(session.transcoding, sessions[0].transcoding);
        assert_eq!(vec![session], sessions);
    }

    #[test]
    fn completed_runtime_session_preserves_hidden_lookup_resources_for_stale_clients() {
        let mut session = sample_session("session-runtime", "https://example.test/video.m4s");
        attach_sample_alternate_variant(&mut session, "https://example.test/720p-video.m4s");

        let runtime = completed_runtime_session(&session);

        assert_eq!(1, runtime.alternate_variants.len());
        assert!(!runtime.master_playlist().contains("segments/v1-video.m3u8"));
        assert!(runtime.variant.video.request.url.is_empty());
        assert!(runtime.variant.video.request.backup_urls.is_empty());
        assert!(runtime.variant.video.request.headers.is_empty());
        assert!(runtime.media_playlist_resource("v1-video.m3u8").is_some());
        let alternate_video = runtime
            .media_resource("v1-video.m4s")
            .expect("runtime alternate video should remain addressable");
        assert_eq!(
            "https://example.test/720p-video.m4s",
            alternate_video.request.url
        );
        assert!(alternate_video.request.backup_urls.is_empty());
        assert!(!alternate_video.request.headers.is_empty());
        let alternate_audio = runtime
            .media_resource("v1-audio.m4s")
            .expect("runtime alternate audio should remain addressable");
        assert_eq!(
            "https://example.test/720p-audio.m4s",
            alternate_audio.request.url
        );
        assert!(alternate_audio.request.backup_urls.is_empty());
        assert!(!alternate_audio.request.headers.is_empty());
    }

    #[test]
    fn sanitized_completed_session_scrubs_hidden_lookup_resources() {
        let mut session = sample_session("session-sanitized", "https://example.test/video.m4s");
        session.accepted_identity = Some(sample_refresh_identity());
        attach_sample_alternate_variant(&mut session, "https://example.test/720p-video.m4s");

        let sanitized = sanitized_completed_session(&session);

        assert_eq!(1, sanitized.alternate_variants.len());
        assert!(sanitized.accepted_identity.is_none());
        assert!(session_completion_matches(&session, &sanitized));
        assert!(
            !sanitized
                .master_playlist()
                .contains("segments/v1-video.m3u8")
        );
        assert!(sanitized.media_playlist_resource("v1-video.m3u8").is_some());
        let alternate_video = sanitized
            .media_resource("v1-video.m4s")
            .expect("sanitized alternate video lookup should remain addressable");
        assert!(alternate_video.request.url.is_empty());
        assert!(alternate_video.request.backup_urls.is_empty());
        assert!(alternate_video.request.headers.is_empty());
        let alternate_audio = sanitized
            .media_resource("v1-audio.m4s")
            .expect("sanitized alternate audio lookup should remain addressable");
        assert!(alternate_audio.request.url.is_empty());
        assert!(alternate_audio.request.backup_urls.is_empty());
        assert!(alternate_audio.request.headers.is_empty());
    }

    #[test]
    fn loads_legacy_hls_session_manifest_without_abr_metadata() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-legacy", "https://example.test/video.m4s");
        let mut manifest = serde_json::to_value(PersistedHlsSession::from(session.clone()))
            .expect("manifest should serialize");
        let object = manifest
            .as_object_mut()
            .expect("persisted session should be a JSON object");
        object.remove("alternate_variants");
        object.remove("advertise_alternate_variants");
        object.remove("abr");
        object.remove("variants");
        object.remove("transcoding");
        object.remove("effective_policy");
        object.remove("accepted_identity");

        let manifest_path = store
            .session_dir("session-legacy")
            .expect("session dir should be valid")
            .join("session.json");
        store
            .write_json_atomically(&manifest_path, &manifest)
            .expect("legacy manifest should save");
        let sessions = store.load_sessions().expect("legacy manifest should load");

        assert_eq!(1, sessions.len());
        assert!(sessions[0].abr.groups.is_empty());
        assert!(sessions[0].variants.is_empty());
        assert_eq!(HlsTranscodingPlan::default(), sessions[0].transcoding);
        assert_eq!(PlaybackPolicy::default(), sessions[0].effective_policy);
        assert_eq!(session, sessions[0]);
    }

    #[test]
    fn persisted_hls_session_round_trips_accepted_identity() {
        let mut session = sample_session(
            "session-identity-roundtrip",
            "https://example.test/video.m4s",
        );
        session.accepted_identity = Some(sample_refresh_identity());

        let persisted = PersistedHlsSession::from(session.clone());
        let restored = HlsPlaybackSession::try_from(persisted)
            .expect("persisted accepted identity should restore");

        assert_eq!(session.accepted_identity, restored.accepted_identity);
        assert_eq!(session, restored);
    }

    #[test]
    fn refreshed_session_accepts_added_removed_and_reordered_headers() {
        fn header(name: &str, value: &str) -> BilibiliHttpHeader {
            BilibiliHttpHeader {
                name: name.to_owned(),
                value: value.to_owned(),
            }
        }

        let cases = [
            (
                vec![header("referer", "https://www.bilibili.com")],
                vec![
                    header("referer", "https://www.bilibili.com/fresh"),
                    header("x-client-hint", "player-v2"),
                ],
            ),
            (
                vec![
                    header("referer", "https://www.bilibili.com"),
                    header("x-client-hint", "player-v1"),
                ],
                vec![header("referer", "https://www.bilibili.com/fresh")],
            ),
            (
                vec![
                    header("referer", "https://www.bilibili.com"),
                    header("x-client-hint", "player-v1"),
                ],
                vec![
                    header("x-client-hint", "player-v2"),
                    header("referer", "https://www.bilibili.com/fresh"),
                ],
            ),
        ];

        for (index, (original_headers, refreshed_headers)) in cases.into_iter().enumerate() {
            let temp = TempDir::new().expect("temp dir should be created");
            let store = temp_store(&temp);
            let mut original = sample_session(
                &format!("session-refresh-headers-{index}"),
                "https://cdn.example/video?old=1",
            );
            original.variant.video.request.headers = original_headers;
            store
                .save_session(&original)
                .expect("initial session should save");

            let mut refreshed = original.clone();
            refreshed.variant.video.request.url = "https://cdn.example/video?fresh=2".to_owned();
            refreshed.variant.video.request.headers = refreshed_headers.clone();
            store
                .save_refreshed_session(&original, &refreshed)
                .expect("header-list changes should be refreshable");

            assert_eq!(
                refreshed_headers,
                store
                    .load_session(&original.id)
                    .expect("refreshed session should load")
                    .variant
                    .video
                    .request
                    .headers
            );
        }
    }

    #[test]
    fn refreshed_session_rejects_immutable_binding_and_policy_changes() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let original = sample_session("session-refresh-binding", "https://cdn.example/video?old=1");
        store
            .save_session(&original)
            .expect("initial session should save");

        let mut changed_resource_id = original.clone();
        changed_resource_id.variant.video.id = "other-video.m4s".to_owned();
        let mut changed_cache_key = original.clone();
        changed_cache_key
            .variant
            .video
            .request
            .cache_key
            .source_hash = "other-source".to_owned();
        let mut changed_policy = original.clone();
        changed_policy.effective_policy = PlaybackPolicy {
            transcoding_preference: TranscodingPreference::Force,
            compatible_variant_preference: CompatibleVariantPreference::PreferRequested,
            weak_network_preference: WeakNetworkPreference::HoldDowngrade,
        };

        for replacement in [changed_resource_id, changed_cache_key, changed_policy] {
            assert!(
                store
                    .save_refreshed_session(&original, &replacement)
                    .is_err()
            );
            assert_eq!(
                original,
                store
                    .load_session(&original.id)
                    .expect("original session should remain persisted")
            );
        }
    }

    #[test]
    fn stale_generic_session_save_preserves_refreshed_request_candidates() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let original = sample_session("session-monotonic-save", "https://cdn.example/video?old=1");
        store
            .save_session(&original)
            .expect("initial session should save");
        let original_range_key = store
            .range_resource_key(&original.id, &original.variant.video)
            .expect("range key should be valid");
        let mut refreshed = original.clone();
        refreshed.variant.video.request.url = "https://cdn.example/video?fresh=2".to_owned();
        refreshed.variant.video.request.backup_urls =
            vec!["https://backup.example/video?fresh=2".to_owned()];
        refreshed.variant.video.request.headers = vec![
            BilibiliHttpHeader {
                name: "x-client-hint".to_owned(),
                value: "player-v2".to_owned(),
            },
            BilibiliHttpHeader {
                name: "referer".to_owned(),
                value: "https://www.bilibili.com/fresh".to_owned(),
            },
        ];
        store
            .save_refreshed_session(&original, &refreshed)
            .expect("current refresh should publish");

        store
            .save_session(&original)
            .expect("stale fill-start save should preserve current candidates");

        let persisted = store
            .load_session(&original.id)
            .expect("refreshed session should remain persisted");
        assert_eq!(
            refreshed.variant.video.request.url,
            persisted.variant.video.request.url
        );
        assert_eq!(
            refreshed.variant.video.request.backup_urls,
            persisted.variant.video.request.backup_urls
        );
        assert_eq!(
            refreshed.variant.video.request.headers,
            persisted.variant.video.request.headers
        );
        assert_eq!(
            original_range_key,
            store
                .range_resource_key(&persisted.id, &persisted.variant.video)
                .expect("refreshed range key should remain valid")
        );
    }

    #[test]
    fn stale_refresh_compare_and_publish_does_not_overwrite_current_manifest() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let original = sample_session("session-refresh-cas", "https://cdn.example/video?old=1");
        store
            .save_session(&original)
            .expect("initial session should save");
        let mut current = original.clone();
        current.variant.video.request.url = "https://cdn.example/video?current=2".to_owned();
        store
            .save_refreshed_session(&original, &current)
            .expect("current refresh should publish");
        let mut stale_replacement = original.clone();
        stale_replacement.variant.video.request.url =
            "https://cdn.example/video?stale=3".to_owned();

        assert!(
            store
                .save_refreshed_session(&original, &stale_replacement)
                .is_err()
        );
        assert_eq!(
            current.variant.video.request.url,
            store
                .load_session(&original.id)
                .expect("current manifest should load")
                .variant
                .video
                .request
                .url
        );
    }

    #[tokio::test]
    async fn refreshed_urls_preserve_durable_range_extents() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(16 * 1024 * 1024);
        let body = fake_mp4();
        let mut original =
            sample_session("session-refresh-extents", "https://cdn.example/video?old=1");
        original.variant.video.request.size = Some(body.len() as u64);
        store
            .save_session(&original)
            .expect("initial session should save");
        let resource = &original.variant.video;
        let key = store
            .range_resource_key(&original.id, resource)
            .expect("range key should be valid");
        let loaded = store
            .ensure_range_manifest(&original.id, resource, &key)
            .expect("range manifest should initialize");
        let loaded = store
            .publish_range_manifest(&original.id, resource, loaded, |manifest| {
                manifest.total_length = Some(body.len() as u64);
            })
            .await
            .expect("range total should publish");
        store
            .commit_range_extent(
                &original.id,
                resource,
                &loaded.manifest,
                0..body.len() as u64,
                &body,
                "https://cdn.example/video?old=1",
                None,
                None,
            )
            .await
            .expect("range extent should become durable");
        let mut refreshed = original.clone();
        refreshed.variant.video.request.url = "https://cdn.example/video?fresh=2".to_owned();
        store
            .save_refreshed_session(&original, &refreshed)
            .expect("same binding should refresh its request URL");

        let checkpoint = store
            .load_range_manifest(&refreshed.id, &refreshed.variant.video)
            .expect("range manifest should remain readable")
            .expect("durable range manifest should remain present");
        assert_eq!(body.len() as u64, checkpoint.manifest.durable_bytes);
        assert!(range_is_covered(
            &checkpoint.manifest.extents,
            0..body.len() as u64
        ));
    }

    #[test]
    fn retryable_candidate_statuses_include_expired_media_responses() {
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::GONE,
        ] {
            assert!(is_retryable_range_candidate_error(
                &HlsRangeError::UpstreamStatus(status)
            ));
        }
        assert!(!is_retryable_range_candidate_error(
            &HlsRangeError::UpstreamStatus(StatusCode::BAD_REQUEST)
        ));
    }

    #[test]
    fn missing_hls_store_directory_scans_as_empty_cache() {
        let temp = TempDir::new().expect("temp dir should be created");
        let root = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()));
        let store = HlsCacheStore::new(root);

        let sessions = store
            .load_sessions()
            .expect("missing HLS store directory should scan as empty");
        let entries = store
            .completed_cache_entries()
            .expect("missing HLS store directory should have no completed entries");
        let usage = store
            .usage_snapshot()
            .expect("missing HLS store directory should report empty usage");

        assert!(sessions.is_empty());
        assert!(entries.is_empty());
        assert_eq!(0, usage.used_bytes);
        assert_eq!(0, usage.completed_session_count);
    }

    #[test]
    fn session_directory_scan_rejects_valid_named_non_directory_entry() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let store_root = store.store_root();
        fs::create_dir_all(&store_root).expect("HLS store root should be created");
        fs::write(store_root.join("replaced-session"), b"not a directory")
            .expect("replacement file should be created");

        let mut scan = store
            .session_directory_scan()
            .expect("HLS session directory scan should start");
        let error = scan
            .next_session_id()
            .expect("replacement entry should be observed")
            .expect_err("valid session names must still resolve to directories");

        assert_eq!(io::ErrorKind::PermissionDenied, error.kind());
    }

    #[test]
    fn missing_cache_root_reports_not_found() {
        let temp = TempDir::new().expect("temp dir should be created");
        let missing_root = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(temp.path()))
            .join("fresh-cache-root");
        let store = HlsCacheStore::new(&missing_root);

        let error = store
            .load_sessions()
            .expect_err("missing cache root should fail closed");

        assert_eq!(io::ErrorKind::NotFound, error.kind());
    }

    #[test]
    fn load_sessions_skips_manifest_with_mismatched_directory_id() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-1", "https://example.test/video.m4s");
        let mismatched_dir = temp
            .path()
            .join(".tvos-net-player")
            .join("hls")
            .join("orphan");
        std::fs::create_dir_all(&mismatched_dir).expect("session dir should be created");
        write_pretty_json(
            &mismatched_dir.join("session.json"),
            &PersistedHlsSession::from(session),
        );

        let sessions = store.load_sessions().expect("cache scan should succeed");

        assert!(sessions.is_empty());
    }

    #[test]
    fn get_completed_library_item_skips_manifest_with_mismatched_directory_id() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let completed_session = sample_session("session-2", "https://example.test/video.m4s");
        let completed_dir = store
            .session_dir(&completed_session.id)
            .expect("session dir should be valid");
        std::fs::create_dir_all(&completed_dir).expect("completed session dir should be created");
        write_pretty_json(
            &completed_dir.join("session.json"),
            &PersistedHlsSession::from(completed_session.clone()),
        );
        std::fs::write(
            store
                .resource_path(&completed_session.id, "video.m4s")
                .expect("resource path should be valid"),
            fake_mp4(),
        )
        .expect("completed resource should be written");
        write_pretty_json(
            &store
                .resource_metadata_path(&completed_session.id, "video.m4s")
                .expect("resource metadata path should be valid"),
            &cached_metadata_for_session(&completed_session, "video.m4s"),
        );
        let mismatched_dir = store
            .session_dir("orphan")
            .expect("mismatched session dir should be valid");
        std::fs::create_dir_all(&mismatched_dir).expect("mismatched session dir should be created");
        write_pretty_json(
            &mismatched_dir.join("session.json"),
            &PersistedHlsSession::from(completed_session),
        );

        assert!(
            store
                .get_completed_library_item("bilibili.hls.orphan")
                .is_none()
        );
    }

    #[test]
    fn load_sessions_reports_unreadable_store_path() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let store_root = temp.path().join(".tvos-net-player").join("hls");
        std::fs::create_dir_all(store_root.parent().unwrap()).unwrap();
        std::fs::write(&store_root, b"not a directory").unwrap();

        let error = store
            .load_sessions()
            .expect_err("non-directory store root should be reported");

        assert_eq!(io::ErrorKind::NotADirectory, error.kind());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_hls_cache_root_symlink_ancestor() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let real_parent = temp.path().join("real-parent");
        let real_root = real_parent.join("cache");
        std::fs::create_dir_all(&real_root).expect("real cache root should be created");
        let link_parent = temp.path().join("link-parent");
        symlink(&real_parent, &link_parent).expect("root ancestor symlink should be made");
        let store = HlsCacheStore::new(link_parent.join("cache"));
        let session = sample_session("session-1", "https://example.test/video.m4s");

        let save_error = store
            .save_session(&session)
            .expect_err("symlinked root ancestor should not be written");
        let load_error = store
            .load_sessions()
            .expect_err("symlinked root ancestor should not be scanned");

        assert_eq!(io::ErrorKind::PermissionDenied, save_error.kind());
        assert_eq!(io::ErrorKind::PermissionDenied, load_error.kind());
    }

    #[test]
    fn rejects_dot_segments_as_hls_cache_ids() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);

        assert!(validate_cache_id(".").is_err());
        assert!(validate_cache_id("..").is_err());
        assert!(store.session_dir("..").is_err());
        assert!(
            store
                .get_completed_library_item("bilibili.hls...")
                .is_none()
        );
    }

    #[tokio::test]
    async fn caches_session_resources_and_exposes_completed_library_item() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-1", &upstream_url);
        let client = reqwest::Client::new();

        let item_id = store
            .cache_session_resources(&client, &session)
            .await
            .expect("session resources should cache");
        let item = store
            .get_completed_library_item(&item_id)
            .expect("completed session should expose a library item");
        let cached = store
            .cached_resource("session-1", "video.m4s")
            .expect("resource should be cached");

        assert_eq!("bilibili.hls.session-1", item.id);
        assert_eq!("Episode", item.title);
        assert_eq!(PlaybackProtocol::Hls as i32, item.variants[0].protocol);
        assert_eq!(fake_mp4().len() as u64, cached.total_length);
        assert_eq!(28, cached.initialization_length);
    }

    #[tokio::test]
    async fn full_cache_fetch_measurement_ranks_refreshed_url_by_observed_speed() {
        let (failed_url, _failed_task) = start_hls_cache_upstream(
            Router::new().route("/video.m4s", get(upstream_server_failure)),
        )
        .await;
        let (working_url, _working_task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let history = Arc::new(CdnHistory::default());
        let store = temp_store(&temp).with_cdn_history(Arc::clone(&history));
        let mut session = sample_session("cdn-full-fetch", &failed_url);
        session.variant.video.request.backup_urls = vec![working_url.clone()];
        let client = reqwest::Client::new();
        let request = &session.variant.video.request;
        let slow_candidate = "https://slow-cdn.example/video.m4s?version=1";
        history.record_request(
            request,
            slow_candidate,
            CdnObservation::playback_complete(
                fake_mp4().len() as u64,
                Duration::from_secs(60),
                Duration::from_secs(30),
                Some(true),
            ),
        );

        store
            .cache_session_resources(&client, &session)
            .await
            .expect("backup should complete the full resource download");

        let mut refreshed_request = request.clone();
        refreshed_request.url = format!("{failed_url}?expires=2");
        refreshed_request.backup_urls = vec![
            format!("{working_url}?expires=2"),
            "https://slow-cdn.example/video.m4s?version=2".to_owned(),
        ];
        assert_eq!(
            refreshed_request.backup_urls[0],
            history.rank_request(&refreshed_request)[0],
            "validated cache transfer metrics should promote the fast refreshed origin"
        );
        let cached = store
            .cached_resource(&session.id, &session.variant.video.id)
            .expect("validated resource should be committed");
        assert_eq!(fake_mp4().len() as u64, cached.total_length);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn caches_transcoded_session_and_restores_generated_manifest() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_transcoding_ready_session("session-transcoded", &upstream_url);
        let active_job_count = Arc::new(AtomicUsize::new(0));
        let config = HlsTranscodingExecutionConfig {
            ffmpeg_path: write_copying_fake_ffmpeg(temp.path()),
            permits: Arc::new(Semaphore::new(1)),
            active_job_count: Arc::clone(&active_job_count),
        };
        let client = reqwest::Client::new();

        let completion = store
            .cache_session_resources_completion_with_control(
                &client,
                &session,
                || HlsCacheFillControl::Continue,
                |_| {},
                Some(config),
            )
            .await
            .expect("transcoding-ready session should cache and transcode");
        let item = store
            .get_completed_library_item(&completion.library_item_id)
            .expect("transcoded session should expose a completed item");
        let cached = store
            .cached_resource("session-transcoded", HLS_TRANSCODED_RESOURCE_ID)
            .expect("generated HLS resource should be cached");
        let reloaded = temp_store(&temp)
            .completed_session("session-transcoded")
            .expect("completed transcoded session should restore after restart");
        let args_log = std::fs::read_to_string(temp.path().join("ffmpeg-args.log"))
            .expect("fake ffmpeg args should be logged");

        assert_eq!(0, active_job_count.load(Ordering::SeqCst));
        assert_eq!(
            HLS_TRANSCODED_RESOURCE_ID,
            completion.session.variant.video.id
        );
        assert_eq!(HLS_TRANSCODED_RESOURCE_ID, reloaded.variant.video.id);
        assert!(completion.session.variant.audio.is_none());
        assert_eq!(
            HlsTranscodingPlanState::NotRequired,
            completion.session.transcoding.state
        );
        assert_eq!(HLS_TRANSCODED_VIDEO_CODEC, item.variants[0].video_codec);
        assert_eq!("mp4a.40.2", item.variants[0].audio_codec);
        assert_eq!(fake_mp4().len() as u64, cached.total_length);
        assert_eq!(28, cached.initialization_length);
        assert!(
            store
                .resource_path("session-transcoded", "video.m4s")
                .unwrap()
                .exists()
        );
        assert!(
            store
                .resource_path("session-transcoded", "audio.m4s")
                .unwrap()
                .exists()
        );
        assert!(
            !store
                .transcoding_commit_marker_path("session-transcoded")
                .unwrap()
                .exists()
        );
        assert!(args_log.contains("-c:v\nlibx264\n"));
        assert!(args_log.contains("-c:a\naac\n"));
        assert!(args_log.contains("-level:v\n4.2\n"));
        assert!(args_log.contains("-vf\nscale=w='min(1920,iw)'"));
        assert!(args_log.contains("fps=fps='min(source_fps,60)'"));
        assert!(args_log.contains("-maxrate\n10000k\n"));
        assert!(args_log.contains("-bufsize\n20000k\n"));
        assert!(
            reloaded
                .master_playlist()
                .contains("segments/transcoded.m3u8")
        );
        assert!(!reloaded.master_playlist().contains("segments/video.m3u8"));
        assert!(reloaded.media_playlist_resource("video.m3u8").is_some());
        assert!(reloaded.media_resource("video.m4s").is_some());
        assert_eq!(
            "",
            reloaded
                .media_resource("video.m4s")
                .expect("source lookup resource should remain addressable")
                .request
                .url
        );
    }

    #[test]
    fn transcoded_completed_session_advertises_capped_output_profile() {
        let mut source_session =
            sample_transcoding_ready_session("session-transcoded-profile", "https://example.test");
        source_session.variant.bandwidth = 30_000_000;
        source_session.variant.width = Some(3840);
        source_session.variant.height = Some(2160);
        source_session.variant.video.request.bandwidth = Some(30_000_000);
        source_session.variant.video.request.width = Some(3840);
        source_session.variant.video.request.height = Some(2160);
        source_session.variant.video.request.frame_rate = Some("120000/1000".to_owned());

        let completed_session =
            transcoded_completed_session(&source_session, fake_mp4().len() as u64);
        let expected_bandwidth =
            LAN_TRANSCODING_MAX_VIDEO_BANDWIDTH_BPS + LAN_TRANSCODING_AUDIO_BANDWIDTH_BPS;

        assert_eq!(
            vec![
                HLS_TRANSCODED_VIDEO_CODEC.to_owned(),
                HLS_TRANSCODED_AUDIO_CODEC.to_owned()
            ],
            completed_session.variant.codecs
        );
        assert_eq!(expected_bandwidth, completed_session.variant.bandwidth);
        assert_eq!(Some(1920), completed_session.variant.width);
        assert_eq!(Some(1080), completed_session.variant.height);
        assert_eq!(
            Some("60".to_owned()),
            completed_session.variant.video.request.frame_rate
        );
        assert_eq!(
            Some(expected_bandwidth),
            completed_session.variant.video.request.bandwidth
        );
        assert_eq!(Some(1920), completed_session.variant.video.request.width);
        assert_eq!(Some(1080), completed_session.variant.video.request.height);
        assert_eq!(
            Some(expected_bandwidth),
            completed_session.variants[0].bandwidth
        );
        assert_eq!(Some(1920), completed_session.variants[0].width);
        assert_eq!(Some(1080), completed_session.variants[0].height);
        assert_eq!(
            Some("60".to_owned()),
            completed_session.variants[0].frame_rate
        );
    }

    #[test]
    fn transcoded_dimensions_match_even_ffmpeg_bounds() {
        assert_eq!(
            (Some(1920), Some(1080)),
            transcoded_dimensions(Some(3840), Some(2160))
        );
        assert_eq!(
            (Some(608), Some(1080)),
            transcoded_dimensions(Some(2160), Some(3840))
        );
        assert_eq!(
            (Some(852), Some(480)),
            transcoded_dimensions(Some(853), Some(480))
        );
    }

    #[test]
    fn usage_snapshot_retains_transcoded_source_lookup_resources_after_manifest_rewrite() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let source_session =
            sample_transcoding_ready_session("session-transcoded-orphans", "https://example.test");
        let completed_session =
            transcoded_completed_session(&source_session, fake_mp4().len() as u64);
        store
            .save_completed_session(&completed_session)
            .expect("completed manifest should save");

        for (session, resource_id) in [
            (&completed_session, HLS_TRANSCODED_RESOURCE_ID),
            (&source_session, "video.m4s"),
            (&source_session, "audio.m4s"),
        ] {
            std::fs::write(
                store
                    .resource_path(&completed_session.id, resource_id)
                    .expect("resource path should be valid"),
                fake_mp4(),
            )
            .expect("resource should be written");
            write_pretty_json(
                &store
                    .resource_metadata_path(&completed_session.id, resource_id)
                    .expect("metadata path should be valid"),
                &cached_metadata_for_session(session, resource_id),
            );
        }
        let transcode_temp_name = transcoding_temp_file_name(HLS_TRANSCODED_RESOURCE_ID);
        for temp_name in ["video.tmp", transcode_temp_name.as_str()] {
            std::fs::write(
                store
                    .resource_path(&completed_session.id, temp_name)
                    .expect("temporary resource path should be valid"),
                b"active temporary writer payload",
            )
            .expect("temporary resource should be written");
        }

        assert!(
            store
                .resource_path(&completed_session.id, "video.m4s")
                .expect("source video path should be valid")
                .exists()
        );
        assert!(
            store
                .resource_path(&completed_session.id, "audio.m4s")
                .expect("source audio path should be valid")
                .exists()
        );

        let usage = store
            .usage_snapshot()
            .expect("usage snapshot should repair orphaned resources");

        assert_eq!(1, usage.completed_session_count);
        assert_eq!(3 * fake_mp4().len() as u64, usage.used_bytes);
        let entries = store
            .completed_cache_entries()
            .expect("completed cache entries should include retained lookup resources");
        assert_eq!(1, entries.len());
        assert_eq!(3 * fake_mp4().len() as u64, entries[0].size_bytes);
        assert!(
            store
                .cached_resource(&completed_session.id, HLS_TRANSCODED_RESOURCE_ID)
                .is_some()
        );
        for resource_id in ["video.m4s", "audio.m4s"] {
            assert!(
                store
                    .resource_path(&completed_session.id, resource_id)
                    .expect("resource path should be valid")
                    .exists()
            );
            assert!(
                store
                    .resource_metadata_path(&completed_session.id, resource_id)
                    .expect("metadata path should be valid")
                    .exists()
            );
        }
        assert!(
            store
                .resource_path(&completed_session.id, "video.tmp")
                .expect("temporary resource path should be valid")
                .exists()
        );
        assert!(
            !store
                .resource_path(
                    &completed_session.id,
                    &transcoding_temp_file_name(HLS_TRANSCODED_RESOURCE_ID)
                )
                .expect("temporary resource path should be valid")
                .exists()
        );
    }

    #[cfg(unix)]
    #[test]
    fn usage_snapshot_preserves_generated_transcode_output_while_manifest_is_ready() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let source_session =
            sample_transcoding_ready_session("session-transcoding-window", "https://example.test");
        let completed_session =
            transcoded_completed_session(&source_session, fake_mp4().len() as u64);
        store
            .save_session(&source_session)
            .expect("ready manifest should save");
        std::fs::write(
            store
                .resource_path(&source_session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated resource path should be valid"),
            fake_mp4(),
        )
        .expect("generated resource should be written");
        write_pretty_json(
            &store
                .resource_metadata_path(&source_session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated metadata path should be valid"),
            &cached_metadata_for_session(&completed_session, HLS_TRANSCODED_RESOURCE_ID),
        );
        std::fs::write(
            store
                .resource_path(
                    &source_session.id,
                    &transcoding_temp_file_name(HLS_TRANSCODED_RESOURCE_ID),
                )
                .expect("transcode temporary path should be valid"),
            b"active ffmpeg output",
        )
        .expect("transcode temporary file should be written");
        let guard = HlsTranscodingCommitGuard::create_if_needed(&store, &source_session)
            .expect("active transcoding marker should be created");
        let marker_path = store
            .transcoding_commit_marker_path(&source_session.id)
            .expect("transcoding marker path should be valid");
        set_file_modified_time(
            &marker_path,
            SystemTime::now()
                .checked_sub(HLS_TRANSCODING_COMMIT_MARKER_TTL + Duration::from_secs(1))
                .expect("test stale marker time should be valid"),
        );
        guard
            .refresh()
            .expect("active transcoding marker should refresh");

        let usage = store
            .usage_snapshot()
            .expect("usage snapshot should preserve active transcode output");

        assert_eq!(
            fake_mp4().len() as u64 + b"active ffmpeg output".len() as u64,
            usage.used_bytes
        );
        assert_eq!(0, usage.completed_session_count);
        assert!(
            store
                .resource_path(&source_session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated resource path should be valid")
                .exists()
        );
        assert!(
            store
                .resource_metadata_path(&source_session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated metadata path should be valid")
                .exists()
        );
        assert!(
            store
                .resource_path(
                    &source_session.id,
                    &transcoding_temp_file_name(HLS_TRANSCODED_RESOURCE_ID),
                )
                .expect("transcode temporary path should be valid")
                .exists()
        );
    }

    #[cfg(unix)]
    #[test]
    fn usage_snapshot_removes_stale_transcode_output_for_ready_manifest() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let source_session =
            sample_transcoding_ready_session("session-stale-transcode", "https://example.test");
        let completed_session =
            transcoded_completed_session(&source_session, fake_mp4().len() as u64);
        store
            .save_session(&source_session)
            .expect("ready manifest should save");
        std::fs::write(
            store
                .resource_path(&source_session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated resource path should be valid"),
            fake_mp4(),
        )
        .expect("generated resource should be written");
        write_pretty_json(
            &store
                .resource_metadata_path(&source_session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated metadata path should be valid"),
            &cached_metadata_for_session(&completed_session, HLS_TRANSCODED_RESOURCE_ID),
        );
        std::fs::write(
            store
                .resource_path(
                    &source_session.id,
                    &transcoding_temp_file_name(HLS_TRANSCODED_RESOURCE_ID),
                )
                .expect("transcode temporary path should be valid"),
            b"stale ffmpeg output",
        )
        .expect("transcode temporary file should be written");
        let _guard = HlsTranscodingCommitGuard::create_if_needed(&store, &source_session)
            .expect("stale transcoding marker should be created");
        let marker_path = store
            .transcoding_commit_marker_path(&source_session.id)
            .expect("transcoding marker path should be valid");
        set_file_modified_time(
            &marker_path,
            SystemTime::now()
                .checked_sub(HLS_TRANSCODING_COMMIT_MARKER_TTL + Duration::from_secs(1))
                .expect("test stale marker time should be valid"),
        );

        let usage = store
            .usage_snapshot()
            .expect("usage snapshot should clean stale transcode output");

        assert_eq!(0, usage.completed_session_count);
        assert_eq!(0, usage.used_bytes);
        assert!(
            !store
                .resource_path(&source_session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated resource path should be valid")
                .exists()
        );
        assert!(
            !store
                .resource_metadata_path(&source_session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated metadata path should be valid")
                .exists()
        );
        assert!(
            !store
                .resource_path(
                    &source_session.id,
                    &transcoding_temp_file_name(HLS_TRANSCODED_RESOURCE_ID),
                )
                .expect("transcode temporary path should be valid")
                .exists()
        );
    }

    #[test]
    fn usage_snapshot_removes_abandoned_transcode_output_for_ready_manifest() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let source_session =
            sample_transcoding_ready_session("session-abandoned-transcode", "https://example.test");
        let completed_session =
            transcoded_completed_session(&source_session, fake_mp4().len() as u64);
        store
            .save_session(&source_session)
            .expect("ready manifest should save");
        std::fs::write(
            store
                .resource_path(&source_session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated resource path should be valid"),
            fake_mp4(),
        )
        .expect("generated resource should be written");
        write_pretty_json(
            &store
                .resource_metadata_path(&source_session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated metadata path should be valid"),
            &cached_metadata_for_session(&completed_session, HLS_TRANSCODED_RESOURCE_ID),
        );
        std::fs::write(
            store
                .resource_path(
                    &source_session.id,
                    &transcoding_temp_file_name(HLS_TRANSCODED_RESOURCE_ID),
                )
                .expect("transcode temporary path should be valid"),
            b"abandoned ffmpeg output",
        )
        .expect("transcode temporary file should be written");

        let usage = store
            .usage_snapshot()
            .expect("usage snapshot should clean abandoned transcode output");

        assert_eq!(0, usage.completed_session_count);
        assert_eq!(0, usage.used_bytes);
        assert!(
            !store
                .resource_path(&source_session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated resource path should be valid")
                .exists()
        );
        assert!(
            !store
                .resource_metadata_path(&source_session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated metadata path should be valid")
                .exists()
        );
        assert!(
            !store
                .resource_path(
                    &source_session.id,
                    &transcoding_temp_file_name(HLS_TRANSCODED_RESOURCE_ID),
                )
                .expect("transcode temporary path should be valid")
                .exists()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn transcoding_failure_does_not_expose_original_ready_session_as_completed() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_transcoding_ready_session("session-transcode-fail", &upstream_url);
        let config = HlsTranscodingExecutionConfig {
            ffmpeg_path: write_failing_fake_ffmpeg(temp.path()),
            permits: Arc::new(Semaphore::new(1)),
            active_job_count: Arc::new(AtomicUsize::new(0)),
        };
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources_completion_with_control(
                &client,
                &session,
                || HlsCacheFillControl::Continue,
                |_| {},
                Some(config),
            )
            .await
            .expect_err("ffmpeg failure should fail cache completion");

        assert!(
            matches!(error, HlsCacheError::InvalidResource(message) if message.contains("LAN transcoding ffmpeg failed"))
        );
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-transcode-fail")
                .is_none()
        );
        assert!(
            store
                .partial_cache_entries()
                .expect("partial entries should scan")
                .iter()
                .any(|entry| entry.session_id == "session-transcode-fail")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn transcoding_cancellation_cleans_session_and_releases_active_job() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_transcoding_ready_session("session-transcode-cancel", &upstream_url);
        let active_job_count = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicUsize::new(0));
        let config = HlsTranscodingExecutionConfig {
            ffmpeg_path: write_blocking_fake_ffmpeg(temp.path()),
            permits: Arc::new(Semaphore::new(1)),
            active_job_count: Arc::clone(&active_job_count),
        };
        let client = reqwest::Client::new();
        let store_for_task = store.clone();
        let session_for_task = session.clone();
        let cancel_for_task = Arc::clone(&cancel);
        let mut task = tokio::spawn(async move {
            store_for_task
                .cache_session_resources_completion_with_control(
                    &client,
                    &session_for_task,
                    move || {
                        if cancel_for_task.load(Ordering::SeqCst) == 0 {
                            HlsCacheFillControl::Continue
                        } else {
                            HlsCacheFillControl::Cancel
                        }
                    },
                    |_| {},
                    Some(config),
                )
                .await
        });

        let ffmpeg_started_path = temp.path().join("ffmpeg-started");
        tokio::select! {
            () = wait_for_path(&ffmpeg_started_path) => {}
            result = &mut task => {
                panic!("transcoding task finished before fake ffmpeg started: {result:?}");
            }
        }
        assert_eq!(1, active_job_count.load(Ordering::SeqCst));
        cancel.store(1, Ordering::SeqCst);
        let error = task
            .await
            .expect("transcoding task should not panic")
            .expect_err("cancelled ffmpeg should fail with cancellation");

        assert!(matches!(error, HlsCacheError::Cancelled));
        assert_eq!(0, active_job_count.load(Ordering::SeqCst));
        assert!(store.playback_session("session-transcode-cancel").is_none());
    }

    #[tokio::test]
    async fn usage_snapshot_counts_completed_hls_cache_entries() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let client = reqwest::Client::new();

        store
            .cache_session_resources(&client, &sample_session("session-a", &upstream_url))
            .await
            .expect("first session should cache");
        store
            .cache_session_resources(&client, &sample_session("session-b", &upstream_url))
            .await
            .expect("second session should cache");

        let usage = store
            .usage_snapshot()
            .expect("usage snapshot should scan completed cache");
        let entries = store
            .completed_cache_entries()
            .expect("completed cache entries should scan");

        assert_eq!(2, usage.completed_session_count);
        assert_eq!(2 * fake_mp4().len() as u64, usage.used_bytes);
        assert_eq!(vec!["session-a", "session-b"], session_ids(&entries));
    }

    #[tokio::test]
    async fn usage_snapshot_counts_partial_hls_cache_resources() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session_with_audio("session-partial-usage", &upstream_url);
        let client = reqwest::Client::new();
        let store_for_preempt = store.clone();
        let session_id = session.id.clone();

        let error = store
            .cache_session_resources_with_control(
                &client,
                &session,
                move || {
                    if store_for_preempt
                        .cached_resource(&session_id, "video.m4s")
                        .is_some()
                    {
                        HlsCacheFillControl::Preempt
                    } else {
                        HlsCacheFillControl::Continue
                    }
                },
                |_| {},
            )
            .await
            .expect_err("preempted session should leave partial cache resources");
        assert!(matches!(error, HlsCacheError::Preempted));

        let usage = store
            .usage_snapshot()
            .expect("usage snapshot should count partial cache resources");
        assert_eq!(0, usage.completed_session_count);
        assert_eq!(fake_mp4().len() as u64, usage.used_bytes);
    }

    #[tokio::test]
    async fn projected_remaining_size_excludes_managed_partial_resources() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session_with_audio("session-partial-projection", &upstream_url);
        let resource_size = fake_mp4().len() as u64;
        session.variant.video.request.size = Some(resource_size);
        session
            .variant
            .audio
            .as_mut()
            .expect("sample session should include audio")
            .request
            .size = Some(resource_size);
        let client = reqwest::Client::new();
        let store_for_preempt = store.clone();
        let session_id = session.id.clone();

        let error = store
            .cache_session_resources_with_control(
                &client,
                &session,
                move || {
                    if store_for_preempt
                        .cached_resource(&session_id, "video.m4s")
                        .is_some()
                    {
                        HlsCacheFillControl::Preempt
                    } else {
                        HlsCacheFillControl::Continue
                    }
                },
                |_| {},
            )
            .await
            .expect_err("preempted session should leave partial cache resources");
        assert!(matches!(error, HlsCacheError::Preempted));

        assert_eq!(
            Some(resource_size),
            store.session_projected_remaining_size_bytes(&session)
        );
    }

    #[tokio::test]
    async fn projected_remaining_size_does_not_subtract_prewarmed_prefix() {
        let (upstream_url, _task) = start_prewarm_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-prewarm-projection", &upstream_url);
        let resource_size = fake_mp4().len() as u64;
        session.variant.video.request.size = Some(resource_size);
        let client = reqwest::Client::new();

        store
            .prewarm_session_first_frame_with_control(&client, &session, || {
                HlsCacheFillControl::Continue
            })
            .await
            .expect("session should prewarm");

        assert!(store.prewarmed_resource(&session.id, "video.m4s").is_some());
        assert_eq!(
            Some(resource_size),
            store.session_projected_remaining_size_bytes(&session)
        );
    }

    #[test]
    fn finalization_projection_includes_generated_transcode_output() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_transcoding_ready_session(
            "session-transcode-projection",
            "https://example.test/video.m4s",
        );
        let source_size = fake_mp4().len() as u64;
        session.variant.video.request.size = Some(source_size);
        session
            .variant
            .audio
            .as_mut()
            .expect("sample transcoding session should include audio")
            .request
            .size = Some(source_size);

        store
            .save_session(&session)
            .expect("session should be persisted");
        for resource in session
            .variant
            .audio
            .iter()
            .chain(std::iter::once(&session.variant.video))
        {
            let path = store
                .resource_path(&session.id, &resource.id)
                .expect("resource path should be valid");
            std::fs::create_dir_all(path.parent().expect("resource should have a parent"))
                .expect("session cache directory should be created");
            std::fs::write(&path, fake_mp4()).expect("cached resource should be written");
            write_pretty_json(
                &store
                    .resource_metadata_path(&session.id, &resource.id)
                    .expect("metadata path should be valid"),
                &PersistedHlsCachedResource {
                    schema_version: HLS_CACHE_SCHEMA_VERSION,
                    id: resource.id.clone(),
                    content_type: resource.content_type().to_owned(),
                    total_length: source_size,
                    initialization_length: 28,
                    segments: Vec::new(),
                    cache_key: PersistedBilibiliMediaCacheKey::from(
                        resource.request.cache_key.clone(),
                    ),
                },
            );
        }

        let expected_transcoded_output_bytes =
            u64::from(session.variant.duration_seconds) * transcoded_bandwidth(true) / 8;

        assert_eq!(
            Some(0),
            store.session_projected_remaining_size_bytes(&session)
        );
        assert_eq!(
            Some(expected_transcoded_output_bytes),
            store.session_projected_finalization_added_size_bytes(&session)
        );
    }

    #[tokio::test]
    async fn prewarm_records_first_window_target_metadata() {
        let (upstream_url, _task) = start_prewarm_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-prewarm-window", &upstream_url);
        let client = reqwest::Client::new();

        store
            .prewarm_session_first_frame_with_control(&client, &session, || {
                HlsCacheFillControl::Continue
            })
            .await
            .expect("session should prewarm");
        let prewarmed = store
            .prewarmed_resource(&session.id, "video.m4s")
            .expect("prewarm metadata should load");

        assert_eq!(fake_mp4().len() as u64, prewarmed.prefix_length);
        assert_eq!(
            hls_first_window_prefetch_prefix_bytes(&session.variant.video),
            prewarmed.target_prefix_length
        );
        assert!(prewarmed.target_prefix_length > prewarmed.prefix_length);
        assert_eq!(
            HLS_FIRST_WINDOW_PREFETCH_SECONDS,
            prewarmed.target_window_seconds
        );
    }

    #[test]
    fn prewarmed_resource_loads_legacy_metadata_without_target_fields() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-legacy-prewarm", "https://example.test/video.m4s");
        store
            .save_session(&session)
            .expect("session manifest should save");
        std::fs::write(
            store
                .resource_prewarm_path(&session.id, "video.m4s")
                .expect("prewarm resource path should be valid"),
            fake_mp4(),
        )
        .expect("prewarm resource should be written");
        write_pretty_json(
            &store
                .resource_prewarm_metadata_path(&session.id, "video.m4s")
                .expect("prewarm metadata path should be valid"),
            &serde_json::json!({
                "schema_version": HLS_CACHE_SCHEMA_VERSION,
                "id": "video.m4s",
                "content_type": session.variant.video.content_type(),
                "prefix_length": fake_mp4().len() as u64,
                "total_length": fake_mp4().len() as u64,
                "initialization_length": 28,
                "cache_key": PersistedBilibiliMediaCacheKey::from(
                    session.variant.video.request.cache_key.clone(),
                ),
            }),
        );

        let prewarmed = store
            .prewarmed_resource(&session.id, "video.m4s")
            .expect("legacy prewarm metadata should load");

        assert_eq!(fake_mp4().len() as u64, prewarmed.prefix_length);
        assert_eq!(prewarmed.prefix_length, prewarmed.target_prefix_length);
        assert_eq!(
            HLS_FIRST_WINDOW_PREFETCH_SECONDS,
            prewarmed.target_window_seconds
        );
    }

    #[tokio::test]
    async fn prewarm_fetches_bandwidth_sized_first_window_prefix() {
        let (upstream_url, _task) = start_large_prewarm_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-large-prewarm-window", &upstream_url);
        session.variant.video.request.size = Some(large_prefetch_fake_mp4().len() as u64);
        session.variant.video.request.bandwidth = Some(800_000);
        let target_prefix_length = hls_first_window_prefetch_prefix_bytes(&session.variant.video);
        let client = reqwest::Client::new();

        store
            .prewarm_session_first_frame_with_control(&client, &session, || {
                HlsCacheFillControl::Continue
            })
            .await
            .expect("session should prewarm first window");
        let prewarmed = store
            .prewarmed_resource(&session.id, "video.m4s")
            .expect("prewarm metadata should load");

        assert!(target_prefix_length > HLS_PREWARM_HEAD_BYTES);
        assert_eq!(target_prefix_length, prewarmed.prefix_length);
        assert_eq!(target_prefix_length, prewarmed.target_prefix_length);
        assert_eq!(
            large_prefetch_fake_mp4().len() as u64,
            prewarmed.total_length
        );
    }

    #[tokio::test]
    async fn prewarm_refetches_legacy_prefix_below_first_window_target() {
        let (upstream_url, _task) = start_large_prewarm_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-legacy-window-upgrade", &upstream_url);
        session.variant.video.request.size = Some(large_prefetch_fake_mp4().len() as u64);
        session.variant.video.request.bandwidth = Some(800_000);
        store
            .save_session(&session)
            .expect("session manifest should save");
        std::fs::write(
            store
                .resource_prewarm_path(&session.id, "video.m4s")
                .expect("prewarm resource path should be valid"),
            &large_prefetch_fake_mp4()[..HLS_PREWARM_HEAD_BYTES as usize],
        )
        .expect("legacy prewarm resource should be written");
        write_pretty_json(
            &store
                .resource_prewarm_metadata_path(&session.id, "video.m4s")
                .expect("prewarm metadata path should be valid"),
            &serde_json::json!({
                "schema_version": HLS_CACHE_SCHEMA_VERSION,
                "id": "video.m4s",
                "content_type": session.variant.video.content_type(),
                "prefix_length": HLS_PREWARM_HEAD_BYTES,
                "total_length": large_prefetch_fake_mp4().len() as u64,
                "initialization_length": 28,
                "cache_key": PersistedBilibiliMediaCacheKey::from(
                    session.variant.video.request.cache_key.clone(),
                ),
            }),
        );
        let target_prefix_length = hls_first_window_prefetch_prefix_bytes(&session.variant.video);
        let client = reqwest::Client::new();

        store
            .prewarm_session_first_frame_with_control(&client, &session, || {
                HlsCacheFillControl::Continue
            })
            .await
            .expect("legacy prewarm should upgrade to first-window target");
        let prewarmed = store
            .prewarmed_resource(&session.id, "video.m4s")
            .expect("prewarm metadata should load");

        assert!(target_prefix_length > HLS_PREWARM_HEAD_BYTES);
        assert_eq!(target_prefix_length, prewarmed.prefix_length);
        assert_eq!(target_prefix_length, prewarmed.target_prefix_length);
    }

    #[tokio::test]
    async fn prewarm_keeps_legacy_prefix_when_upgrade_download_fails() {
        let (upstream_url, _task) = start_invalid_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-legacy-window-upgrade-failure", &upstream_url);
        session.variant.video.request.size = Some(large_prefetch_fake_mp4().len() as u64);
        session.variant.video.request.bandwidth = Some(800_000);
        store
            .save_session(&session)
            .expect("session manifest should save");
        std::fs::write(
            store
                .resource_prewarm_path(&session.id, "video.m4s")
                .expect("prewarm resource path should be valid"),
            &large_prefetch_fake_mp4()[..HLS_PREWARM_HEAD_BYTES as usize],
        )
        .expect("legacy prewarm resource should be written");
        write_pretty_json(
            &store
                .resource_prewarm_metadata_path(&session.id, "video.m4s")
                .expect("prewarm metadata path should be valid"),
            &serde_json::json!({
                "schema_version": HLS_CACHE_SCHEMA_VERSION,
                "id": "video.m4s",
                "content_type": session.variant.video.content_type(),
                "prefix_length": HLS_PREWARM_HEAD_BYTES,
                "total_length": large_prefetch_fake_mp4().len() as u64,
                "initialization_length": 28,
                "cache_key": PersistedBilibiliMediaCacheKey::from(
                    session.variant.video.request.cache_key.clone(),
                ),
            }),
        );
        let target_prefix_length = hls_first_window_prefetch_prefix_bytes(&session.variant.video);
        let client = reqwest::Client::new();

        let error = store
            .prewarm_session_first_frame_with_control(&client, &session, || {
                HlsCacheFillControl::Continue
            })
            .await
            .expect_err("failed prewarm upgrade should surface the upstream error");
        let prewarmed = store
            .prewarmed_resource(&session.id, "video.m4s")
            .expect("legacy prewarm metadata should remain loadable");

        assert!(error.to_string().contains("expected partial content"));
        assert!(target_prefix_length > HLS_PREWARM_HEAD_BYTES);
        assert_eq!(HLS_PREWARM_HEAD_BYTES, prewarmed.prefix_length);
        assert_eq!(prewarmed.prefix_length, prewarmed.target_prefix_length);
    }

    #[test]
    fn first_window_prefetch_target_uses_bandwidth_window() {
        let mut session =
            sample_session("session-prefetch-target", "https://example.test/video.m4s");
        session.variant.video.request.size = Some(10 * 1024 * 1024);
        session.variant.video.request.bandwidth = Some(800_000);

        assert_eq!(
            HLS_PREWARM_HEAD_BYTES + 3_000_000,
            hls_first_window_prefetch_prefix_bytes(&session.variant.video)
        );
    }

    #[test]
    fn first_window_prefetch_target_clamps_to_resource_size_and_maximum() {
        let mut session =
            sample_session("session-prefetch-clamp", "https://example.test/video.m4s");
        session.variant.video.request.size = Some(512 * 1024);
        session.variant.video.request.bandwidth = Some(800_000);
        assert_eq!(
            512 * 1024,
            hls_first_window_prefetch_prefix_bytes(&session.variant.video)
        );

        session.variant.video.request.size = Some(20 * 1024 * 1024);
        session.variant.video.request.bandwidth = Some(20_000_000);
        assert_eq!(
            HLS_FIRST_WINDOW_PREFETCH_MAX_BYTES,
            hls_first_window_prefetch_prefix_bytes(&session.variant.video)
        );
    }

    #[test]
    fn playback_position_prefetch_target_extends_from_reported_position() {
        let mut session = sample_session(
            "session-position-prefetch",
            "https://example.test/video.m4s",
        );
        session.variant.video.request.size = Some(64 * 1024 * 1024);
        session.variant.video.request.bandwidth = Some(800_000);
        let context = HlsPlaybackPrefetchContext {
            position_seconds: 90.0,
            duration_seconds: Some(180.0),
            last_intent: PlaybackProgressIntent::Seek,
        };

        let target = hls_prefetch_prefix_target(&session.variant.video, Some(&context));

        assert_eq!(120, target.window_seconds);
        assert_eq!(HLS_PREWARM_HEAD_BYTES + 12_000_000, target.prefix_bytes);
        assert!(target.prefix_bytes > HLS_FIRST_WINDOW_PREFETCH_MAX_BYTES);
    }

    #[test]
    fn playback_position_prefetch_target_keeps_first_window_maximum_at_start() {
        let mut session = sample_session(
            "session-position-prefetch-start",
            "https://example.test/video.m4s",
        );
        session.variant.video.request.size = Some(64 * 1024 * 1024);
        session.variant.video.request.bandwidth = Some(20_000_000);
        let context = HlsPlaybackPrefetchContext {
            position_seconds: 0.0,
            duration_seconds: Some(180.0),
            last_intent: PlaybackProgressIntent::Playing,
        };

        let target = hls_prefetch_prefix_target(&session.variant.video, Some(&context));

        assert_eq!(HLS_FIRST_WINDOW_PREFETCH_SECONDS, target.window_seconds);
        assert_eq!(HLS_FIRST_WINDOW_PREFETCH_MAX_BYTES, target.prefix_bytes);
    }

    #[test]
    fn playback_position_prefetch_target_keeps_first_window_maximum_near_start() {
        let mut session = sample_session(
            "session-position-prefetch-near-start",
            "https://example.test/video.m4s",
        );
        session.variant.video.request.size = Some(64 * 1024 * 1024);
        session.variant.video.request.bandwidth = Some(20_000_000);
        let context = HlsPlaybackPrefetchContext {
            position_seconds: 0.1,
            duration_seconds: Some(180.0),
            last_intent: PlaybackProgressIntent::Playing,
        };

        let target = hls_prefetch_prefix_target(&session.variant.video, Some(&context));

        assert_eq!(HLS_FIRST_WINDOW_PREFETCH_SECONDS, target.window_seconds);
        assert_eq!(HLS_FIRST_WINDOW_PREFETCH_MAX_BYTES, target.prefix_bytes);
    }

    #[test]
    fn playback_position_prefetch_target_clamps_to_duration_and_position_maximum() {
        let mut session = sample_session(
            "session-position-prefetch-clamp",
            "https://example.test/video.m4s",
        );
        session.variant.video.request.size = Some(64 * 1024 * 1024);
        session.variant.video.request.bandwidth = Some(20_000_000);
        let context = HlsPlaybackPrefetchContext {
            position_seconds: 150.0,
            duration_seconds: Some(160.0),
            last_intent: PlaybackProgressIntent::Seek,
        };

        let target = hls_prefetch_prefix_target(&session.variant.video, Some(&context));

        assert_eq!(160, target.window_seconds);
        assert_eq!(
            HLS_PLAYBACK_POSITION_PREFETCH_MAX_BYTES,
            target.prefix_bytes
        );
    }

    #[test]
    fn playback_position_prefetch_context_requires_active_matching_session_and_variant() {
        let session = sample_session("session-position-context", "https://example.test/video.m4s");
        let matching = HlsPlaybackProgressSnapshot {
            state: HlsPlaybackActivityState::Active,
            message: String::new(),
            session_id: session.id.clone(),
            library_item_id: String::new(),
            variant_id: session.variant.id.clone(),
            playback_uri: String::new(),
            position_seconds: 42.0,
            duration_seconds: Some(120.0),
            last_intent: PlaybackProgressIntent::Playing,
            updated_at: SystemTime::UNIX_EPOCH,
        };
        let stopped = HlsPlaybackProgressSnapshot {
            state: HlsPlaybackActivityState::RecentlyStopped,
            ..matching.clone()
        };
        let other_variant = HlsPlaybackProgressSnapshot {
            variant_id: "hevc".to_owned(),
            ..matching.clone()
        };

        assert_eq!(
            Some(HlsPlaybackPrefetchContext {
                position_seconds: 42.0,
                duration_seconds: Some(120.0),
                last_intent: PlaybackProgressIntent::Playing,
            }),
            hls_playback_prefetch_context(&session, Some(&matching))
        );
        assert_eq!(
            None,
            hls_playback_prefetch_context(&session, Some(&stopped))
        );
        assert_eq!(
            None,
            hls_playback_prefetch_context(&session, Some(&other_variant))
        );
    }

    #[tokio::test]
    async fn prewarm_upgrades_first_window_prefix_after_playback_position_report() {
        let (upstream_url, _task) = start_position_prewarm_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-position-window-upgrade", &upstream_url);
        session.variant.video.request.size = Some(position_prefetch_fake_mp4().len() as u64);
        session.variant.video.request.bandwidth = Some(800_000);
        let client = reqwest::Client::new();

        store
            .prewarm_session_first_frame_with_control(&client, &session, || {
                HlsCacheFillControl::Continue
            })
            .await
            .expect("session should prewarm first window");
        let first_window = store
            .prewarmed_resource(&session.id, "video.m4s")
            .expect("first prewarm metadata should load");

        let playback_progress = HlsPlaybackProgressSnapshot {
            state: HlsPlaybackActivityState::Active,
            message: String::new(),
            session_id: session.id.clone(),
            library_item_id: String::new(),
            variant_id: session.variant.id.clone(),
            playback_uri: String::new(),
            position_seconds: 90.0,
            duration_seconds: Some(180.0),
            last_intent: PlaybackProgressIntent::Seek,
            updated_at: SystemTime::UNIX_EPOCH,
        };
        store
            .prewarm_session_first_frame_with_playback_progress(
                &client,
                &session,
                Some(&playback_progress),
                || HlsCacheFillControl::Continue,
            )
            .await
            .expect("session should upgrade prewarm around playback position");
        let positioned = store
            .prewarmed_resource(&session.id, "video.m4s")
            .expect("position prewarm metadata should load");

        assert_eq!(
            HLS_FIRST_WINDOW_PREFETCH_SECONDS,
            first_window.target_window_seconds
        );
        assert_eq!(120, positioned.target_window_seconds);
        assert!(positioned.prefix_length > first_window.prefix_length);
        assert_eq!(
            hls_prefetch_prefix_target(
                &session.variant.video,
                hls_playback_prefetch_context(&session, Some(&playback_progress)).as_ref(),
            )
            .prefix_bytes,
            positioned.prefix_length
        );
    }

    #[test]
    fn hls_eviction_policy_derives_watermark_bytes() {
        let policy = HlsCacheEvictionPolicy {
            max_bytes: 1_000,
            high_watermark_percent: 90,
            low_watermark_percent: 80,
        };

        assert!(policy.eviction_enabled());
        assert_eq!(900, policy.high_watermark_bytes());
        assert_eq!(800, policy.low_watermark_bytes());
    }

    #[test]
    fn declared_session_size_requires_all_resource_sizes() {
        let mut session =
            sample_session_with_audio("session-sized", "https://example.test/video.m4s");
        session.variant.video.request.size = Some(10);
        session
            .variant
            .audio
            .as_mut()
            .expect("sample should include audio")
            .request
            .size = Some(3);

        assert_eq!(Some(13), hls_session_declared_size_bytes(&session));
        session.variant.video.request.size = None;
        assert_eq!(None, hls_session_declared_size_bytes(&session));
    }

    #[tokio::test]
    async fn cached_resource_rejects_mismatched_request_cache_key() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-cache-key", &upstream_url);
        let client = reqwest::Client::new();
        let item_id = store
            .cache_session_resources(&client, &session)
            .await
            .expect("session resources should cache");
        assert!(
            store
                .cached_resource("session-cache-key", "video.m4s")
                .is_some()
        );
        session.variant.video.request.cache_key.source_hash = "different-source".to_owned();
        store
            .write_json_atomically(
                &store
                    .session_dir("session-cache-key")
                    .expect("session directory should resolve")
                    .join("session.json"),
                &PersistedHlsSession::from(session),
            )
            .expect("tampered persisted session fixture should be written");

        assert!(
            store
                .cached_resource("session-cache-key", "video.m4s")
                .is_none()
        );
        assert!(store.get_completed_library_item(&item_id).is_none());
    }

    #[tokio::test]
    async fn cached_resource_rejects_invalid_initialization_length_metadata() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-invalid-init-range", &upstream_url);
        let client = reqwest::Client::new();
        let item_id = store
            .cache_session_resources(&client, &session)
            .await
            .expect("session resources should cache");
        let metadata_path = store
            .resource_metadata_path(&session.id, "video.m4s")
            .expect("metadata path should be valid");
        let mut metadata = cached_metadata_for_session(&session, "video.m4s");
        metadata.initialization_length = metadata.total_length;
        write_pretty_json(&metadata_path, &metadata);

        assert!(store.cached_resource(&session.id, "video.m4s").is_none());
        assert!(store.get_completed_library_item(&item_id).is_none());

        metadata.initialization_length = 0;
        write_pretty_json(&metadata_path, &metadata);

        assert!(store.cached_resource(&session.id, "video.m4s").is_none());
        assert!(store.get_completed_library_item(&item_id).is_none());
    }

    #[test]
    fn cached_resource_reads_persisted_segment_ranges() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-segment-index", "https://example.test/video.m4s");
        store
            .save_session(&session)
            .expect("session manifest should save");
        let resource_path = store
            .resource_path(&session.id, "video.m4s")
            .expect("resource path should be valid");
        let mp4 = multi_fragment_fake_mp4();
        let segments = multi_fragment_fake_mp4_segments();
        std::fs::write(&resource_path, &mp4).expect("cached resource should be written");
        let metadata_path = store
            .resource_metadata_path(&session.id, "video.m4s")
            .expect("metadata path should be valid");
        let mut metadata = cached_metadata_for_session(&session, "video.m4s");
        metadata.total_length = mp4.len() as u64;
        metadata.initialization_length = multi_fragment_fake_mp4_initialization_length();
        metadata.segments = segments.clone();
        write_pretty_json(&metadata_path, &metadata);

        let cached = store
            .cached_resource(&session.id, "video.m4s")
            .expect("cached resource should load");

        assert_eq!(
            segments
                .into_iter()
                .map(|segment| HlsMediaSegment {
                    byte_range_offset: segment.byte_range_offset,
                    byte_range_length: segment.byte_range_length,
                    duration_millis: segment.duration_millis,
                })
                .collect::<Vec<_>>(),
            cached.segments
        );
    }

    #[test]
    fn cached_resource_accepts_legacy_metadata_without_segment_ranges() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session(
            "session-legacy-segment-index",
            "https://example.test/video.m4s",
        );
        store
            .save_session(&session)
            .expect("session manifest should save");
        let resource_path = store
            .resource_path(&session.id, "video.m4s")
            .expect("resource path should be valid");
        std::fs::write(&resource_path, fake_mp4()).expect("cached resource should be written");
        let metadata_path = store
            .resource_metadata_path(&session.id, "video.m4s")
            .expect("metadata path should be valid");
        write_pretty_json(
            &metadata_path,
            &serde_json::json!({
                "schema_version": HLS_CACHE_SCHEMA_VERSION,
                "id": "video.m4s",
                "content_type": session.variant.video.content_type(),
                "total_length": fake_mp4().len() as u64,
                "initialization_length": 28,
                "cache_key": PersistedBilibiliMediaCacheKey::from(
                    session.variant.video.request.cache_key.clone(),
                ),
            }),
        );

        let cached = store
            .cached_resource(&session.id, "video.m4s")
            .expect("legacy cached resource should load");

        assert!(cached.segments.is_empty());
    }

    #[test]
    fn cached_resource_drops_invalid_persisted_segment_ranges() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session(
            "session-invalid-segment-index",
            "https://example.test/video.m4s",
        );
        store
            .save_session(&session)
            .expect("session manifest should save");
        let resource_path = store
            .resource_path(&session.id, "video.m4s")
            .expect("resource path should be valid");
        let mp4 = multi_fragment_fake_mp4();
        std::fs::write(&resource_path, &mp4).expect("cached resource should be written");
        let metadata_path = store
            .resource_metadata_path(&session.id, "video.m4s")
            .expect("metadata path should be valid");
        let mut metadata = cached_metadata_for_session(&session, "video.m4s");
        metadata.total_length = mp4.len() as u64;
        metadata.initialization_length = multi_fragment_fake_mp4_initialization_length();
        metadata.segments = vec![
            PersistedHlsMediaSegment {
                byte_range_offset: metadata.initialization_length,
                byte_range_length: 10,
                duration_millis: 1_000,
            },
            PersistedHlsMediaSegment {
                byte_range_offset: metadata.initialization_length + 20,
                byte_range_length: 10,
                duration_millis: 1_000,
            },
        ];
        write_pretty_json(&metadata_path, &metadata);

        let cached = store
            .cached_resource(&session.id, "video.m4s")
            .expect("cached resource should load without invalid segment index");

        assert!(cached.segments.is_empty());
    }

    #[tokio::test]
    async fn rejects_short_hls_cache_response_with_declared_size() {
        let (upstream_url, _task) = start_short_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-short", &upstream_url);
        session.variant.video.request.size = Some(fake_mp4().len() as u64);
        let temp_path = store
            .resource_range_full_get_temp_path(&session.id, "video.m4s")
            .expect("compatibility temp path should be valid");
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("short response should be rejected");

        assert!(matches!(
            error,
            HlsCacheError::Range(HlsRangeError::InvalidResponse(_))
        ));
        assert!(!temp_path.exists());
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-short")
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_lengthless_chunked_hls_cache_response() {
        let (upstream_url, _task) = start_overlong_chunked_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-lengthless", &upstream_url);
        let temp_path = store
            .resource_range_full_get_temp_path("session-lengthless", "video.m4s")
            .expect("compatibility temp path should be valid");
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("lengthless response should be rejected");

        assert!(matches!(
            error,
            HlsCacheError::Range(HlsRangeError::InvalidResponse(_))
        ));
        assert!(!temp_path.exists());
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-lengthless")
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_overlong_chunked_hls_cache_response_with_expected_size() {
        let (upstream_url, _task) = start_overlong_chunked_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-overlong", &upstream_url);
        session.variant.video.request.size = Some(fake_mp4().len() as u64);
        let temp_path = store
            .resource_range_full_get_temp_path("session-overlong", "video.m4s")
            .expect("compatibility temp path should be valid");
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("overlong response should be rejected");

        assert!(matches!(
            error,
            HlsCacheError::Range(HlsRangeError::InvalidResponse(_))
        ));
        assert!(!temp_path.exists());
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-overlong")
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_unsolicited_partial_hls_cache_response() {
        let (upstream_url, _task) = start_partial_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-partial", &upstream_url);
        session.variant.video.request.size = Some(fake_mp4().len() as u64);
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("partial response should be rejected");

        assert!(matches!(
            error,
            HlsCacheError::Range(HlsRangeError::InvalidResponse(_))
        ));
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-partial")
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_range_only_hls_cache_prewarm_request() {
        let (upstream_url, _task) = start_prewarm_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-prewarm-range", &upstream_url);
        session
            .variant
            .video
            .request
            .headers
            .push(BilibiliHttpHeader {
                name: "range".to_owned(),
                value: "bytes=128-255".to_owned(),
            });
        let client = reqwest::Client::new();

        let error = store
            .prewarm_session_first_frame_with_control(&client, &session, || {
                HlsCacheFillControl::Continue
            })
            .await
            .expect_err("range-only prewarm should be rejected");

        assert!(error.to_string().contains("range-only"));
        assert!(store.prewarmed_resource(&session.id, "video.m4s").is_none());
        let usage = store
            .usage_snapshot()
            .expect("usage should not count rejected prewarm");
        assert_eq!(0, usage.used_bytes);
    }

    #[tokio::test]
    async fn removes_temp_file_when_cached_initialization_is_invalid() {
        let (upstream_url, _task) = start_invalid_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-invalid", &upstream_url);
        session.variant.video.request.size = Some(invalid_mp4().len() as u64);
        let temp_path = store
            .resource_range_full_get_temp_path(&session.id, "video.m4s")
            .expect("compatibility temp path should be valid");
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("invalid MP4 should be rejected");

        assert!(matches!(
            error,
            HlsCacheError::Range(HlsRangeError::InvalidResponse(_))
        ));
        assert!(!temp_path.exists());
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-invalid")
                .is_none()
        );
    }

    #[tokio::test]
    async fn tries_backup_url_after_cached_initialization_is_invalid() {
        let (primary_url, _primary_task) = start_invalid_mp4_upstream().await;
        let (backup_url, _backup_task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-backup", &primary_url);
        session.variant.video.request.backup_urls = vec![backup_url];
        let temp_path = store
            .resource_range_full_get_temp_path(&session.id, "video.m4s")
            .expect("compatibility temp path should be valid");
        let client = reqwest::Client::new();

        let item_id = store
            .cache_session_resources(&client, &session)
            .await
            .expect("backup URL should cache after invalid primary");
        let cached = store
            .cached_resource("session-backup", "video.m4s")
            .expect("backup resource should be cached");

        assert_eq!("bilibili.hls.session-backup", item_id);
        assert_eq!(fake_mp4().len() as u64, cached.total_length);
        assert_eq!(28, cached.initialization_length);
        assert!(!temp_path.exists());
    }

    #[tokio::test]
    async fn tries_backup_url_after_primary_returns_401() {
        let (primary_url, _primary_task) = start_hls_cache_upstream(
            Router::new().route("/video.m4s", get(|| async { StatusCode::UNAUTHORIZED })),
        )
        .await;
        let (backup_url, _backup_task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-backup-401", &primary_url);
        session.variant.video.request.backup_urls = vec![backup_url];

        let item_id = store
            .cache_session_resources(&reqwest::Client::new(), &session)
            .await
            .expect("working backup should succeed after primary 401");

        assert_eq!("bilibili.hls.session-backup-401", item_id);
        assert!(store.cached_resource(&session.id, "video.m4s").is_some());
    }

    #[tokio::test]
    async fn does_not_forward_sensitive_headers_to_cross_origin_backup_url() {
        let (primary_url, _primary_task) = start_invalid_mp4_upstream().await;
        let (backup_url, _backup_task) = start_hls_cache_upstream(
            Router::new().route("/video.m4s", get(upstream_mp4_reject_sensitive_headers)),
        )
        .await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-sensitive-backup", &primary_url);
        session.variant.video.request.backup_urls = vec![backup_url];
        session.variant.video.request.headers.extend([
            BilibiliHttpHeader {
                name: "authorization".to_owned(),
                value: "Bearer secret-token".to_owned(),
            },
            BilibiliHttpHeader {
                name: "cookie".to_owned(),
                value: "SESSDATA=secret-cookie".to_owned(),
            },
        ]);
        let client = reqwest::Client::new();

        let item_id = store
            .cache_session_resources(&client, &session)
            .await
            .expect("cross-origin backup should not receive sensitive primary headers");
        let cached = store
            .cached_resource("session-sensitive-backup", "video.m4s")
            .expect("backup resource should be cached");

        assert_eq!("bilibili.hls.session-sensitive-backup", item_id);
        assert_eq!(fake_mp4().len() as u64, cached.total_length);
    }

    #[tokio::test]
    async fn completed_session_manifest_scrubs_upstream_request_data() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-scrubbed", &upstream_url);
        attach_sample_abr_metadata(&mut session);
        attach_sample_alternate_variant(
            &mut session,
            "https://cdn-alt.example.test/720p-video.m4s",
        );
        let backup_url = "https://cdn-backup.example.test/video.m4s".to_owned();
        session.variant.video.request.backup_urls = vec![backup_url.clone()];
        session.variant.video.request.headers.extend([
            BilibiliHttpHeader {
                name: "authorization".to_owned(),
                value: "Bearer secret-token".to_owned(),
            },
            BilibiliHttpHeader {
                name: "cookie".to_owned(),
                value: "SESSDATA=secret-cookie".to_owned(),
            },
        ]);
        let client = reqwest::Client::new();

        let item_id = store
            .cache_session_resources(&client, &session)
            .await
            .expect("session resources should cache");
        let manifest_path = store
            .session_dir("session-scrubbed")
            .expect("session dir should be valid")
            .join("session.json");
        let manifest = std::fs::read_to_string(manifest_path)
            .expect("completed session manifest should remain readable");
        let sessions = store
            .load_sessions()
            .expect("completed session manifest should load");

        assert!(!manifest.contains(&upstream_url));
        assert!(!manifest.contains(&backup_url));
        assert!(!manifest.contains("secret-token"));
        assert!(!manifest.contains("SESSDATA"));
        assert!(!manifest.contains("cdn-alt.example.test"));
        assert!(manifest.contains("dash-video"));
        assert!(manifest.contains("source-hash"));
        assert!(manifest.contains("hevc-source-hash"));
        assert_eq!(1, sessions.len());
        assert_eq!(1, sessions[0].alternate_variants.len());
        let request = &sessions[0].variant.video.request;
        assert!(request.url.is_empty());
        assert!(request.backup_urls.is_empty());
        assert!(request.headers.is_empty());
        let alternate_video_request = &sessions[0].alternate_variants[0].video.request;
        assert!(alternate_video_request.url.is_empty());
        assert!(alternate_video_request.backup_urls.is_empty());
        assert!(alternate_video_request.headers.is_empty());
        let alternate_audio_request = &sessions[0].alternate_variants[0]
            .audio
            .as_ref()
            .expect("alternate audio should remain for lookup")
            .request;
        assert!(alternate_audio_request.url.is_empty());
        assert!(alternate_audio_request.backup_urls.is_empty());
        assert!(alternate_audio_request.headers.is_empty());
        assert!(
            !sessions[0]
                .master_playlist()
                .contains("segments/v1-video.m3u8")
        );
        assert!(
            sessions[0]
                .media_playlist_resource("v1-video.m3u8")
                .is_some()
        );
        assert_eq!(session.abr, sessions[0].abr);
        assert_eq!(session.variants, sessions[0].variants);
        assert!(store.get_completed_library_item(&item_id).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_cached_resource() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-symlink", "https://example.test/video.m4s");
        store
            .save_session(&session)
            .expect("session manifest should save");
        let target_path = temp.path().join("outside.mp4");
        std::fs::write(&target_path, fake_mp4()).expect("target file should be written");
        symlink(
            &target_path,
            store
                .resource_path("session-symlink", "video.m4s")
                .expect("resource path should be valid"),
        )
        .expect("resource symlink should be created");
        let metadata = PersistedHlsCachedResource {
            schema_version: HLS_CACHE_SCHEMA_VERSION,
            id: "video.m4s".to_owned(),
            content_type: session.variant.video.content_type().to_owned(),
            total_length: fake_mp4().len() as u64,
            initialization_length: 28,
            segments: Vec::new(),
            cache_key: PersistedBilibiliMediaCacheKey::from(
                session.variant.video.request.cache_key.clone(),
            ),
        };
        store
            .write_json_atomically(
                &store
                    .resource_metadata_path("session-symlink", "video.m4s")
                    .expect("metadata path should be valid"),
                &metadata,
            )
            .expect("resource metadata should save");

        assert!(
            store
                .cached_resource("session-symlink", "video.m4s")
                .is_none()
        );
        assert!(
            store
                .open_cached_resource("session-symlink", "video.m4s")
                .is_none()
        );
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-symlink")
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_prewarmed_resource() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-prewarm-symlink", "https://example.test/video.m4s");
        store
            .save_session(&session)
            .expect("session manifest should save");
        let target_path = temp.path().join("outside-prewarm.mp4");
        std::fs::write(&target_path, fake_mp4()).expect("target file should be written");
        symlink(
            &target_path,
            store
                .resource_prewarm_path("session-prewarm-symlink", "video.m4s")
                .expect("prewarm resource path should be valid"),
        )
        .expect("prewarm resource symlink should be created");
        let metadata = PersistedHlsPrewarmedResource {
            schema_version: HLS_CACHE_SCHEMA_VERSION,
            id: "video.m4s".to_owned(),
            content_type: session.variant.video.content_type().to_owned(),
            prefix_length: fake_mp4().len() as u64,
            target_prefix_length: Some(fake_mp4().len() as u64),
            target_window_seconds: Some(HLS_FIRST_WINDOW_PREFETCH_SECONDS),
            total_length: fake_mp4().len() as u64,
            initialization_length: 28,
            cache_key: PersistedBilibiliMediaCacheKey::from(
                session.variant.video.request.cache_key.clone(),
            ),
        };
        store
            .write_json_atomically(
                &store
                    .resource_prewarm_metadata_path("session-prewarm-symlink", "video.m4s")
                    .expect("prewarm metadata path should be valid"),
                &metadata,
            )
            .expect("prewarm metadata should save");

        assert!(
            store
                .prewarmed_resource("session-prewarm-symlink", "video.m4s")
                .is_none()
        );
        assert!(
            store
                .open_prewarmed_resource("session-prewarm-symlink", "video.m4s")
                .is_none()
        );
        let usage = store
            .usage_snapshot()
            .expect("cache usage should ignore symlinked prewarm resource");
        assert_eq!(0, usage.used_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn load_sessions_skips_symlinked_hls_cache_session_directory_for_reads() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let store_root = temp.path().join(".tvos-net-player").join("hls");
        let outside_dir = temp.path().join("outside-session-read");
        let session = sample_session("session-read-link", "https://example.test/video.m4s");
        std::fs::create_dir_all(&store_root).expect("store root should be created");
        std::fs::create_dir(&outside_dir).expect("outside target should be created");
        write_pretty_json(
            &outside_dir.join("session.json"),
            &PersistedHlsSession::from(session.clone()),
        );
        symlink(&outside_dir, store_root.join(&session.id))
            .expect("session dir symlink should be made");

        let sessions = store.load_sessions().expect("cache scan should succeed");

        assert!(sessions.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn load_session_rejects_symlinked_hls_cache_session_manifest_for_reads() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-manifest-link", "https://example.test/video.m4s");
        let session_dir = store
            .session_dir(&session.id)
            .expect("session dir should be valid");
        std::fs::create_dir_all(&session_dir).expect("session dir should be created");
        let outside_manifest = temp.path().join("outside-session.json");
        write_pretty_json(
            &outside_manifest,
            &PersistedHlsSession::from(session.clone()),
        );
        symlink(&outside_manifest, session_dir.join("session.json"))
            .expect("session manifest symlink should be made");

        let sessions = store.load_sessions().expect("cache scan should succeed");

        assert!(sessions.is_empty());
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-manifest-link")
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_hls_cache_store_root_for_reads() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let internal_dir = temp.path().join(".tvos-net-player");
        let outside_dir = temp.path().join("outside-hls-root");
        std::fs::create_dir_all(&internal_dir).expect("internal parent should be created");
        std::fs::create_dir(&outside_dir).expect("outside target should be created");
        symlink(&outside_dir, internal_dir.join("hls")).expect("store root symlink should be made");

        let error = store
            .load_sessions()
            .expect_err("symlinked HLS store root should be rejected");

        assert_eq!(io::ErrorKind::PermissionDenied, error.kind());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_hls_cache_metadata_file_for_reads() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-metadata-link", "https://example.test/video.m4s");
        store
            .save_session(&session)
            .expect("session manifest should save");
        std::fs::write(
            store
                .resource_path(&session.id, "video.m4s")
                .expect("resource path should be valid"),
            fake_mp4(),
        )
        .expect("resource should be written");
        let outside_metadata = temp.path().join("outside-video.m4s.json");
        write_pretty_json(
            &outside_metadata,
            &cached_metadata_for_session(&session, "video.m4s"),
        );
        symlink(
            &outside_metadata,
            store
                .resource_metadata_path(&session.id, "video.m4s")
                .expect("metadata path should be valid"),
        )
        .expect("metadata symlink should be made");

        assert!(store.cached_resource(&session.id, "video.m4s").is_none());
        assert!(
            store
                .get_completed_library_item("bilibili.hls.session-metadata-link")
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn cached_resource_rejects_symlinked_hls_cache_session_directory_for_reads() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let store_root = temp.path().join(".tvos-net-player").join("hls");
        let outside_dir = temp.path().join("outside-session-resource");
        let session = sample_session("session-resource-link", "https://example.test/video.m4s");
        std::fs::create_dir_all(&store_root).expect("store root should be created");
        std::fs::create_dir(&outside_dir).expect("outside target should be created");
        std::fs::write(outside_dir.join("video.m4s"), fake_mp4())
            .expect("outside resource should be written");
        write_pretty_json(
            &outside_dir.join("video.m4s.json"),
            &cached_metadata_for_session(&session, "video.m4s"),
        );
        symlink(&outside_dir, store_root.join(&session.id))
            .expect("session dir symlink should be made");

        assert!(store.cached_resource(&session.id, "video.m4s").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_hls_cache_session_directory_for_writes_and_removal() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let store_root = temp.path().join(".tvos-net-player").join("hls");
        let outside_dir = temp.path().join("outside-session");
        std::fs::create_dir_all(&store_root).expect("store root should be created");
        std::fs::create_dir(&outside_dir).expect("outside target should be created");
        std::fs::write(outside_dir.join("keep.txt"), b"outside")
            .expect("outside sentinel should be written");
        symlink(&outside_dir, store_root.join("session-link"))
            .expect("session dir symlink should be made");
        let session = sample_session("session-link", "https://example.test/video.m4s");

        let save_error = store
            .save_session(&session)
            .expect_err("symlinked session dir should not be written");
        let remove_error = store
            .remove_session("session-link")
            .expect_err("symlinked session dir should not be removed");

        assert_eq!(io::ErrorKind::PermissionDenied, save_error.kind());
        assert_eq!(io::ErrorKind::PermissionDenied, remove_error.kind());
        assert_eq!(
            b"outside",
            std::fs::read(outside_dir.join("keep.txt"))
                .expect("outside sentinel should survive")
                .as_slice()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinked_hls_cache_temp_resource_path() {
        use std::os::unix::fs::symlink;

        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-temp-symlink", &upstream_url);
        store
            .save_session(&session)
            .expect("session manifest should save");
        let target_path = temp.path().join("outside-temp-target");
        std::fs::write(&target_path, b"outside").expect("target file should be written");
        let temp_path = store
            .resource_range_full_get_temp_path(&session.id, "video.m4s")
            .expect("compatibility temp path should be valid");
        symlink(&target_path, &temp_path).expect("temp path symlink should be made");
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("symlinked temp resource should be rejected");

        assert!(
            matches!(error, HlsCacheError::Io(error) if error.kind() == io::ErrorKind::PermissionDenied)
        );
        assert_eq!(
            b"outside",
            std::fs::read(&target_path)
                .expect("target file should survive")
                .as_slice()
        );
        assert!(
            store
                .cached_resource("session-temp-symlink", "video.m4s")
                .is_none()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinked_hls_cache_resource_path_before_commit() {
        use std::os::unix::fs::symlink;

        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-resource-symlink", &upstream_url);
        store
            .save_session(&session)
            .expect("session manifest should save");
        let target_path = temp.path().join("outside-resource-target");
        std::fs::write(&target_path, b"outside").expect("target file should be written");
        let resource_path = store
            .resource_path("session-resource-symlink", "video.m4s")
            .expect("resource path should be valid");
        symlink(&target_path, &resource_path).expect("resource path symlink should be made");
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("symlinked resource target should be rejected before rename");

        assert!(
            matches!(error, HlsCacheError::Io(error) if error.kind() == io::ErrorKind::PermissionDenied)
        );
        assert_eq!(
            b"outside",
            std::fs::read(&target_path)
                .expect("target file should survive")
                .as_slice()
        );
        assert!(
            std::fs::symlink_metadata(&resource_path)
                .expect("resource symlink should remain")
                .file_type()
                .is_symlink()
        );
        assert!(
            store
                .cached_resource("session-resource-symlink", "video.m4s")
                .is_none()
        );
    }

    #[tokio::test]
    async fn cancellation_after_committed_resource_removes_partial_session() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session_with_audio("session-cancel-after-video", &upstream_url);
        let client = reqwest::Client::new();
        let store_for_cancel = store.clone();
        let session_id = session.id.clone();

        let error = store
            .cache_session_resources_until(&client, &session, move || {
                store_for_cancel
                    .cached_resource(&session_id, "video.m4s")
                    .is_some()
            })
            .await
            .expect_err("cancellation after video commit should stop finalization");

        assert!(matches!(error, HlsCacheError::Cancelled));
        assert!(store.cached_resource(&session.id, "video.m4s").is_none());
        assert!(store.cached_resource(&session.id, "audio.m4s").is_none());
        assert!(
            store
                .get_completed_library_item(&format!("bilibili.hls.{}", session.id))
                .is_none()
        );
        assert!(
            !store
                .session_dir(&session.id)
                .expect("session dir should be valid")
                .exists()
        );
    }

    #[tokio::test]
    async fn completed_cache_removes_prewarmed_sidecars() {
        let (prewarm_url, _prewarm_task) = start_prewarm_mp4_upstream().await;
        let (full_url, _full_task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-prewarm-cleanup", &prewarm_url);
        session.variant.video.request.backup_urls = vec![full_url];
        let client = reqwest::Client::new();

        store
            .prewarm_session_first_frame_with_control(&client, &session, || {
                HlsCacheFillControl::Continue
            })
            .await
            .expect("session should prewarm");
        assert!(store.prewarmed_resource(&session.id, "video.m4s").is_some());

        store
            .cache_session_resources(&client, &session)
            .await
            .expect("session should finish full cache fill");

        assert!(store.prewarmed_resource(&session.id, "video.m4s").is_none());
        assert!(
            !store
                .resource_prewarm_path(&session.id, "video.m4s")
                .expect("prewarm resource path should be valid")
                .exists()
        );
        assert!(
            !store
                .resource_prewarm_metadata_path(&session.id, "video.m4s")
                .expect("prewarm metadata path should be valid")
                .exists()
        );
        let usage = store
            .usage_snapshot()
            .expect("usage snapshot should count only completed resource bytes");
        assert_eq!(1, usage.completed_session_count);
        assert_eq!(fake_mp4().len() as u64, usage.used_bytes);
    }

    #[tokio::test]
    async fn known_and_discovered_58_byte_resources_fit_exact_media_quota() {
        let (upstream_url, _task) = start_prewarm_mp4_upstream().await;
        let body = fake_mp4();
        assert_eq!(58, body.len(), "fixture should remain a 58-byte MP4");

        for (index, known_size) in [true, false].into_iter().enumerate() {
            let temp = TempDir::new().expect("temp dir should be created");
            let store = temp_store(&temp).with_range_budget(body.len() as u64);
            let mut session = sample_session(&format!("exact-tiny-quota-{index}"), &upstream_url);
            session.variant.video.request.size = known_size.then_some(body.len() as u64);
            store
                .save_session(&session)
                .expect("session should be saved");

            let downloaded = store
                .cache_resource(
                    &reqwest::Client::new(),
                    &session.id,
                    &session.variant.video,
                    &|| HlsCacheFillControl::Continue,
                    |_| {},
                )
                .await
                .expect("58-byte resource should fit an exact media quota");

            assert_eq!(body.len() as u64, downloaded);
            assert_eq!(
                body.len() as u64,
                store
                    .usage_snapshot()
                    .expect("usage should be readable")
                    .used_bytes
            );
            assert_eq!((0, 0), store.range_activity_counts());
        }
    }

    #[tokio::test]
    async fn known_and_discovered_58_byte_resources_reject_insufficient_media_quota() {
        let (upstream_url, _task) = start_prewarm_mp4_upstream().await;
        let body = fake_mp4();
        assert_eq!(58, body.len(), "fixture should remain a 58-byte MP4");

        for (index, known_size) in [true, false].into_iter().enumerate() {
            let temp = TempDir::new().expect("temp dir should be created");
            let store = temp_store(&temp).with_range_budget((body.len() - 1) as u64);
            let mut session = sample_session(&format!("short-tiny-quota-{index}"), &upstream_url);
            session.variant.video.request.size = known_size.then_some(body.len() as u64);
            store
                .save_session(&session)
                .expect("session should be saved");

            let error = store
                .cache_resource(
                    &reqwest::Client::new(),
                    &session.id,
                    &session.variant.video,
                    &|| HlsCacheFillControl::Continue,
                    |_| {},
                )
                .await
                .expect_err("58-byte resource must not exceed a 57-byte quota");

            assert!(matches!(
                error,
                HlsCacheError::Range(HlsRangeError::QuotaExceeded)
            ));
            assert_eq!((0, 0), store.range_activity_counts());
            assert_eq!(
                0,
                store.range_managed_size(&session.id, "video.m4s").unwrap()
            );
        }
    }

    #[tokio::test]
    async fn zero_media_quota_keeps_caching_without_automatic_quota_limit() {
        let (upstream_url, _task) = start_prewarm_mp4_upstream().await;
        let body = fake_mp4();
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(0);
        let mut session = sample_session("zero-quota-cache", &upstream_url);
        session.variant.video.request.size = Some(body.len() as u64);
        store
            .save_session(&session)
            .expect("session should be saved");

        let downloaded = store
            .cache_resource(
                &reqwest::Client::new(),
                &session.id,
                &session.variant.video,
                &|| HlsCacheFillControl::Continue,
                |_| {},
            )
            .await
            .expect("zero should disable the quota limit, not caching");

        assert_eq!(body.len() as u64, downloaded);
        assert_eq!(
            body.len() as u64,
            store
                .usage_snapshot()
                .expect("usage should be readable")
                .used_bytes
        );
    }

    #[test]
    fn media_reservation_is_released_after_owner_finishes() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(fake_mp4().len() as u64);
        let session = sample_session("reservation-release", "https://cdn.example/video.m4s");
        let key = store
            .range_resource_key(&session.id, &session.variant.video)
            .expect("range key should be valid");
        let lease = store
            .range_cache
            .reserve(key, fake_mp4().len() as u64, 0, 0)
            .expect("reservation within quota should succeed");
        assert_eq!((0, 1), store.range_activity_counts());

        drop(lease);

        assert_eq!((0, 0), store.range_activity_counts());
    }

    #[tokio::test]
    async fn full_get_scratch_yields_to_concurrently_checkpointed_range_extent() {
        let body = fake_mp4();
        let gate = Arc::new(FullGetGate {
            started: Notify::new(),
            release: Notify::new(),
            body: body.clone(),
        });
        let app = Router::new()
            .route("/video.m4s", get(upstream_ignores_range_and_holds_full_get))
            .with_state(Arc::clone(&gate));
        let (url, _server) = start_hls_cache_upstream(app).await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp).with_range_budget(4 * 1024 * 1024);
        let session = sample_session("full-get-concurrent-range", &url);
        let resource = session.variant.video.clone();
        let fill_store = store.clone();
        let fill_session_id = session.id.clone();
        let fill_resource = resource.clone();
        let fill = tokio::spawn(async move {
            fill_store
                .fill_missing_ranges_with_progress(
                    &reqwest::Client::new(),
                    &fill_session_id,
                    &fill_resource,
                    HlsRangePriority::Background,
                    &|| HlsCacheFillControl::Continue,
                    |_| {},
                )
                .await
        });

        tokio::time::timeout(Duration::from_secs(5), gate.started.notified())
            .await
            .expect("full GET should begin after range is ignored");
        let loaded = store
            .load_range_manifest(&session.id, &resource)
            .expect("range checkpoint should load")
            .expect("range checkpoint should exist");
        let loaded = store
            .publish_range_manifest(&session.id, &resource, loaded, |manifest| {
                manifest.total_length = Some(body.len() as u64);
            })
            .await
            .expect("concurrent total should checkpoint");
        store
            .commit_range_extent(
                &session.id,
                &resource,
                &loaded.manifest,
                0..1,
                &body[..1],
                &url,
                None,
                None,
            )
            .await
            .expect("foreground range extent should checkpoint during full GET");
        gate.release.notify_one();

        let result = tokio::time::timeout(Duration::from_secs(5), fill)
            .await
            .expect("full GET should finish after release")
            .expect("fill task should join");
        assert!(matches!(result, Err(HlsRangeError::RangeUnsupported)));
        let checkpoint = store
            .load_range_manifest(&session.id, &resource)
            .expect("trusted checkpoint should remain readable")
            .expect("trusted checkpoint should remain present");
        assert!(range_is_covered(&checkpoint.manifest.extents, 0..1));
        assert_eq!(1, checkpoint.manifest.durable_bytes);
        assert_eq!(
            Some(body[..1].to_vec()),
            store
                .read_durable_range(&session.id, &resource, 0..1)
                .await
                .expect("trusted partial bytes should remain verifiable")
        );
        assert!(
            !store
                .resource_range_full_get_temp_path(&session.id, &resource.id)
                .expect("full GET scratch path should be valid")
                .exists()
        );
    }

    #[test]
    fn remove_session_managed_resources_preserves_manifest_and_removes_sidecars() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-resource-cleanup", "https://example.test/video.m4s");
        store
            .save_session(&session)
            .expect("session manifest should save");
        std::fs::write(
            store
                .resource_path(&session.id, "video.m4s")
                .expect("resource path should be valid"),
            fake_mp4(),
        )
        .expect("resource should be written");
        write_pretty_json(
            &store
                .resource_metadata_path(&session.id, "video.m4s")
                .expect("resource metadata path should be valid"),
            &cached_metadata_for_session(&session, "video.m4s"),
        );
        let prewarm_prefix_length = 32_u64;
        std::fs::write(
            store
                .resource_prewarm_path(&session.id, "video.m4s")
                .expect("prewarm resource path should be valid"),
            &fake_mp4()[..prewarm_prefix_length as usize],
        )
        .expect("prewarm resource should be written");
        write_pretty_json(
            &store
                .resource_prewarm_metadata_path(&session.id, "video.m4s")
                .expect("prewarm metadata path should be valid"),
            &PersistedHlsPrewarmedResource {
                schema_version: HLS_CACHE_SCHEMA_VERSION,
                id: "video.m4s".to_owned(),
                content_type: session.variant.video.content_type().to_owned(),
                prefix_length: prewarm_prefix_length,
                target_prefix_length: Some(prewarm_prefix_length),
                target_window_seconds: Some(HLS_FIRST_WINDOW_PREFETCH_SECONDS),
                total_length: fake_mp4().len() as u64,
                initialization_length: 28,
                cache_key: PersistedBilibiliMediaCacheKey::from(
                    session.variant.video.request.cache_key.clone(),
                ),
            },
        );
        std::fs::write(
            store
                .resource_path(&session.id, "stale.m4s")
                .expect("stale resource path should be valid"),
            fake_mp4(),
        )
        .expect("stale resource should be written");
        write_pretty_json(
            &store
                .resource_metadata_path(&session.id, "stale.m4s")
                .expect("stale metadata path should be valid"),
            &cached_metadata_for_session(&session, "stale.m4s"),
        );

        assert!(store.playback_session("session-resource-cleanup").is_some());
        assert!(store.cached_resource(&session.id, "video.m4s").is_some());
        assert!(store.prewarmed_resource(&session.id, "video.m4s").is_some());
        assert!(
            store
                .resource_path(&session.id, "stale.m4s")
                .expect("stale resource path should be valid")
                .exists()
        );

        store
            .remove_session_managed_resources("session-resource-cleanup")
            .expect("managed resources should be removed");

        assert!(store.playback_session("session-resource-cleanup").is_some());
        assert!(store.cached_resource(&session.id, "video.m4s").is_none());
        assert!(store.prewarmed_resource(&session.id, "video.m4s").is_none());
        assert!(
            !store
                .resource_path(&session.id, "video.m4s")
                .expect("resource path should be valid")
                .exists()
        );
        assert!(
            !store
                .resource_metadata_path(&session.id, "video.m4s")
                .expect("resource metadata path should be valid")
                .exists()
        );
        assert!(
            !store
                .resource_path(&session.id, "stale.m4s")
                .expect("stale resource path should be valid")
                .exists()
        );
        assert!(
            !store
                .resource_metadata_path(&session.id, "stale.m4s")
                .expect("stale metadata path should be valid")
                .exists()
        );
        assert!(
            !store
                .resource_prewarm_path(&session.id, "video.m4s")
                .expect("prewarm resource path should be valid")
                .exists()
        );
        assert!(
            !store
                .resource_prewarm_metadata_path(&session.id, "video.m4s")
                .expect("prewarm metadata path should be valid")
                .exists()
        );
        let usage = store
            .usage_snapshot()
            .expect("usage snapshot should scan preserved manifest");
        assert_eq!(0, usage.completed_session_count);
        assert_eq!(0, usage.used_bytes);
    }

    #[test]
    fn eviction_resource_cleanup_removes_active_transcode_outputs() {
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_transcoding_ready_session(
            "session-transcode-eviction-cleanup",
            "https://example.test/video.m4s",
        );
        let completed_session = transcoded_completed_session(&session, fake_mp4().len() as u64);
        store
            .save_session(&session)
            .expect("session manifest should save");

        for resource in std::iter::once(&session.variant.video).chain(session.variant.audio.iter())
        {
            std::fs::write(
                store
                    .resource_path(&session.id, &resource.id)
                    .expect("source resource path should be valid"),
                fake_mp4(),
            )
            .expect("source resource should be written");
            write_pretty_json(
                &store
                    .resource_metadata_path(&session.id, &resource.id)
                    .expect("source metadata path should be valid"),
                &cached_metadata_for_session(&session, &resource.id),
            );
        }
        std::fs::write(
            store
                .resource_path(&session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated resource path should be valid"),
            fake_mp4(),
        )
        .expect("generated resource should be written");
        write_pretty_json(
            &store
                .resource_metadata_path(&session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated metadata path should be valid"),
            &cached_metadata_for_session(&completed_session, HLS_TRANSCODED_RESOURCE_ID),
        );
        std::fs::write(
            store
                .resource_path(
                    &session.id,
                    &transcoding_temp_file_name(HLS_TRANSCODED_RESOURCE_ID),
                )
                .expect("transcode temp path should be valid"),
            b"active ffmpeg output",
        )
        .expect("transcode temp should be written");
        let _guard = HlsTranscodingCommitGuard::create_if_needed(&store, &session)
            .expect("active transcoding marker should be created");

        let partial = store
            .partial_cache_entries()
            .expect("partial entries should scan")
            .into_iter()
            .find(|entry| entry.session_id == session.id)
            .expect("active transcode bytes should be counted before eviction cleanup");
        assert!(partial.size_bytes > (2 * fake_mp4().len()) as u64);

        store
            .remove_session_managed_resources_for_eviction(&session.id)
            .expect("eviction cleanup should remove managed resources");

        assert!(store.playback_session(&session.id).is_some());
        for resource in std::iter::once(&session.variant.video).chain(session.variant.audio.iter())
        {
            assert!(store.cached_resource(&session.id, &resource.id).is_none());
            assert!(
                !store
                    .resource_path(&session.id, &resource.id)
                    .expect("source resource path should be valid")
                    .exists()
            );
            assert!(
                !store
                    .resource_metadata_path(&session.id, &resource.id)
                    .expect("source metadata path should be valid")
                    .exists()
            );
        }
        assert!(
            store
                .cached_resource(&session.id, HLS_TRANSCODED_RESOURCE_ID)
                .is_none()
        );
        assert!(
            !store
                .resource_path(&session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated resource path should be valid")
                .exists()
        );
        assert!(
            !store
                .resource_metadata_path(&session.id, HLS_TRANSCODED_RESOURCE_ID)
                .expect("generated metadata path should be valid")
                .exists()
        );
        assert!(
            !store
                .resource_path(
                    &session.id,
                    &transcoding_temp_file_name(HLS_TRANSCODED_RESOURCE_ID),
                )
                .expect("transcode temp path should be valid")
                .exists()
        );
        assert!(
            !store
                .transcoding_commit_marker_path(&session.id)
                .expect("transcoding marker path should be valid")
                .exists()
        );
        let usage = store
            .usage_snapshot()
            .expect("usage snapshot should scan preserved manifest");
        assert_eq!(0, usage.used_bytes);
    }

    #[tokio::test]
    async fn prewarm_prefix_download_observes_preemption_while_body_is_stalled() {
        let (upstream_url, _task) = start_stalled_prewarm_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-stalled-prewarm", &upstream_url);
        let client = reqwest::Client::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let control_calls = Arc::clone(&calls);

        let result = tokio::time::timeout(Duration::from_secs(2), async {
            store
                .prewarm_session_first_frame_with_control(&client, &session, move || {
                    if control_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 4 {
                        HlsCacheFillControl::Preempt
                    } else {
                        HlsCacheFillControl::Continue
                    }
                })
                .await
        })
        .await
        .expect("prewarm should observe preemption without waiting for read timeout");

        assert!(matches!(result, Err(HlsCacheError::Preempted)));
        assert!(store.prewarmed_resource(&session.id, "video.m4s").is_none());
    }

    #[tokio::test]
    async fn prewarm_prefix_download_observes_preemption_while_headers_are_stalled() {
        let (upstream_url, _task) = start_headers_stalled_prewarm_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-stalled-prewarm-headers", &upstream_url);
        let client = reqwest::Client::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let control_calls = Arc::clone(&calls);

        let result = tokio::time::timeout(Duration::from_secs(2), async {
            store
                .prewarm_session_first_frame_with_control(&client, &session, move || {
                    if control_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 4 {
                        HlsCacheFillControl::Preempt
                    } else {
                        HlsCacheFillControl::Continue
                    }
                })
                .await
        })
        .await
        .expect("prewarm should observe preemption before upstream headers arrive");

        assert!(matches!(result, Err(HlsCacheError::Preempted)));
        assert!(store.prewarmed_resource(&session.id, "video.m4s").is_none());
    }

    #[tokio::test]
    async fn full_resource_download_observes_preemption_while_headers_are_stalled() {
        let (upstream_url, _task) = start_headers_stalled_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-stalled-resource-headers", &upstream_url);
        let client = reqwest::Client::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let control_calls = Arc::clone(&calls);

        let result = tokio::time::timeout(Duration::from_secs(2), async {
            store
                .cache_session_resources_with_control(
                    &client,
                    &session,
                    move || {
                        if control_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 4 {
                            HlsCacheFillControl::Preempt
                        } else {
                            HlsCacheFillControl::Continue
                        }
                    },
                    |_| {},
                )
                .await
        })
        .await
        .expect("resource fill should observe preemption before upstream headers arrive");

        assert!(matches!(result, Err(HlsCacheError::Preempted)));
        assert!(store.cached_resource(&session.id, "video.m4s").is_none());
    }

    #[tokio::test]
    async fn preemption_after_prewarm_rename_commits_metadata_before_stopping() {
        let (upstream_url, _task) = start_prewarm_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session("session-prewarm-commit-preempt", &upstream_url);
        let client = reqwest::Client::new();
        let prewarm_path = store
            .resource_prewarm_path(&session.id, "video.m4s")
            .expect("prewarm resource path should be valid");

        let error = store
            .prewarm_session_first_frame_with_control(&client, &session, || {
                if prewarm_path.exists() {
                    HlsCacheFillControl::Preempt
                } else {
                    HlsCacheFillControl::Continue
                }
            })
            .await
            .expect_err("preempted prewarm should stop after metadata commit");

        assert!(matches!(error, HlsCacheError::Preempted));
        let prewarmed = store
            .prewarmed_resource(&session.id, "video.m4s")
            .expect("prewarmed resource should be registered");
        let usage = store.usage_snapshot().expect("prewarmed usage should scan");
        assert_eq!(prewarmed.prefix_length, usage.used_bytes);
    }

    #[tokio::test]
    async fn preemption_after_committed_resource_preserves_partial_session() {
        let (upstream_url, _task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let session = sample_session_with_audio("session-preempt-after-video", &upstream_url);
        let client = reqwest::Client::new();
        let store_for_preempt = store.clone();
        let session_id = session.id.clone();

        let error = store
            .cache_session_resources_with_control(
                &client,
                &session,
                move || {
                    if store_for_preempt
                        .cached_resource(&session_id, "video.m4s")
                        .is_some()
                    {
                        HlsCacheFillControl::Preempt
                    } else {
                        HlsCacheFillControl::Continue
                    }
                },
                |_| {},
            )
            .await
            .expect_err("preemption after video commit should stop finalization");

        assert!(matches!(error, HlsCacheError::Preempted));
        assert!(store.cached_resource(&session.id, "video.m4s").is_some());
        assert!(store.cached_resource(&session.id, "audio.m4s").is_none());
        assert!(
            store
                .get_completed_library_item(&format!("bilibili.hls.{}", session.id))
                .is_none()
        );
        assert!(
            store
                .session_dir(&session.id)
                .expect("session dir should be valid")
                .exists()
        );
        assert!(
            !store
                .resource_path(&session.id, "audio.m4s")
                .expect("audio resource path should be valid")
                .with_extension("tmp")
                .exists()
        );
    }

    #[tokio::test]
    async fn preemption_after_resource_rename_commits_metadata_before_stopping() {
        let (prewarm_url, _prewarm_task) = start_prewarm_mp4_upstream().await;
        let (full_url, _full_task) = start_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-resource-commit-preempt", &prewarm_url);
        let client = reqwest::Client::new();
        let prewarm_path = store
            .resource_prewarm_path(&session.id, "video.m4s")
            .expect("prewarm resource path should be valid");
        let prewarm_metadata_path = store
            .resource_prewarm_metadata_path(&session.id, "video.m4s")
            .expect("prewarm metadata path should be valid");
        let resource_path = store
            .resource_path(&session.id, "video.m4s")
            .expect("resource path should be valid");

        store
            .prewarm_session_first_frame_with_control(&client, &session, || {
                HlsCacheFillControl::Continue
            })
            .await
            .expect("session should prewarm");
        assert!(prewarm_path.exists());
        assert!(prewarm_metadata_path.exists());

        session.variant.video.request.url = full_url;
        let error = store
            .cache_session_resources_with_control(
                &client,
                &session,
                || {
                    if resource_path.exists() {
                        HlsCacheFillControl::Preempt
                    } else {
                        HlsCacheFillControl::Continue
                    }
                },
                |_| {},
            )
            .await
            .expect_err("preempted fill should stop after metadata commit");

        assert!(matches!(error, HlsCacheError::Preempted));
        assert!(store.cached_resource(&session.id, "video.m4s").is_some());
        assert!(store.prewarmed_resource(&session.id, "video.m4s").is_none());
        assert!(!prewarm_path.exists());
        assert!(!prewarm_metadata_path.exists());
        let usage = store
            .usage_snapshot()
            .expect("partial cache usage should scan");
        assert_eq!(fake_mp4().len() as u64, usage.used_bytes);
    }

    async fn start_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(Router::new().route("/video.m4s", get(upstream_mp4))).await
    }

    async fn start_prewarm_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(Router::new().route("/video.m4s", get(upstream_prewarm_mp4))).await
    }

    async fn start_large_prewarm_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(Router::new().route("/video.m4s", get(upstream_large_prewarm_mp4)))
            .await
    }

    async fn start_position_prewarm_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(
            Router::new().route("/video.m4s", get(upstream_position_prewarm_mp4)),
        )
        .await
    }

    async fn start_headers_stalled_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(
            Router::new().route("/video.m4s", get(upstream_headers_stalled_mp4)),
        )
        .await
    }

    async fn start_headers_stalled_prewarm_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(
            Router::new().route("/video.m4s", get(upstream_headers_stalled_prewarm_mp4)),
        )
        .await
    }

    async fn start_stalled_prewarm_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(
            Router::new().route("/video.m4s", get(upstream_stalled_prewarm_mp4)),
        )
        .await
    }

    async fn start_short_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(Router::new().route("/video.m4s", get(upstream_short_mp4))).await
    }

    async fn start_overlong_chunked_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(
            Router::new().route("/video.m4s", get(upstream_overlong_chunked_mp4)),
        )
        .await
    }

    async fn start_partial_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(Router::new().route("/video.m4s", get(upstream_partial_mp4))).await
    }

    async fn start_invalid_mp4_upstream() -> (String, tokio::task::JoinHandle<()>) {
        start_hls_cache_upstream(Router::new().route("/video.m4s", get(upstream_invalid_mp4))).await
    }

    async fn start_hls_cache_upstream(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener should bind");
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("upstream should run");
        });

        (format!("http://{addr}/video.m4s"), task)
    }

    async fn upstream_mp4(headers: HeaderMap) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap();
        }

        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "video/mp4")
            .body(Body::from(fake_mp4()))
            .unwrap()
    }

    async fn upstream_server_failure() -> Response<Body> {
        Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::empty())
            .unwrap()
    }

    async fn upstream_bad_gateway() -> Response<Body> {
        Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::empty())
            .unwrap()
    }

    async fn upstream_range_probe(_headers: HeaderMap) -> Response<Body> {
        let body = fake_mp4();
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_LENGTH, "1")
            .header("content-range", format!("bytes 0-0/{}", body.len()))
            .body(Body::from(body[..1].to_vec()))
            .unwrap()
    }

    async fn upstream_prewarm_mp4(headers: HeaderMap) -> Response<Body> {
        upstream_prewarm_mp4_bytes(headers, fake_mp4())
    }

    async fn upstream_large_prewarm_mp4(headers: HeaderMap) -> Response<Body> {
        upstream_prewarm_mp4_bytes(headers, large_prefetch_fake_mp4())
    }

    async fn upstream_position_prewarm_mp4(headers: HeaderMap) -> Response<Body> {
        upstream_prewarm_mp4_bytes(headers, position_prefetch_fake_mp4())
    }

    fn upstream_prewarm_mp4_bytes(headers: HeaderMap, body: Vec<u8>) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap();
        }

        let requested_end = requested_prewarm_range_end(&headers).unwrap_or(body.len() as u64 - 1);
        let prefix_length = (requested_end.saturating_add(1))
            .min(body.len() as u64)
            .max(1);
        let prefix = body[..usize::try_from(prefix_length).unwrap()].to_vec();
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_LENGTH, prefix_length.to_string())
            .header(
                "content-range",
                format!("bytes 0-{}/{}", prefix_length - 1, body.len()),
            )
            .body(Body::from(prefix))
            .unwrap()
    }

    fn requested_prewarm_range_end(headers: &HeaderMap) -> Option<u64> {
        let value = headers.get(reqwest::header::RANGE)?.to_str().ok()?;
        value.strip_prefix("bytes=0-")?.parse().ok()
    }

    async fn upstream_headers_stalled_mp4(headers: HeaderMap) -> Response<Body> {
        tokio::time::sleep(Duration::from_secs(30)).await;
        upstream_mp4(headers).await
    }

    async fn upstream_headers_stalled_prewarm_mp4(headers: HeaderMap) -> Response<Body> {
        tokio::time::sleep(Duration::from_secs(30)).await;
        upstream_prewarm_mp4(headers).await
    }

    async fn upstream_stalled_prewarm_mp4(headers: HeaderMap) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap();
        }

        let body = fake_mp4();
        let body_len = body.len();
        let chunks = futures_util::stream::once(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok::<_, std::convert::Infallible>(body)
        });
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_LENGTH, body_len.to_string())
            .header(
                "content-range",
                format!("bytes 0-{}/{}", body_len - 1, body_len),
            )
            .body(Body::from_stream(chunks))
            .unwrap()
    }

    async fn upstream_short_mp4(headers: HeaderMap) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap();
        }

        let body = fake_mp4();
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_LENGTH, (body.len() - 4).to_string())
            .body(Body::from(body[..body.len() - 4].to_vec()))
            .unwrap()
    }

    async fn upstream_overlong_chunked_mp4(headers: HeaderMap) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap();
        }

        let body = fake_mp4();
        let chunks = futures_util::stream::iter([
            Ok::<_, std::convert::Infallible>(body),
            Ok::<_, std::convert::Infallible>(b"extra".to_vec()),
        ]);
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "video/mp4")
            .body(Body::from_stream(chunks))
            .unwrap()
    }

    async fn upstream_partial_mp4(headers: HeaderMap) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap();
        }

        let body = fake_mp4();
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_LENGTH, body.len().to_string())
            .body(Body::from(body))
            .unwrap()
    }

    async fn upstream_invalid_mp4(headers: HeaderMap) -> Response<Body> {
        if headers.get("referer") != Some(&HeaderValue::from_static("https://www.bilibili.com")) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap();
        }

        let body = invalid_mp4();
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_LENGTH, body.len().to_string())
            .body(Body::from(body))
            .unwrap()
    }

    async fn upstream_mp4_reject_sensitive_headers(headers: HeaderMap) -> Response<Body> {
        if headers.contains_key("authorization") || headers.contains_key("cookie") {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::empty())
                .unwrap();
        }

        upstream_mp4(headers).await
    }

    fn sample_session(id: &str, url: &str) -> HlsPlaybackSession {
        HlsPlaybackSession {
            id: id.to_owned(),
            title: "Episode".to_owned(),
            accepted_identity: None,
            variant: HlsVariant {
                id: "h264".to_owned(),
                bandwidth: 1_000_000,
                codecs: vec!["avc1.640028".to_owned()],
                width: Some(1920),
                height: Some(1080),
                duration_seconds: 60,
                video: HlsMediaResource {
                    id: "video.m4s".to_owned(),
                    request: BilibiliMediaRequest {
                        kind: BilibiliMediaRequestKind::Video,
                        stream_id: None,
                        url: url.to_owned(),
                        backup_urls: Vec::new(),
                        headers: vec![BilibiliHttpHeader {
                            name: "referer".to_owned(),
                            value: "https://www.bilibili.com".to_owned(),
                        }],
                        mime_type: Some("video/mp4".to_owned()),
                        codecs: Some("avc1.640028".to_owned()),
                        bandwidth: Some(1_000_000),
                        width: Some(1920),
                        height: Some(1080),
                        frame_rate: Some("60".to_owned()),
                        size: None,
                        duration_seconds: Some(60),
                        cache_key: BilibiliMediaCacheKey {
                            content_id: "cid-1".to_owned(),
                            media_kind: BilibiliMediaRequestKind::Video,
                            stream_id: None,
                            codecs: Some("avc1.640028".to_owned()),
                            source_hash: "source-hash".to_owned(),
                        },
                    },
                },
                audio: None,
            },
            alternate_variants: Vec::new(),
            advertise_alternate_variants: true,
            abr: Default::default(),
            variants: Vec::new(),
            transcoding: Default::default(),
            effective_policy: PlaybackPolicy::default(),
        }
    }

    fn sample_refresh_identity() -> crate::bilibili_playback::BilibiliContentIdentity {
        crate::bilibili_playback::BilibiliContentIdentity {
            kind: crate::bilibili_playback::BilibiliContentKind::VideoPage,
            aid: Some(1),
            bvid: Some("BV1xx411c7mD".to_owned()),
            cid: Some(2),
            epid: None,
        }
    }

    fn sample_session_with_audio(id: &str, url: &str) -> HlsPlaybackSession {
        let mut session = sample_session(id, url);
        let mut audio = session.variant.video.clone();
        audio.id = "audio.m4s".to_owned();
        audio.request.kind = BilibiliMediaRequestKind::Audio;
        audio.request.codecs = Some("mp4a.40.2".to_owned());
        audio.request.cache_key.media_kind = BilibiliMediaRequestKind::Audio;
        audio.request.cache_key.codecs = Some("mp4a.40.2".to_owned());
        session.variant.audio = Some(audio);
        session
    }

    fn sample_transcoding_ready_session(id: &str, url: &str) -> HlsPlaybackSession {
        let mut session = sample_session_with_audio(id, url);
        session.variant.codecs = vec!["hev1.1.6.L120.90".to_owned()];
        session.variant.video.request.codecs = Some("hev1.1.6.L120.90".to_owned());
        session.variant.video.request.cache_key.codecs = Some("hev1.1.6.L120.90".to_owned());
        session.variant.video.request.cache_key.source_hash = "hevc-source-hash".to_owned();
        session.transcoding = HlsTranscodingPlan::with_state(
            HlsTranscodingPlanState::Ready,
            session.variant.id.clone(),
            "HEVC source should be converted before completed offline cache exposure.",
        );
        session
    }

    fn attach_sample_alternate_variant(session: &mut HlsPlaybackSession, video_url: &str) {
        let mut video = session.variant.video.clone();
        video.id = "v1-video.m4s".to_owned();
        video.request.url = video_url.to_owned();
        video.request.bandwidth = Some(600_000);
        video.request.width = Some(1280);
        video.request.height = Some(720);
        video.request.cache_key.source_hash = "h264-720p-video-source".to_owned();

        let mut audio = session.variant.video.clone();
        audio.id = "v1-audio.m4s".to_owned();
        audio.request.kind = BilibiliMediaRequestKind::Audio;
        audio.request.url = "https://example.test/720p-audio.m4s".to_owned();
        audio.request.codecs = Some("mp4a.40.2".to_owned());
        audio.request.width = None;
        audio.request.height = None;
        audio.request.cache_key.media_kind = BilibiliMediaRequestKind::Audio;
        audio.request.cache_key.codecs = Some("mp4a.40.2".to_owned());
        audio.request.cache_key.source_hash = "h264-720p-audio-source".to_owned();

        session.alternate_variants.push(HlsVariant {
            id: "h264-720p".to_owned(),
            bandwidth: 600_000,
            codecs: vec!["avc1.640028".to_owned()],
            width: Some(1280),
            height: Some(720),
            duration_seconds: 60,
            video,
            audio: Some(audio),
        });
    }

    fn attach_sample_abr_metadata(session: &mut HlsPlaybackSession) {
        let h264_video = sample_resource_metadata(&session.variant.video.request);
        let hevc_video = HlsMediaResourceMetadata {
            kind: BilibiliMediaRequestKind::Video,
            stream_id: Some(80),
            mime_type: Some("video/mp4".to_owned()),
            codecs: Some("hev1.1.6.L120.90".to_owned()),
            bandwidth: Some(1_800_000),
            width: Some(1920),
            height: Some(1080),
            frame_rate: Some("60".to_owned()),
            size: Some(2048),
            duration_seconds: Some(60),
            cache_key: BilibiliMediaCacheKey {
                content_id: "cid-1".to_owned(),
                media_kind: BilibiliMediaRequestKind::Video,
                stream_id: Some(80),
                codecs: Some("hev1.1.6.L120.90".to_owned()),
                source_hash: "hevc-source-hash".to_owned(),
            },
        };
        session.abr = HlsAbrMetadata {
            groups: vec![HlsAbrGroup {
                id: "dash-video".to_owned(),
                kind: HlsAbrGroupKind::DashVideo,
                variant_ids: vec!["h264".to_owned(), "hevc".to_owned()],
                level_count: 2,
                min_bandwidth: Some(1_000_000),
                max_bandwidth: Some(1_800_000),
            }],
        };
        session.variants = vec![
            HlsVariantMetadata {
                id: "h264".to_owned(),
                kind: BilibiliPlaybackVariantKind::Dash,
                content_id: "cid-1".to_owned(),
                bandwidth: Some(1_000_000),
                codecs: vec!["avc1.640028".to_owned()],
                mime_types: vec!["video/mp4".to_owned()],
                width: Some(1920),
                height: Some(1080),
                frame_rate: Some("60".to_owned()),
                duration_seconds: Some(60),
                abr: Some(HlsAbrLevel {
                    group_id: "dash-video".to_owned(),
                    level_index: 0,
                    level_count: 2,
                    switchable: true,
                }),
                media: vec![h264_video],
            },
            HlsVariantMetadata {
                id: "hevc".to_owned(),
                kind: BilibiliPlaybackVariantKind::Dash,
                content_id: "cid-1".to_owned(),
                bandwidth: Some(1_800_000),
                codecs: vec!["hev1.1.6.L120.90".to_owned()],
                mime_types: vec!["video/mp4".to_owned()],
                width: Some(1920),
                height: Some(1080),
                frame_rate: Some("60".to_owned()),
                duration_seconds: Some(60),
                abr: Some(HlsAbrLevel {
                    group_id: "dash-video".to_owned(),
                    level_index: 1,
                    level_count: 2,
                    switchable: true,
                }),
                media: vec![hevc_video],
            },
        ];
    }

    fn sample_resource_metadata(request: &BilibiliMediaRequest) -> HlsMediaResourceMetadata {
        HlsMediaResourceMetadata {
            kind: request.kind,
            stream_id: request.stream_id,
            mime_type: request.mime_type.clone(),
            codecs: request.codecs.clone(),
            bandwidth: request.bandwidth,
            width: request.width,
            height: request.height,
            frame_rate: request.frame_rate.clone(),
            size: request.size,
            duration_seconds: request.duration_seconds,
            cache_key: request.cache_key.clone(),
        }
    }

    fn fake_mp4() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(mp4_box(*b"ftyp", b"isom"));
        bytes.extend(mp4_box(*b"moov", b"metadata"));
        bytes.extend(mp4_box(*b"moof", b"frag"));
        bytes.extend(mp4_box(*b"mdat", b"media-data"));
        bytes
    }

    fn multi_fragment_fake_mp4() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(mp4_box(*b"ftyp", b"isom"));
        bytes.extend(mp4_box(
            *b"moov",
            &mp4_box(*b"trak", &mp4_box(*b"mdia", &mdhd_box(1_000))),
        ));
        bytes.extend(moof_box(1_000));
        bytes.extend(mp4_box(*b"mdat", b"first-media"));
        bytes.extend(moof_box(2_000));
        bytes.extend(mp4_box(*b"mdat", b"second-media"));
        bytes
    }

    fn multi_fragment_fake_mp4_segments() -> Vec<PersistedHlsMediaSegment> {
        let initialization_length = multi_fragment_fake_mp4_initialization_length();
        let first_length = (moof_box(1_000).len() + mp4_box(*b"mdat", b"first-media").len()) as u64;
        let second_length =
            (moof_box(2_000).len() + mp4_box(*b"mdat", b"second-media").len()) as u64;
        vec![
            PersistedHlsMediaSegment {
                byte_range_offset: initialization_length,
                byte_range_length: first_length,
                duration_millis: 1_000,
            },
            PersistedHlsMediaSegment {
                byte_range_offset: initialization_length + first_length,
                byte_range_length: second_length,
                duration_millis: 2_000,
            },
        ]
    }

    fn multi_fragment_fake_mp4_initialization_length() -> u64 {
        (mp4_box(*b"ftyp", b"isom").len()
            + mp4_box(
                *b"moov",
                &mp4_box(*b"trak", &mp4_box(*b"mdia", &mdhd_box(1_000))),
            )
            .len()) as u64
    }

    fn mdhd_box(timescale: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend([0, 0, 0, 0]);
        payload.extend(0_u32.to_be_bytes());
        payload.extend(0_u32.to_be_bytes());
        payload.extend(timescale.to_be_bytes());
        payload.extend(0_u32.to_be_bytes());
        payload.extend(0_u16.to_be_bytes());
        payload.extend(0_u16.to_be_bytes());
        mp4_box(*b"mdhd", &payload)
    }

    fn moof_box(duration: u32) -> Vec<u8> {
        mp4_box(
            *b"moof",
            &mp4_box(*b"traf", &[tfhd_box(1), trun_box(duration)].concat()),
        )
    }

    fn tfhd_box(track_id: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend([0, 0, 0, 0]);
        payload.extend(track_id.to_be_bytes());
        mp4_box(*b"tfhd", &payload)
    }

    fn trun_box(duration: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend([0, 0, 1, 0]);
        payload.extend(1_u32.to_be_bytes());
        payload.extend(duration.to_be_bytes());
        mp4_box(*b"trun", &payload)
    }

    fn large_prefetch_fake_mp4() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(mp4_box(*b"ftyp", b"isom"));
        bytes.extend(mp4_box(*b"moov", b"metadata"));
        bytes.extend(mp4_box(*b"moof", b"frag"));
        bytes.extend(mp4_box(
            *b"mdat",
            &vec![0x55; usize::try_from(HLS_FIRST_WINDOW_PREFETCH_MAX_BYTES).unwrap()],
        ));
        bytes
    }

    fn position_prefetch_fake_mp4() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(mp4_box(*b"ftyp", b"isom"));
        bytes.extend(mp4_box(*b"moov", b"metadata"));
        bytes.extend(mp4_box(*b"moof", b"frag"));
        bytes.extend(mp4_box(
            *b"mdat",
            &vec![0x55; usize::try_from(16 * 1024 * 1024).unwrap()],
        ));
        bytes
    }

    fn invalid_mp4() -> Vec<u8> {
        b"not-fragmented-mp4".to_vec()
    }

    fn mp4_box(kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + payload.len()).unwrap();
        let mut bytes = Vec::new();
        bytes.extend(size.to_be_bytes());
        bytes.extend(kind);
        bytes.extend(payload);
        bytes
    }

    fn cached_metadata_for_session(
        session: &HlsPlaybackSession,
        resource_id: &str,
    ) -> PersistedHlsCachedResource {
        let resource = session_unique_media_resources(session)
            .into_iter()
            .find(|resource| resource.id == resource_id)
            .unwrap_or(&session.variant.video);
        PersistedHlsCachedResource {
            schema_version: HLS_CACHE_SCHEMA_VERSION,
            id: resource_id.to_owned(),
            content_type: resource.content_type().to_owned(),
            total_length: fake_mp4().len() as u64,
            initialization_length: 28,
            segments: Vec::new(),
            cache_key: PersistedBilibiliMediaCacheKey::from(resource.request.cache_key.clone()),
        }
    }

    fn write_pretty_json<T: Serialize>(path: &Path, value: &T) {
        let mut bytes = serde_json::to_vec_pretty(value).expect("test JSON should serialize");
        bytes.push(b'\n');
        std::fs::write(path, bytes).expect("test JSON should be written");
    }

    fn session_ids(entries: &[HlsCacheCompletedEntry]) -> Vec<&str> {
        entries
            .iter()
            .map(|entry| entry.session_id.as_str())
            .collect()
    }

    #[cfg(unix)]
    fn write_copying_fake_ffmpeg(dir: &Path) -> PathBuf {
        write_executable(
            dir.join("fake-ffmpeg-copy"),
            r#"#!/bin/sh
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
args_log="$script_dir/ffmpeg-args.log"
: > "$args_log"
last=
input=
previous=
for arg in "$@"; do
  printf '%s\n' "$arg" >> "$args_log"
  if [ "$previous" = "-i" ] && [ -z "$input" ]; then
    input=$arg
  fi
  last=$arg
  previous=$arg
done
cp "$input" "$last"
"#,
        )
    }

    #[cfg(unix)]
    fn write_failing_fake_ffmpeg(dir: &Path) -> PathBuf {
        write_executable(
            dir.join("fake-ffmpeg-fail"),
            r#"#!/bin/sh
set -eu
printf '%s\n' 'synthetic transcoding failure' >&2
exit 42
"#,
        )
    }

    #[cfg(unix)]
    fn write_blocking_fake_ffmpeg(dir: &Path) -> PathBuf {
        write_executable(
            dir.join("fake-ffmpeg-blocking"),
            r#"#!/bin/sh
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
: > "$script_dir/ffmpeg-started"
last=
input=
previous=
for arg in "$@"; do
  if [ "$previous" = "-i" ] && [ -z "$input" ]; then
    input=$arg
  fi
  last=$arg
  previous=$arg
done
cp "$input" "$last"
while :; do
  sleep 1
done
"#,
        )
    }

    #[cfg(unix)]
    fn write_executable(path: PathBuf, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(&path, script).expect("fake ffmpeg should be written");
        let mut permissions = std::fs::metadata(&path)
            .expect("fake ffmpeg metadata should be readable")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("fake ffmpeg should be executable");
        path
    }

    async fn wait_for_path(path: &Path) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if path.exists() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}
