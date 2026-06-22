use std::{
    collections::HashSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::http::StatusCode;
use futures_util::StreamExt;
use prost_types::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{OwnedSemaphorePermit, Semaphore},
};

use crate::{
    bbdown_adapter::{
        BilibiliHttpHeader, BilibiliMediaCacheKey, BilibiliMediaRequest, BilibiliMediaRequestKind,
        BilibiliPlaybackVariantKind,
    },
    generated::tvos_net_player::v1::{
        LibraryItem, LibrarySource, MediaVariant, PlaybackProtocol, PlaybackSource,
    },
    hls::{
        HlsAbrGroup, HlsAbrGroupKind, HlsAbrLevel, HlsAbrMetadata, HlsMediaResource,
        HlsMediaResourceMetadata, HlsPlaybackSession, HlsVariant, HlsVariantMetadata,
        mp4_initialization_length, should_forward_media_request_header,
    },
    library::{OpenedMediaFile, open_read_no_follow},
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
const HLS_TRANSCODED_RESOURCE_ID: &str = "transcoded.m4s";
const HLS_TRANSCODED_VIDEO_CODEC: &str = LAN_TRANSCODING_VIDEO_CODEC;
const HLS_TRANSCODED_AUDIO_CODEC: &str = LAN_TRANSCODING_AUDIO_CODEC;
const HLS_TRANSCODING_COMMIT_MARKER_FILE: &str = "transcoding-commit.tmp";
const HLS_TRANSCODING_COMMIT_MARKER_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
pub(crate) struct HlsCacheStore {
    root_path: Arc<PathBuf>,
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

impl HlsCacheStore {
    pub(crate) fn new(root_path: impl Into<PathBuf>) -> Self {
        Self {
            root_path: Arc::new(root_path.into()),
        }
    }

    pub(crate) fn save_session(&self, session: &HlsPlaybackSession) -> io::Result<()> {
        let session_dir = self.session_dir(&session.id)?;
        self.ensure_cache_directory(&session_dir)?;
        self.write_json_atomically(
            &session_dir.join("session.json"),
            &PersistedHlsSession::from(session.clone()),
        )
    }

    pub(crate) fn save_completed_session(&self, session: &HlsPlaybackSession) -> io::Result<()> {
        self.save_session(&sanitized_completed_session(session))
    }

    pub(crate) fn remove_session(&self, session_id: &str) -> io::Result<()> {
        let session_dir = self.session_dir(session_id)?;
        self.reject_cache_path_symlink(&session_dir)?;
        match fs::remove_dir_all(session_dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn remove_session_managed_resources(&self, session_id: &str) -> io::Result<()> {
        let Some(session) = self.load_session(session_id) else {
            return Ok(());
        };
        for resource in session
            .variant
            .audio
            .iter()
            .chain(std::iter::once(&session.variant.video))
        {
            self.remove_cached_resource(session_id, &resource.id)?;
            self.remove_prewarmed_resource(session_id, &resource.id)?;
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
        let mut entries = self
            .load_sessions()?
            .iter()
            .filter_map(|session| self.partial_cache_entry(session))
            .collect::<Vec<_>>();
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

        Some(CachedHlsResource {
            path: file_path,
            content_type: metadata.content_type,
            initialization_length: metadata.initialization_length,
            total_length: metadata.total_length,
            last_modified: file_metadata.modified().unwrap_or(UNIX_EPOCH),
        })
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

    pub(crate) async fn prewarm_session_first_frame_with_control<F>(
        &self,
        client: &reqwest::Client,
        session: &HlsPlaybackSession,
        control: F,
    ) -> Result<(), HlsCacheError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        check_fill_control(&control)?;
        self.save_session(session)?;
        self.prewarm_resource(client, &session.id, &session.variant.video, &control)
            .await?;
        if let Some(audio) = &session.variant.audio {
            self.prewarm_resource(client, &session.id, audio, &control)
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
            .transcode_cached_session_if_needed(session, transcoding, control)
            .await?;
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
        let temp_path = output_path.with_extension("transcode.tmp");
        self.prepare_temp_path(&temp_path)?;
        self.remove_cached_resource(&session.id, HLS_TRANSCODED_RESOURCE_ID)?;

        let transcode_result = run_hls_ffmpeg_transcode(
            &transcoding.ffmpeg_path,
            &cached_video.path,
            cached_audio.as_ref().map(|audio| audio.path.as_path()),
            &temp_path,
            &|| match control() {
                HlsCacheFillControl::Continue => LanTranscodingJobControl::Continue,
                HlsCacheFillControl::Cancel => LanTranscodingJobControl::Cancel,
                HlsCacheFillControl::Preempt => LanTranscodingJobControl::Preempt,
            },
        )
        .await;
        if let Err(error) = transcode_result {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(hls_cache_error_from_transcoding(error));
        }
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
        control: &F,
    ) -> Result<(), HlsCacheError>
    where
        F: Fn() -> HlsCacheFillControl + Send + Sync,
    {
        check_fill_control(control)?;
        if self.cached_resource(session_id, &resource.id).is_some() {
            return Ok(());
        }
        if let Some(prewarmed) = self.prewarmed_resource(session_id, &resource.id) {
            let target_prefix_length =
                hls_first_window_prefetch_prefix_bytes(resource).min(prewarmed.total_length);
            if prewarmed.prefix_length >= target_prefix_length {
                return Ok(());
            }
        }

        let session_dir = self.session_dir(session_id)?;
        self.ensure_cache_directory(&session_dir)?;
        let prewarm_path = self.resource_prewarm_path(session_id, &resource.id)?;
        let temp_path = prewarm_path.with_extension("tmp");
        let mut last_error = None;
        for url in resource_urls(resource) {
            check_fill_control(control)?;
            self.prepare_temp_path(&temp_path)?;
            match download_resource_prefix(client, resource, &url, &temp_path, control).await {
                Ok(prefix) => {
                    if prefix.initialization_length == 0
                        || prefix.initialization_length >= prefix.total_length
                        || prefix.initialization_length > prefix.prefix_length
                    {
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        last_error = Some(HlsCacheError::InvalidResource(
                            "prewarmed HLS MP4 initialization range was invalid".to_owned(),
                        ));
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
                        target_window_seconds: Some(HLS_FIRST_WINDOW_PREFETCH_SECONDS),
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
        let size_bytes = self.cached_resource_total_size(session)?;
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

    fn partial_cache_entry(&self, session: &HlsPlaybackSession) -> Option<HlsCachePartialEntry> {
        if self.session_is_complete(session) {
            return None;
        }
        let size_bytes = self.session_managed_resource_size(session);
        if size_bytes == 0 {
            return None;
        }
        let updated_at = self
            .managed_resource_modification_times(session)
            .into_iter()
            .max()
            .unwrap_or(UNIX_EPOCH);

        Some(HlsCachePartialEntry {
            session_id: session.id.clone(),
            size_bytes,
            updated_at,
        })
    }

    fn session_is_complete(&self, session: &HlsPlaybackSession) -> bool {
        if session.transcoding.state == HlsTranscodingPlanState::Ready {
            return false;
        }
        self.cached_resource(&session.id, &session.variant.video.id)
            .is_some()
            && session
                .variant
                .audio
                .as_ref()
                .is_none_or(|audio| self.cached_resource(&session.id, &audio.id).is_some())
    }

    fn cached_resource_total_size(&self, session: &HlsPlaybackSession) -> Option<u64> {
        let mut total = self
            .cached_resource(&session.id, &session.variant.video.id)?
            .total_length;
        if let Some(audio) = &session.variant.audio {
            total =
                total.checked_add(self.cached_resource(&session.id, &audio.id)?.total_length)?;
        }
        Some(total)
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
            total = total.saturating_add(self.session_managed_resource_size(&session));
        }
        Ok(total)
    }

    fn session_managed_resource_size(&self, session: &HlsPlaybackSession) -> u64 {
        session
            .variant
            .audio
            .iter()
            .chain(std::iter::once(&session.variant.video))
            .map(|resource| self.resource_managed_size(&session.id, &resource.id))
            .sum()
    }

    fn resource_managed_size(&self, session_id: &str, resource_id: &str) -> u64 {
        if let Some(cached) = self.cached_resource(session_id, resource_id) {
            cached.total_length
        } else if let Some(prewarmed) = self.prewarmed_resource(session_id, resource_id) {
            prewarmed.prefix_length
        } else {
            0
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
        session
            .variant
            .audio
            .iter()
            .chain(std::iter::once(&session.variant.video))
            .filter_map(|resource| self.cached_resource(&session.id, &resource.id))
            .map(|resource| resource.last_modified)
            .collect()
    }

    fn managed_resource_modification_times(&self, session: &HlsPlaybackSession) -> Vec<SystemTime> {
        session
            .variant
            .audio
            .iter()
            .chain(std::iter::once(&session.variant.video))
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
            progress(cached.total_length);
            return Ok(cached.total_length);
        }

        let session_dir = self.session_dir(session_id)?;
        self.ensure_cache_directory(&session_dir)?;
        let resource_path = self.resource_path(session_id, &resource.id)?;
        let temp_path = resource_path.with_extension("tmp");
        let mut last_error = None;
        for url in resource_urls(resource) {
            check_fill_control(control)?;
            self.prepare_temp_path(&temp_path)?;
            match download_resource(client, resource, &url, &temp_path, control, &progress).await {
                Ok(total_length) => {
                    let initialization_length =
                        match cached_mp4_initialization_length(&temp_path).await {
                            Ok(length) => length,
                            Err(error) => {
                                let _ = tokio::fs::remove_file(&temp_path).await;
                                last_error = Some(error);
                                continue;
                            }
                        };
                    if initialization_length == 0 || initialization_length >= total_length {
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        last_error = Some(HlsCacheError::InvalidResource(
                            "cached HLS MP4 initialization range was invalid".to_owned(),
                        ));
                        continue;
                    }
                    if let Err(error) = check_fill_control(control) {
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        return Err(error);
                    }
                    if let Err(error) = self.reject_cache_path_symlink(&resource_path) {
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        return Err(error.into());
                    }
                    tokio::fs::rename(&temp_path, &resource_path).await?;
                    let metadata = PersistedHlsCachedResource {
                        schema_version: HLS_CACHE_SCHEMA_VERSION,
                        id: resource.id.clone(),
                        content_type: resource.content_type().to_owned(),
                        total_length,
                        initialization_length,
                        cache_key: PersistedBilibiliMediaCacheKey::from(
                            resource.request.cache_key.clone(),
                        ),
                    };
                    self.write_json_atomically(
                        &self.resource_metadata_path(session_id, &resource.id)?,
                        &metadata,
                    )?;
                    self.remove_prewarmed_resource(session_id, &resource.id)?;
                    check_fill_control(control)?;
                    return Ok(total_length);
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

struct HlsTranscodingCommitGuard {
    store: HlsCacheStore,
    session_id: String,
    active: bool,
}

impl HlsTranscodingCommitGuard {
    fn create_if_needed(store: &HlsCacheStore, session: &HlsPlaybackSession) -> io::Result<Self> {
        let mut guard = Self {
            store: store.clone(),
            session_id: session.id.clone(),
            active: false,
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
        Ok(guard)
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
    for resource in session
        .variant
        .audio
        .iter()
        .chain(std::iter::once(&session.variant.video))
    {
        insert_resource_managed_file_names(&mut retained, &resource.id);
    }
    retained
}

fn insert_resource_managed_file_names(retained: &mut HashSet<String>, resource_id: &str) {
    retained.insert(resource_id.to_owned());
    retained.insert(format!("{resource_id}.json"));
    retained.insert(format!("{resource_id}.prewarm"));
    retained.insert(format!("{resource_id}.prewarm.json"));
}

fn is_managed_resource_file_name(file_name: &str) -> bool {
    if file_name.ends_with(".tmp") {
        return false;
    }
    managed_resource_id_from_file_name(file_name)
        .is_some_and(|resource_id| validate_cache_id(resource_id).is_ok())
}

fn managed_resource_id_from_file_name(file_name: &str) -> Option<&str> {
    for suffix in [".prewarm.json", ".prewarm", ".json"] {
        if let Some(resource_id) = file_name.strip_suffix(suffix) {
            return Some(resource_id);
        }
    }
    Some(file_name)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CachedHlsResource {
    pub(crate) path: PathBuf,
    pub(crate) content_type: String,
    pub(crate) initialization_length: u64,
    pub(crate) total_length: u64,
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

async fn download_resource(
    client: &reqwest::Client,
    resource: &HlsMediaResource,
    url: &str,
    temp_path: &Path,
    control: &(impl Fn() -> HlsCacheFillControl + Send + Sync),
    progress: &(impl Fn(u64) + Send + Sync),
) -> Result<u64, HlsCacheError> {
    check_fill_control(control)?;
    let mut request = client.get(url);
    let mut requested_range = false;
    for header in &resource.request.headers {
        if header.name.eq_ignore_ascii_case("range") {
            requested_range = true;
        }
        if !should_forward_media_request_header(&header.name, &resource.request.url, url) {
            continue;
        }
        request = request.header(header.name.as_str(), header.value.as_str());
    }
    if requested_range {
        return Err(HlsCacheError::InvalidResource(
            "offline HLS cache does not support range-only media requests".to_owned(),
        ));
    }
    let response = send_request_with_control(request, control).await?;
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        return Err(HlsCacheError::UpstreamStatus(status));
    }
    if status == StatusCode::PARTIAL_CONTENT {
        return Err(HlsCacheError::InvalidResource(
            "offline HLS cache received partial content for a full media resource".to_owned(),
        ));
    }
    let declared_length = response.content_length();
    if let (Some(expected), Some(declared)) = (resource.request.size, declared_length)
        && declared != expected
    {
        return Err(HlsCacheError::InvalidResource(format!(
            "HLS resource Content-Length {declared} did not match expected size {expected}"
        )));
    }
    let Some(maximum_length) = resource.request.size.or(declared_length) else {
        return Err(HlsCacheError::InvalidResource(
            "HLS resource length was unknown".to_owned(),
        ));
    };

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)
        .await?;
    let mut stream = response.bytes_stream();
    let mut total_length = 0_u64;
    loop {
        check_fill_control(control)?;
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            () = tokio::time::sleep(Duration::from_millis(100)) => {
                continue;
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk?;
        check_fill_control(control)?;
        total_length = total_length
            .checked_add(chunk.len().try_into().unwrap_or(u64::MAX))
            .ok_or_else(|| {
                HlsCacheError::InvalidResource("HLS resource is too large".to_owned())
            })?;
        if total_length > maximum_length {
            return Err(HlsCacheError::InvalidResource(format!(
                "HLS resource body length exceeded expected size {maximum_length}"
            )));
        }
        file.write_all(&chunk).await?;
        progress(total_length);
    }
    file.sync_all().await?;
    if let Some(declared) = declared_length
        && total_length != declared
    {
        return Err(HlsCacheError::InvalidResource(format!(
            "HLS resource body length {total_length} did not match Content-Length {declared}"
        )));
    }
    if let Some(expected) = resource.request.size
        && total_length != expected
    {
        return Err(HlsCacheError::InvalidResource(format!(
            "HLS resource body length {total_length} did not match expected size {expected}"
        )));
    }

    Ok(total_length)
}

struct DownloadedResourcePrefix {
    prefix_length: u64,
    target_prefix_length: u64,
    total_length: u64,
    initialization_length: u64,
}

async fn download_resource_prefix(
    client: &reqwest::Client,
    resource: &HlsMediaResource,
    url: &str,
    temp_path: &Path,
    control: &(impl Fn() -> HlsCacheFillControl + Send + Sync),
) -> Result<DownloadedResourcePrefix, HlsCacheError> {
    check_fill_control(control)?;
    let target_prefix_length = hls_first_window_prefetch_prefix_bytes(resource);
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

    let response = send_request_with_control(request, control).await?;
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if status != StatusCode::PARTIAL_CONTENT {
        return Err(HlsCacheError::InvalidResource(format!(
            "HLS prewarm expected partial content, got {status}"
        )));
    }
    let headers = response.headers().clone();
    let (start, end, total_length) = parse_content_range_header(&headers).ok_or_else(|| {
        HlsCacheError::InvalidResource(
            "HLS prewarm response did not include Content-Range".to_owned(),
        )
    })?;
    if start != 0 || end < start || end >= total_length {
        return Err(HlsCacheError::InvalidResource(
            "HLS prewarm Content-Range was invalid".to_owned(),
        ));
    }
    let prefix_length = end.saturating_add(1);
    if prefix_length > target_prefix_length {
        return Err(HlsCacheError::InvalidResource(
            "HLS prewarm response exceeded bounded prefix length".to_owned(),
        ));
    }
    if let Some(declared_length) = response.content_length()
        && declared_length != prefix_length
    {
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
        check_fill_control(control)?;
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            () = tokio::time::sleep(Duration::from_millis(100)) => {
                continue;
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk?;
        let next_len = bytes.len().checked_add(chunk.len()).ok_or_else(|| {
            HlsCacheError::InvalidResource("HLS prewarm prefix is too large".to_owned())
        })?;
        if u64::try_from(next_len).unwrap_or(u64::MAX) > prefix_length {
            return Err(HlsCacheError::InvalidResource(
                "HLS prewarm body exceeded Content-Range length".to_owned(),
            ));
        }
        check_fill_control(control)?;
        file.write_all(&chunk).await?;
        bytes.extend_from_slice(&chunk);
    }
    file.sync_all().await?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != prefix_length {
        return Err(HlsCacheError::InvalidResource(format!(
            "HLS prewarm body length {} did not match Content-Range length {prefix_length}",
            bytes.len()
        )));
    }
    let initialization_length = mp4_initialization_length(&bytes).ok_or_else(|| {
        HlsCacheError::InvalidResource("prewarmed HLS MP4 init box not found".to_owned())
    })?;

    Ok(DownloadedResourcePrefix {
        prefix_length,
        target_prefix_length,
        total_length,
        initialization_length,
    })
}

fn hls_first_window_prefetch_prefix_bytes(resource: &HlsMediaResource) -> u64 {
    let bitrate_window_bytes = resource
        .request
        .bandwidth
        .map(|bandwidth_bits_per_second| {
            bandwidth_bits_per_second
                .saturating_mul(HLS_FIRST_WINDOW_PREFETCH_SECONDS)
                .saturating_add(7)
                / 8
        })
        .unwrap_or_default();
    let target = HLS_PREWARM_HEAD_BYTES
        .saturating_add(bitrate_window_bytes)
        .clamp(HLS_PREWARM_HEAD_BYTES, HLS_FIRST_WINDOW_PREFETCH_MAX_BYTES);

    resource
        .request
        .size
        .filter(|size| *size > 0)
        .map(|size| target.min(size))
        .unwrap_or(target)
        .max(1)
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
    completed.alternate_variants.clear();
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

fn resource_urls(resource: &HlsMediaResource) -> Vec<String> {
    let mut urls = Vec::with_capacity(resource.request.backup_urls.len() + 1);
    if !resource.request.url.trim().is_empty() {
        urls.push(resource.request.url.clone());
    }
    urls.extend(resource.request.backup_urls.clone());
    urls
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
    session.alternate_variants.clear();
    session
}

pub(crate) fn completed_runtime_session(session: &HlsPlaybackSession) -> HlsPlaybackSession {
    let mut session = session.clone();
    session.advertise_alternate_variants = false;
    sanitize_completed_resource(&mut session.variant.video);
    if let Some(audio) = session.variant.audio.as_mut() {
        sanitize_completed_resource(audio);
    }
    session
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
    variant: PersistedHlsVariant,
    #[serde(default)]
    alternate_variants: Vec<PersistedHlsVariant>,
    #[serde(default)]
    abr: PersistedHlsAbrMetadata,
    #[serde(default)]
    variants: Vec<PersistedHlsVariantMetadata>,
    #[serde(default)]
    transcoding: PersistedHlsTranscodingPlan,
}

impl From<HlsPlaybackSession> for PersistedHlsSession {
    fn from(session: HlsPlaybackSession) -> Self {
        Self {
            schema_version: HLS_CACHE_SCHEMA_VERSION,
            id: session.id,
            title: session.title,
            variant: PersistedHlsVariant::from(session.variant),
            alternate_variants: session
                .alternate_variants
                .into_iter()
                .map(PersistedHlsVariant::from)
                .collect(),
            abr: PersistedHlsAbrMetadata::from(session.abr),
            variants: session
                .variants
                .into_iter()
                .map(PersistedHlsVariantMetadata::from)
                .collect(),
            transcoding: PersistedHlsTranscodingPlan::from(session.transcoding),
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
            variant: HlsVariant::try_from(session.variant)?,
            alternate_variants: session
                .alternate_variants
                .into_iter()
                .map(HlsVariant::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            advertise_alternate_variants: true,
            abr: HlsAbrMetadata::from(session.abr),
            variants: session
                .variants
                .into_iter()
                .map(HlsVariantMetadata::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            transcoding: HlsTranscodingPlan::from(session.transcoding),
        })
    }
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
    cache_key: PersistedBilibiliMediaCacheKey,
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
    use axum::{
        Router,
        body::Body,
        http::{HeaderMap, HeaderValue, Response, header::CONTENT_LENGTH, header::CONTENT_TYPE},
        routing::get,
    };
    use tempfile::TempDir;

    use super::*;

    fn temp_store(temp: &TempDir) -> HlsCacheStore {
        HlsCacheStore::new(
            temp.path()
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(temp.path())),
        )
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
    fn completed_runtime_session_hides_alternates_from_new_master_but_keeps_lookup() {
        let mut session = sample_session("session-runtime", "https://example.test/video.m4s");
        attach_sample_alternate_variant(&mut session, "https://example.test/720p-video.m4s");

        let runtime = completed_runtime_session(&session);

        assert_eq!(1, runtime.alternate_variants.len());
        assert!(!runtime.master_playlist().contains("segments/v1-video.m3u8"));
        assert!(runtime.media_playlist_resource("v1-video.m3u8").is_some());
        assert_eq!(
            "https://example.test/720p-video.m4s",
            runtime
                .media_resource("v1-video.m4s")
                .expect("runtime alternate resource should remain addressable")
                .request
                .url
        );
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
        object.remove("abr");
        object.remove("variants");
        object.remove("transcoding");

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
        assert_eq!(session, sessions[0]);
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
            !store
                .resource_path("session-transcoded", "video.m4s")
                .unwrap()
                .exists()
        );
        assert!(
            !store
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
    fn usage_snapshot_removes_orphaned_transcoding_sources_after_manifest_rewrite() {
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
        for temp_name in ["video.tmp", "transcoded.transcode.tmp"] {
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
        assert_eq!(fake_mp4().len() as u64, usage.used_bytes);
        assert!(
            store
                .cached_resource(&completed_session.id, HLS_TRANSCODED_RESOURCE_ID)
                .is_some()
        );
        for resource_id in ["video.m4s", "audio.m4s"] {
            assert!(
                !store
                    .resource_path(&completed_session.id, resource_id)
                    .expect("resource path should be valid")
                    .exists()
            );
            assert!(
                !store
                    .resource_metadata_path(&completed_session.id, resource_id)
                    .expect("metadata path should be valid")
                    .exists()
            );
        }
        for temp_name in ["video.tmp", "transcoded.transcode.tmp"] {
            assert!(
                store
                    .resource_path(&completed_session.id, temp_name)
                    .expect("temporary resource path should be valid")
                    .exists()
            );
        }
    }

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
        let _guard = HlsTranscodingCommitGuard::create_if_needed(&store, &source_session)
            .expect("active transcoding marker should be created");

        let usage = store
            .usage_snapshot()
            .expect("usage snapshot should preserve active transcode output");

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
        let task = tokio::spawn(async move {
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

        wait_for_path(&temp.path().join("ffmpeg-started")).await;
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
            .save_completed_session(&session)
            .expect("tampered session manifest should save");

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

    #[tokio::test]
    async fn rejects_short_hls_cache_response_with_declared_size() {
        let (upstream_url, _task) = start_short_mp4_upstream().await;
        let temp = TempDir::new().expect("temp dir should be created");
        let store = temp_store(&temp);
        let mut session = sample_session("session-short", &upstream_url);
        session.variant.video.request.size = Some(fake_mp4().len() as u64);
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("short response should be rejected");

        assert!(error.to_string().contains("expected size"));
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
            .resource_path("session-lengthless", "video.m4s")
            .expect("resource path should be valid")
            .with_extension("tmp");
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("lengthless response should be rejected");

        assert!(error.to_string().contains("length was unknown"));
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
            .resource_path("session-overlong", "video.m4s")
            .expect("resource path should be valid")
            .with_extension("tmp");
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("overlong response should be rejected");

        assert!(error.to_string().contains("exceeded expected size"));
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

        assert!(error.to_string().contains("partial content"));
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
            .resource_path("session-invalid", "video.m4s")
            .expect("resource path should be valid")
            .with_extension("tmp");
        let client = reqwest::Client::new();

        let error = store
            .cache_session_resources(&client, &session)
            .await
            .expect_err("invalid MP4 should be rejected");

        assert!(error.to_string().contains("init box not found"));
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
            .resource_path("session-backup", "video.m4s")
            .expect("resource path should be valid")
            .with_extension("tmp");
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
        assert!(sessions[0].alternate_variants.is_empty());
        let request = &sessions[0].variant.video.request;
        assert!(request.url.is_empty());
        assert!(request.backup_urls.is_empty());
        assert!(request.headers.is_empty());
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
            .resource_path("session-temp-symlink", "video.m4s")
            .expect("resource path should be valid")
            .with_extension("tmp");
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

    async fn upstream_prewarm_mp4(headers: HeaderMap) -> Response<Body> {
        upstream_prewarm_mp4_bytes(headers, fake_mp4())
    }

    async fn upstream_large_prewarm_mp4(headers: HeaderMap) -> Response<Body> {
        upstream_prewarm_mp4_bytes(headers, large_prefetch_fake_mp4())
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
        PersistedHlsCachedResource {
            schema_version: HLS_CACHE_SCHEMA_VERSION,
            id: resource_id.to_owned(),
            content_type: session.variant.video.content_type().to_owned(),
            total_length: fake_mp4().len() as u64,
            initialization_length: 28,
            cache_key: PersistedBilibiliMediaCacheKey::from(
                session.variant.video.request.cache_key.clone(),
            ),
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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
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
