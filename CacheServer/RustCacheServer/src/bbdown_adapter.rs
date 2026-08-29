use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fmt::Display,
    future::Future,
    mem,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use bbdown_core::{
    BiliClient, ClientConfig, CredentialProfileSelection, CredentialStore, Credentials,
    DanmakuFormat, DownloadArchive, DownloadCancellationToken, DownloadFileKind, DownloadMode,
    DownloadOptions, DownloadPlan, DownloadProgressEvent, DownloadProgressSink, DownloadReport,
    DuplicateDecision, EntryDownloadReport, Error as BbdownError, HttpHeaderSpec, IndexSelection,
    Input, MediaRequestKind, MediaRequestSpec, MuxOptions, MuxReport, PlaybackAbrGroup,
    PlaybackAbrGroupKind, PlaybackAbrLevel, PlaybackAbrMetadata, PlaybackCodecPreference,
    PlaybackPlan, PlaybackVariant, PlaybackVariantKind, PlayurlMode, ResolvedContent,
    RestrictedArea, RestrictedAreaConfig, RestrictedAreaProxy, Selection, StreamSelection,
    SubtitleAiPolicy, VideoCollectionKind,
};
use tokio::{
    fs,
    io::AsyncReadExt,
    process::Command,
    sync::Mutex,
    time::{Instant, sleep},
};

use crate::{
    bilibili_playback::{
        BilibiliContentIdentity, BilibiliContentKind, BilibiliInputResolution,
        BilibiliResolvedCandidate, MAX_BILIBILI_RESOLUTION_SNAPSHOT_BYTES,
        MAX_BILIBILI_RESOLUTION_STRING_BYTES, MAX_BILIBILI_RESOLVE_CANDIDATE_LIMIT,
    },
    bilibili_resolution::BilibiliTaskCandidateRecord,
    bilibili_worker::{
        BilibiliDownloadAdapter, BilibiliDownloadContext, BilibiliDownloadError,
        BilibiliDownloadFuture, BilibiliDownloadOutput, BilibiliDownloadOutputV2,
        BilibiliDownloadRequest, BilibiliTaskResourceBody, BilibiliTaskResourceBodySource,
        cleanup_unpublished_output_paths,
    },
    config::{
        BbdownRestrictedArea as CacheBbdownRestrictedArea,
        BbdownRestrictedProxy as CacheBbdownRestrictedProxy, CacheServerOptions,
    },
    generated::tvos_net_player::v1::{
        BilibiliApiMode, BilibiliContentIdentity as ProtoBilibiliContentIdentity,
        BilibiliDanmakuFormat, BilibiliDownloadMode, BilibiliDownloadOptions,
        BilibiliRequestContext, BilibiliSubtitleAiPolicy, BilibiliTaskResultDetails,
        CacheResourceRef, TaskArtifact, TaskArtifactKind, TaskArtifactState, TaskProblem,
        TaskProblemCategory, TaskResult, TaskResultProgress, TaskResultProviderDetails,
        TaskResultSubject, TaskState,
    },
    library::{LibraryItemPublicationLease, LocalMediaLibrary},
    playback_policy::{
        CompatibleVariantPreference, PlaybackPolicy, variant_is_avplayer_h264_aac_hls_compatible,
    },
    task_output::TaskResourceRecord,
    task_registry::BilibiliTaskProgress,
};
use uuid::Uuid;

const DOWNLOAD_PROGRESS_START: f64 = 0.10;
const DOWNLOAD_PROGRESS_END: f64 = 0.80;
const ACTIVE_ENTRY_INCOMPLETE_PROGRESS_CAP: f64 = 0.50;
const BBDOWN_CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(50);
const BBDOWN_CANCELLATION_GRACE_PERIOD: Duration = Duration::from_secs(5);
const DOWNLOAD_PROGRESS_PUBLISH_MIN_BYTES: u64 = 32 * 1024 * 1024;
const DOWNLOAD_PROGRESS_PUBLISH_MIN_FRACTION: f64 = 0.01;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BilibiliResolveCandidateWindow {
    candidate_limit: usize,
    truncation_probe_limit: u32,
}

impl BilibiliResolveCandidateWindow {
    fn new(candidate_limit: usize) -> Result<Self, BilibiliDownloadError> {
        if !(1..=MAX_BILIBILI_RESOLVE_CANDIDATE_LIMIT).contains(&candidate_limit) {
            return Err(invalid_resolve_candidate_limit(candidate_limit));
        }
        let candidate_limit_u32 = u32::try_from(candidate_limit)
            .map_err(|_| invalid_resolve_candidate_limit(candidate_limit))?;
        let truncation_probe_limit = candidate_limit_u32
            .checked_add(1)
            .ok_or_else(|| invalid_resolve_candidate_limit(candidate_limit))?;
        Ok(Self {
            candidate_limit,
            truncation_probe_limit,
        })
    }
}

fn invalid_resolve_candidate_limit(candidate_limit: usize) -> BilibiliDownloadError {
    BilibiliDownloadError::Failed(format!(
        "Bilibili resolve candidate limit must be between 1 and {MAX_BILIBILI_RESOLVE_CANDIDATE_LIMIT}; received {candidate_limit}."
    ))
}

pub struct BbdownBilibiliAdapter {
    options: Arc<CacheServerOptions>,
    client: BiliClient,
    tv_client: BiliClient,
    library: Arc<LocalMediaLibrary>,
    output_dir: PathBuf,
    archive_path: PathBuf,
    ffmpeg_path: PathBuf,
    archive_lock: Arc<Mutex<()>>,
    #[cfg(test)]
    archive_save_fail_after: StdMutex<Option<usize>>,
}

#[derive(Default)]
struct V2TaskArchive {
    accepted: DownloadArchive,
}

impl V2TaskArchive {
    fn stage_candidate(&self) -> DownloadArchive {
        self.accepted.clone()
    }

    fn accept_candidate(&mut self, candidate: DownloadArchive) {
        self.accepted = candidate;
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BilibiliPlaybackPlan {
    pub title: String,
    pub entries: Vec<BilibiliPlaybackEntry>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BilibiliPlaybackEntry {
    pub index: u32,
    pub aid: u64,
    pub bvid: Option<String>,
    pub cid: u64,
    pub epid: Option<u64>,
    pub title: String,
    pub content_id: String,
    pub duration_seconds: Option<u32>,
    pub abr: BilibiliPlaybackAbrMetadata,
    pub selected_variant: Option<BilibiliSelectedPlaybackVariant>,
    pub variants: Vec<BilibiliPlaybackVariant>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BilibiliSelectedPlaybackVariant {
    pub variant: BilibiliPlaybackVariant,
    pub selection: BilibiliPlaybackVariantSelection,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BilibiliPlaybackVariant {
    pub id: String,
    pub kind: BilibiliPlaybackVariantKind,
    pub content_id: String,
    pub bandwidth: Option<u64>,
    pub codecs: Vec<String>,
    pub mime_types: Vec<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<String>,
    pub duration_seconds: Option<u32>,
    pub abr: Option<BilibiliPlaybackAbrLevel>,
    pub video: Option<BilibiliMediaRequest>,
    pub audio: Option<BilibiliMediaRequest>,
    pub flv_segments: Vec<BilibiliMediaRequest>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BilibiliPlaybackAbrMetadata {
    pub groups: Vec<BilibiliPlaybackAbrGroup>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BilibiliPlaybackAbrGroup {
    pub id: String,
    pub kind: BilibiliPlaybackAbrGroupKind,
    pub variant_ids: Vec<String>,
    pub level_count: u32,
    pub min_bandwidth: Option<u64>,
    pub max_bandwidth: Option<u64>,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BilibiliPlaybackAbrGroupKind {
    DashVideo,
    DashAudioOnly,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BilibiliPlaybackAbrLevel {
    pub group_id: String,
    pub level_index: u32,
    pub level_count: u32,
    pub switchable: bool,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BilibiliMediaRequest {
    pub kind: BilibiliMediaRequestKind,
    pub stream_id: Option<u32>,
    pub url: String,
    pub backup_urls: Vec<String>,
    pub headers: Vec<BilibiliHttpHeader>,
    pub mime_type: Option<String>,
    pub codecs: Option<String>,
    pub bandwidth: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<String>,
    pub size: Option<u64>,
    pub duration_seconds: Option<u32>,
    pub cache_key: BilibiliMediaCacheKey,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BilibiliHttpHeader {
    pub name: String,
    pub value: String,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BilibiliMediaCacheKey {
    pub content_id: String,
    pub media_kind: BilibiliMediaRequestKind,
    pub stream_id: Option<u32>,
    pub codecs: Option<String>,
    pub source_hash: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BilibiliPlaybackVariantKind {
    Dash,
    Flv,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BilibiliMediaRequestKind {
    Video,
    Audio,
    FlvSegment,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BilibiliPlaybackVariantSelection {
    pub policy: BilibiliPlaybackVariantSelectionPolicy,
    pub codec_rank: Option<usize>,
    pub score: i32,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BilibiliPlaybackVariantSelectionPolicy {
    AvPlayerDefault,
    ExplicitEncodingPreference,
    H264AacFallback,
    AvPlayerHintFallback,
}

#[allow(dead_code)]
struct PlaybackCodecPreferenceCandidate {
    policy: BilibiliPlaybackVariantSelectionPolicy,
    preference: PlaybackCodecPreference,
}

#[allow(dead_code)]
struct PlaybackVariantPreferences {
    codec_candidates: Vec<PlaybackCodecPreferenceCandidate>,
    quality_preference: Option<u32>,
    allow_avplayer_hint_fallback: bool,
    encoding_preference: Option<String>,
    prefer_conservative_compatible: bool,
}

#[allow(dead_code)]
struct SelectedCorePlaybackVariant<'a> {
    variant: &'a PlaybackVariant,
    selection: BilibiliPlaybackVariantSelection,
}

impl BbdownBilibiliAdapter {
    pub fn new(options: Arc<CacheServerOptions>, library: Arc<LocalMediaLibrary>) -> Self {
        let client_config = bbdown_client_config(&options, PlayurlMode::Web)
            .unwrap_or_else(|error| panic!("failed to configure BBDown client: {error:?}"));
        let tv_client_config = bbdown_client_config(&options, PlayurlMode::Tv)
            .unwrap_or_else(|error| panic!("failed to configure BBDown TV client: {error:?}"));
        Self {
            client: BiliClient::new(client_config),
            tv_client: BiliClient::new(tv_client_config),
            library,
            output_dir: options.bbdown_output_dir(),
            archive_path: options.bbdown_archive_path(),
            ffmpeg_path: options.bbdown_ffmpeg_path.clone(),
            archive_lock: Arc::new(Mutex::new(())),
            #[cfg(test)]
            archive_save_fail_after: StdMutex::new(None),
            options,
        }
    }

    async fn run_inner(
        &self,
        request: BilibiliDownloadRequest,
        context: BilibiliDownloadContext,
    ) -> Result<BilibiliDownloadOutput, BilibiliDownloadError> {
        if context.is_cancel_requested() {
            return Err(BilibiliDownloadError::Cancelled(
                "Cancelled before the BBDown adapter started.".to_owned(),
            ));
        }

        if !request.candidates.is_empty() {
            return self.run_v2_download(request, context).await;
        }

        let input = Input::parse(&request.source).map_err(failed)?;
        let download_options = self.download_options(request.options.as_ref())?;
        let client =
            self.client_for_request(request.options.as_ref(), request.request_context.as_ref())?;
        context.report_progress(progress(
            0.02,
            "Planning Bilibili download with BBDown core.",
        ));
        let selection = default_selection_for_input(&input);
        let plan = run_bbdown_until_cancelled(
            client.plan(input, selection),
            || context.is_cancel_requested(),
            "Cancelled while BBDown planning was running.",
        )
        .await?;

        if context.is_cancel_requested() {
            return Err(BilibiliDownloadError::Cancelled(
                "Cancelled after Bilibili planning completed.".to_owned(),
            ));
        }

        context.report_progress(progress(
            0.10,
            format!("Downloading {} Bilibili entry(s).", plan.entries.len()),
        ));
        let _archive_guard = self.archive_lock.lock().await;
        if context.is_cancel_requested() {
            return Err(BilibiliDownloadError::Cancelled(
                "Cancelled before the BBDown download started.".to_owned(),
            ));
        }

        let mut archive = DownloadArchive::load(&self.archive_path).map_err(failed)?;
        let download_cancellation = DownloadCancellationToken::new();
        let download_progress = BilibiliBbdownProgressSink::new(context.clone());
        let report = run_bbdown_download_until_cancelled(
            client.download_plan_with_archive_decision_with_progress_and_cancellation(
                &plan,
                download_options,
                &mut archive,
                DuplicateDecision::KeepBoth,
                &download_progress,
                &download_cancellation,
            ),
            &download_cancellation,
            || context.is_cancel_requested(),
            "Cancelled while the BBDown download was running.",
        )
        .await?;

        let downloaded_bytes = report.summary().total_bytes;
        if context.is_cancel_requested() {
            cleanup_downloaded_media_sources(&report).await;
            return Err(BilibiliDownloadError::Cancelled(
                "Cancelled before BBDown muxing started.".to_owned(),
            ));
        }

        context.report_progress(BilibiliTaskProgress {
            progress: Some(0.80),
            downloaded_bytes: Some(to_i64_saturating(downloaded_bytes)),
            total_bytes: Some(to_i64_saturating(downloaded_bytes)),
            message: Some("Muxing downloaded media for local playback.".to_owned()),
        });
        let is_cancel_requested = || context.is_cancel_requested();
        let report = mux_download_report(report, &self.ffmpeg_path, &is_cancel_requested).await?;

        if context.is_cancel_requested() {
            return Err(BilibiliDownloadError::Cancelled(
                "Cancelled after BBDown muxing completed.".to_owned(),
            ));
        }

        context.report_progress(BilibiliTaskProgress {
            progress: Some(0.95),
            downloaded_bytes: Some(to_i64_saturating(downloaded_bytes)),
            total_bytes: Some(to_i64_saturating(downloaded_bytes)),
            message: Some("Indexing downloaded media in the cache library.".to_owned()),
        });

        if context.is_cancel_requested() {
            return Err(BilibiliDownloadError::Cancelled(
                "Cancelled after the BBDown download finished.".to_owned(),
            ));
        }

        let candidates = playable_output_candidates(&report);
        for candidate in &candidates {
            if let Some(library_item_id) =
                self.library.item_id_for_media_path(candidate.clone()).await
            {
                if context.is_cancel_requested() {
                    return Err(BilibiliDownloadError::Cancelled(
                        "Cancelled before committing the BBDown archive.".to_owned(),
                    ));
                }
                self.save_archive(&archive)?;
                return Ok(BilibiliDownloadOutput {
                    library_item_id,
                    message: success_message(&report),
                    v2: None,
                });
            }
        }

        Err(BilibiliDownloadError::Failed(format!(
            "BBDown finished but produced no playable cache item under {}. Ensure ffmpeg is installed and muxing outputs .mp4 files.",
            self.library.root_path().display()
        )))
    }

    async fn run_v2_download(
        &self,
        request: BilibiliDownloadRequest,
        context: BilibiliDownloadContext,
    ) -> Result<BilibiliDownloadOutput, BilibiliDownloadError> {
        let client =
            self.client_for_request(request.options.as_ref(), request.request_context.as_ref())?;
        let download_mode = download_mode_from_options(request.options.as_ref())?;
        let input = playback_input_for_planning(&request.source)?;
        let mut results = Vec::with_capacity(request.candidates.len());
        let mut retained_backing = RetainedV2DownloadBacking::default();
        let mut primary_library_item_id = String::new();
        let mut successful_results = 0_usize;
        let mut cancelled = false;
        let mut completed_downloaded_bytes = 0_u64;
        let mut total_bytes_floor = 0_u64;

        let _archive_guard = self.archive_lock.lock().await;
        // V2 task output is the durable authority. This task-local archive only coordinates
        // duplicate names between candidates and is never committed ahead of terminal output.
        let mut archive = V2TaskArchive::default();

        for (offset, candidate) in request.candidates.iter().enumerate() {
            let result_id = bilibili_v2_result_id(&request.task_id, offset);
            if cancelled || context.is_cancel_requested() {
                cancelled = true;
                results.push(cancelled_download_result(result_id, candidate));
                continue;
            }

            context.report_progress(progress(
                v2_candidate_progress(offset, request.candidates.len(), 0.02),
                format!(
                    "Planning Bilibili download {}/{}.",
                    offset + 1,
                    request.candidates.len()
                ),
            ));

            let plan = match self
                .plan_download_candidate(&client, &input, candidate, || {
                    context.is_cancel_requested()
                })
                .await
            {
                Ok(plan) => plan,
                Err(BilibiliDownloadError::Cancelled(_)) => {
                    cancelled = true;
                    results.push(cancelled_download_result(result_id, candidate));
                    continue;
                }
                Err(error) => {
                    results.push(failed_download_result(result_id, candidate, &error));
                    continue;
                }
            };

            let download_options = self.download_options(request.options.as_ref())?;
            let download_cancellation = DownloadCancellationToken::new();
            let download_progress = BilibiliBbdownProgressSink::for_v2_candidate(
                context.clone(),
                offset,
                request.candidates.len(),
                completed_downloaded_bytes,
                total_bytes_floor,
            );
            let mut candidate_archive = archive.stage_candidate();
            let report = match run_bbdown_download_until_cancelled(
                client.download_plan_with_archive_decision_with_progress_and_cancellation(
                    &plan,
                    download_options,
                    &mut candidate_archive,
                    DuplicateDecision::KeepBoth,
                    &download_progress,
                    &download_cancellation,
                ),
                &download_cancellation,
                || context.is_cancel_requested(),
                "Cancelled while the BBDown download was running.",
            )
            .await
            {
                Ok(report) => report,
                Err(BilibiliDownloadError::Cancelled(_)) => {
                    let snapshot = download_progress.v2_progress_snapshot();
                    completed_downloaded_bytes = snapshot.downloaded_bytes;
                    total_bytes_floor = snapshot.total_bytes;
                    cancelled = true;
                    results.push(cancelled_download_result(result_id, candidate));
                    continue;
                }
                Err(error) => {
                    let snapshot = download_progress.v2_progress_snapshot();
                    completed_downloaded_bytes = snapshot.downloaded_bytes;
                    total_bytes_floor = snapshot.total_bytes;
                    log_v2_candidate_error(&request.task_id, &result_id, &error);
                    results.push(failed_download_result(result_id, candidate, &error));
                    report_v2_candidate_finished(
                        &context,
                        offset,
                        request.candidates.len(),
                        completed_downloaded_bytes,
                        total_bytes_floor,
                    );
                    continue;
                }
            };
            let snapshot = download_progress.v2_progress_snapshot();
            completed_downloaded_bytes =
                snapshot.downloaded_bytes.max(report.summary().total_bytes);
            total_bytes_floor = snapshot.total_bytes.max(completed_downloaded_bytes);

            context.report_progress(BilibiliTaskProgress {
                progress: Some(v2_candidate_progress(
                    offset,
                    request.candidates.len(),
                    0.85,
                )),
                downloaded_bytes: Some(to_i64_saturating(completed_downloaded_bytes)),
                total_bytes: Some(to_i64_saturating(total_bytes_floor)),
                message: Some(format!(
                    "Muxing Bilibili download {}/{}.",
                    offset + 1,
                    request.candidates.len()
                )),
            });

            let mut report = report;
            match mux_download_report_in_place(&mut report, &self.ffmpeg_path, &|| {
                context.is_cancel_requested()
            })
            .await
            {
                Ok(()) => {}
                Err(BilibiliDownloadError::Cancelled(_)) => {
                    cleanup_unpublished_download_report(&report).await;
                    cancelled = true;
                    results.push(cancelled_download_result(result_id, candidate));
                    continue;
                }
                Err(error) => {
                    cleanup_unpublished_download_report(&report).await;
                    log_v2_candidate_error(&request.task_id, &result_id, &error);
                    results.push(failed_download_result(result_id, candidate, &error));
                    report_v2_candidate_finished(
                        &context,
                        offset,
                        request.candidates.len(),
                        completed_downloaded_bytes,
                        total_bytes_floor,
                    );
                    continue;
                }
            };

            let mapped = self
                .finalize_v2_download_result(
                    result_id.clone(),
                    candidate,
                    &plan,
                    report,
                    download_mode,
                    context.is_cancel_requested(),
                )
                .await;
            match mapped {
                Ok(mapped) => {
                    archive.accept_candidate(candidate_archive);
                    retain_v2_success(
                        mapped,
                        &mut primary_library_item_id,
                        &mut successful_results,
                        &mut results,
                        &mut retained_backing,
                    );
                }
                Err(BilibiliDownloadError::Cancelled(_)) => {
                    cancelled = true;
                    results.push(cancelled_download_result(result_id, candidate));
                }
                Err(error) => {
                    log_v2_candidate_error(&request.task_id, &result_id, &error);
                    results.push(failed_download_result(result_id, candidate, &error));
                }
            }
            report_v2_candidate_finished(
                &context,
                offset,
                request.candidates.len(),
                completed_downloaded_bytes,
                total_bytes_floor,
            );
        }

        let total = request.candidates.len();
        let terminal_state = if cancelled {
            TaskState::Cancelled
        } else if successful_results > 0 {
            TaskState::Succeeded
        } else {
            TaskState::Failed
        };
        let message = match terminal_state {
            TaskState::Cancelled => format!(
                "Cancelled after completing {successful_results}/{total} Bilibili result(s)."
            ),
            TaskState::Succeeded if successful_results == total => {
                format!("Downloaded all {total} Bilibili result(s).")
            }
            TaskState::Succeeded => {
                format!("Downloaded {successful_results}/{total} Bilibili result(s).")
            }
            TaskState::Failed => format!("Failed to download all {total} Bilibili result(s)."),
            _ => unreachable!("v2 download must finish in a terminal state"),
        };

        Ok(BilibiliDownloadOutput {
            library_item_id: primary_library_item_id,
            message,
            v2: Some(BilibiliDownloadOutputV2 {
                terminal_state,
                results,
                resources: retained_backing.resources,
                resource_bodies: retained_backing.resource_bodies,
                library_item_leases: retained_backing.library_item_leases,
                unpublished_output_paths: retained_backing.unpublished_output_paths,
                transient_output_paths: retained_backing.transient_output_paths,
            }),
        })
    }

    fn save_archive(&self, archive: &DownloadArchive) -> Result<(), BilibiliDownloadError> {
        #[cfg(test)]
        {
            let mut remaining = self
                .archive_save_fail_after
                .lock()
                .expect("archive save failure hook lock poisoned");
            if let Some(remaining) = remaining.as_mut() {
                if *remaining == 0 {
                    return Err(BilibiliDownloadError::Failed(
                        "Injected BBDown archive persistence failure.".to_owned(),
                    ));
                }
                *remaining -= 1;
            }
        }
        archive.save(&self.archive_path).map_err(failed)
    }

    async fn plan_download_candidate(
        &self,
        client: &BiliClient,
        input: &Input,
        candidate: &BilibiliTaskCandidateRecord,
        is_cancel_requested: impl Fn() -> bool,
    ) -> Result<DownloadPlan, BilibiliDownloadError> {
        let PlaybackInputSelection {
            input_override,
            selection,
            expected_identity,
        } = playback_selection_from_id(input, Some(&candidate.selection_id))?;
        let direct_collection_item = input_override.is_some();
        let selected_input = input_override.unwrap_or_else(|| input.clone());
        let selection = if direct_collection_item {
            Some(
                resolve_direct_collection_item_page(
                    client,
                    &selected_input,
                    expected_identity
                        .as_ref()
                        .expect("direct collection item must retain expected identity"),
                    &is_cancel_requested,
                )
                .await?,
            )
        } else {
            selection.or_else(|| default_selection_for_input(&selected_input))
        };
        let plan = run_bbdown_until_cancelled(
            client.plan(selected_input, selection),
            &is_cancel_requested,
            "Cancelled while BBDown planning was running.",
        )
        .await?;
        let matching_entries = plan
            .entries
            .into_iter()
            .filter(|entry| download_entry_matches_candidate(entry, candidate))
            .collect::<Vec<_>>();
        if matching_entries.len() != 1 {
            return Err(BilibiliDownloadError::Failed(
                "Selected Bilibili content no longer matches the accepted resolution snapshot. Resolve the input again and retry."
                    .to_owned(),
            ));
        }
        Ok(DownloadPlan {
            title: plan.title,
            entries: matching_entries,
        })
    }

    async fn finalize_v2_download_result(
        &self,
        result_id: String,
        candidate: &BilibiliTaskCandidateRecord,
        plan: &DownloadPlan,
        report: DownloadReport,
        download_mode: DownloadMode,
        cancel_requested: bool,
    ) -> Result<MappedV2DownloadResult, BilibiliDownloadError> {
        if cancel_requested {
            cleanup_unpublished_download_report(&report).await;
            return Err(BilibiliDownloadError::Cancelled(
                "Cancelled after BBDown finished downloading.".to_owned(),
            ));
        }

        match self
            .map_v2_download_result(result_id, candidate, plan, &report, download_mode)
            .await
        {
            Ok(mapped) => Ok(mapped),
            Err(error) => {
                cleanup_unpublished_download_report(&report).await;
                Err(error)
            }
        }
    }

    async fn map_v2_download_result(
        &self,
        result_id: String,
        candidate: &BilibiliTaskCandidateRecord,
        plan: &DownloadPlan,
        report: &DownloadReport,
        download_mode: DownloadMode,
    ) -> Result<MappedV2DownloadResult, BilibiliDownloadError> {
        let entry = report.entries.first().ok_or_else(|| {
            BilibiliDownloadError::Failed(
                "BBDown returned no entry for the selected Bilibili result.".to_owned(),
            )
        })?;
        if report.entries.len() != 1 {
            return Err(BilibiliDownloadError::Failed(
                "BBDown returned an ambiguous multi-entry report for one accepted Bilibili result."
                    .to_owned(),
            ));
        }
        let plan_entry = plan.entries.first().ok_or_else(|| {
            BilibiliDownloadError::Failed(
                "BBDown returned no plan entry for the selected Bilibili result.".to_owned(),
            )
        })?;

        let mut library_item_id = String::new();
        let mut library_item_lease = None;
        let mut library_output_path = None;
        for candidate_path in playable_entry_output_candidates(entry) {
            if let Some(lease) = self
                .library
                .reserve_media_path_for_publication(candidate_path.clone())
                .await
            {
                library_item_id = lease.item_id.clone();
                library_item_lease = Some(lease);
                library_output_path = Some(candidate_path);
                break;
            }
        }
        if download_mode_requires_media(download_mode) && library_item_id.is_empty() {
            return Err(BilibiliDownloadError::Failed(
                "BBDown finished but produced no playable cache-library item for the selected result."
                    .to_owned(),
            ));
        }

        let mut artifacts = Vec::new();
        let mut resources = Vec::new();
        let mut resource_bodies = Vec::new();

        if !library_item_id.is_empty() {
            artifacts.push(library_media_artifact(entry, &library_item_id));
        }
        for (index, file) in entry
            .files
            .iter()
            .filter(|file| !file.kind.is_media())
            .enumerate()
        {
            let mapped = map_sidecar_artifact(file, index).await?;
            artifacts.push(mapped.artifact);
            resources.push(mapped.resource);
            resource_bodies.push(mapped.body);
        }
        if let Some(required_kind) = sidecar_only_required_artifact_kind(download_mode)
            && !artifacts
                .iter()
                .any(|artifact| artifact.kind() == required_kind)
        {
            return Err(BilibiliDownloadError::Failed(
                "BBDown finished but did not produce the requested sidecar artifact.".to_owned(),
            ));
        }
        if !plan_entry.chapters.is_empty() {
            let body = serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "chapters": plan_entry.chapters,
            }))
            .map_err(failed)?;
            let mapped = map_generated_artifact(
                TaskArtifactKind::Chapters,
                "Chapters",
                "json",
                "application/json",
                body,
            )?;
            artifacts.push(mapped.artifact);
            resources.push(mapped.resource);
            resource_bodies.push(mapped.body);
        }

        let summary = report.summary();
        let metadata_body = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "provider": "bilibili",
            "subject": {
                "kind": candidate.source_kind,
                "id": candidate.content_id,
                "index": candidate.index,
                "aid": candidate.identity.aid,
                "bvid": candidate.identity.bvid,
                "cid": candidate.identity.cid,
                "epid": candidate.identity.epid,
            },
            "title": candidate.title,
            "subtitle": candidate.subtitle,
            "download": {
                "file_count": summary.file_count,
                "media_file_count": summary.media_file_count,
                "sidecar_file_count": summary.sidecar_file_count,
                "mux_count": summary.mux_count,
                "total_bytes": summary.total_bytes,
            },
        }))
        .map_err(failed)?;
        let metadata = map_generated_artifact(
            TaskArtifactKind::Metadata,
            "Metadata",
            "json",
            "application/json",
            metadata_body,
        )?;
        artifacts.push(metadata.artifact);
        resources.push(metadata.resource);
        resource_bodies.push(metadata.body);

        let unpublished_output_paths = download_report_output_paths(report);
        let transient_output_paths = transient_download_output_paths(
            &unpublished_output_paths,
            library_output_path.as_deref(),
        );
        Ok(MappedV2DownloadResult {
            library_item_id: library_item_id.clone(),
            result: successful_download_result(
                result_id,
                candidate,
                library_item_id,
                artifacts,
                summary.total_bytes,
            ),
            resources,
            resource_bodies,
            library_item_lease,
            unpublished_output_paths,
            transient_output_paths,
        })
    }

    fn download_options(
        &self,
        options: Option<&BilibiliDownloadOptions>,
    ) -> Result<DownloadOptions, BilibiliDownloadError> {
        download_options_for_output_dir(self.output_dir.clone(), options)
    }

    fn client_for_request(
        &self,
        options: Option<&BilibiliDownloadOptions>,
        request_context: Option<&BilibiliRequestContext>,
    ) -> Result<BiliClient, BilibiliDownloadError> {
        if let Some(config) =
            bbdown_client_config_for_request(&self.options, options, request_context)?
        {
            return Ok(BiliClient::new(config));
        }
        Ok(if options.is_some_and(|options| options.prefer_tv_api) {
            self.tv_client.clone()
        } else {
            self.client.clone()
        })
    }

    #[allow(dead_code)]
    pub(crate) async fn resolve_playback_input(
        &self,
        source: &str,
        options: Option<&BilibiliDownloadOptions>,
        request_context: Option<&BilibiliRequestContext>,
        candidate_limit: usize,
        include_candidate_cover_uri: bool,
        is_cancel_requested: impl Fn() -> bool,
    ) -> Result<BilibiliInputResolution, BilibiliDownloadError> {
        let candidate_window = BilibiliResolveCandidateWindow::new(candidate_limit)?;
        let _preferences = playback_variant_preferences_from_options(options)?;
        let input = playback_input_for_planning(source)?;
        let selection = resolve_selection_for_input(&input, candidate_window)?;
        let client = self.client_for_request(options, request_context)?;
        let can_retry_bounded_resolve = selection.is_some();
        let resolved = match run_bbdown_core_until_cancelled(
            client.resolve(input.clone(), selection),
            &is_cancel_requested,
            "Cancelled while BBDown input resolution was running.",
        )
        .await?
        {
            Ok(resolved) => resolved,
            Err(error) if can_retry_bounded_resolve && should_retry_bounded_resolve(&error) => {
                self.retry_resolve_with_largest_bounded_prefix(
                    input.clone(),
                    error,
                    &client,
                    candidate_window,
                    &is_cancel_requested,
                )
                .await?
            }
            Err(error) => return Err(failed(error)),
        };
        BilibiliInputResolution::from_resolved_content(
            source.trim().to_owned(),
            &input,
            resolved,
            candidate_window.candidate_limit,
            include_candidate_cover_uri,
        )
    }

    async fn retry_resolve_with_largest_bounded_prefix(
        &self,
        input: Input,
        initial_error: BbdownError,
        client: &BiliClient,
        candidate_window: BilibiliResolveCandidateWindow,
        is_cancel_requested: &impl Fn() -> bool,
    ) -> Result<ResolvedContent, BilibiliDownloadError> {
        let mut last_error = initial_error;
        let mut search =
            BoundedPrefixSearch::after_failed_limit(candidate_window.truncation_probe_limit);
        let mut best_resolved = None;

        while let Some(limit) = search.next_limit() {
            let selection = bounded_resolve_selection(limit)?;
            match run_bbdown_core_until_cancelled(
                client.resolve(input.clone(), Some(selection)),
                is_cancel_requested,
                "Cancelled while BBDown input resolution was running.",
            )
            .await?
            {
                Ok(resolved) => {
                    search.record_success(limit);
                    best_resolved = Some(resolved);
                }
                Err(error) if should_retry_bounded_resolve(&error) => {
                    search.record_missing(limit);
                    last_error = error;
                }
                Err(error) => return Err(failed(error)),
            }
        }

        best_resolved.ok_or_else(|| failed(last_error))
    }

    #[allow(dead_code)]
    pub(crate) async fn plan_playback(
        &self,
        source: &str,
        selection_id: Option<&str>,
        options: Option<&BilibiliDownloadOptions>,
        request_context: Option<&BilibiliRequestContext>,
        policy: PlaybackPolicy,
        is_cancel_requested: impl Fn() -> bool,
    ) -> Result<BilibiliPlaybackPlan, BilibiliDownloadError> {
        let preferences = playback_variant_preferences_from_options_with_policy(options, policy)?;
        let input = playback_input_for_planning(source)?;
        let PlaybackInputSelection {
            input_override,
            selection,
            expected_identity,
        } = playback_selection_from_id(&input, selection_id)?;
        let direct_collection_item = input_override.is_some();
        let input = input_override.unwrap_or(input);
        let client = self.client_for_request(options, request_context)?;
        let selection = if direct_collection_item {
            Some(
                resolve_direct_collection_item_page(
                    &client,
                    &input,
                    expected_identity
                        .as_ref()
                        .expect("direct collection item should retain expected identity"),
                    &is_cancel_requested,
                )
                .await?,
            )
        } else {
            selection.or_else(|| default_selection_for_input(&input))
        };
        let plan = run_bbdown_until_cancelled(
            client.plan_playback_input(input, selection),
            is_cancel_requested,
            "Cancelled while BBDown playback planning was running.",
        )
        .await?;
        let plan = BilibiliPlaybackPlan::from_core_with_preferences(plan, &preferences)?;
        if let Some(expected_identity) = expected_identity.as_ref() {
            plan.validate_expected_identity(expected_identity)?;
        }
        Ok(plan)
    }
}

async fn resolve_direct_collection_item_page(
    client: &BiliClient,
    input: &Input,
    expected_identity: &PlaybackExpectedIdentity,
    is_cancel_requested: &impl Fn() -> bool,
) -> Result<Selection, BilibiliDownloadError> {
    let resolved = match run_bbdown_core_until_cancelled(
        client.resolve(input.clone(), None),
        is_cancel_requested,
        "Cancelled while BBDown collection item metadata resolution was running.",
    )
    .await?
    {
        Ok(resolved) => resolved,
        Err(error) => return Err(failed(error)),
    };
    direct_collection_item_page_selection(resolved, expected_identity)
}

fn direct_collection_item_page_selection(
    resolved: ResolvedContent,
    expected_identity: &PlaybackExpectedIdentity,
) -> Result<Selection, BilibiliDownloadError> {
    let ResolvedContent::Video(video) = resolved else {
        return Err(BilibiliDownloadError::Failed(
            "Selected Bilibili collection item did not resolve as a video.".to_owned(),
        ));
    };
    if expected_identity.aid.is_some_and(|aid| aid != video.aid)
        || matches!(
            (expected_identity.bvid.as_deref(), video.bvid.as_deref()),
            (Some(expected), Some(actual)) if !expected.eq_ignore_ascii_case(actual)
        )
    {
        return Err(BilibiliDownloadError::Failed(
            "Selected Bilibili item no longer matches the resolved candidate. Resolve the input again and retry."
                .to_owned(),
        ));
    }
    let expected_cid = expected_identity
        .cid
        .expect("validated collection identity should include cid");
    let page = video
        .pages
        .iter()
        .find(|page| page.cid == expected_cid)
        .ok_or_else(|| {
            BilibiliDownloadError::Failed(
                "Selected Bilibili item no longer matches the resolved candidate. Resolve the input again and retry."
                    .to_owned(),
            )
        })?;
    Ok(Selection::Page(page.index))
}

fn bbdown_client_config(
    options: &CacheServerOptions,
    playurl_mode: PlayurlMode,
) -> Result<ClientConfig, BilibiliDownloadError> {
    bbdown_client_config_with_profile(
        options,
        playurl_mode,
        options.bbdown_credential_profile.as_deref(),
    )
}

fn bbdown_client_config_for_request(
    server_options: &CacheServerOptions,
    options: Option<&BilibiliDownloadOptions>,
    request_context: Option<&BilibiliRequestContext>,
) -> Result<Option<ClientConfig>, BilibiliDownloadError> {
    let explicit_profile = request_context
        .map(|context| context.credential_profile_id.trim())
        .filter(|profile| !profile.is_empty());
    let explicit_mode = request_context
        .map(|context| BilibiliApiMode::try_from(context.api_mode))
        .transpose()
        .map_err(|_| {
            BilibiliDownloadError::Failed(
                "Bilibili API mode is unknown to this cache server.".to_owned(),
            )
        })?
        .filter(|mode| *mode != BilibiliApiMode::Unspecified);

    if request_context.is_none() && explicit_profile.is_none() && explicit_mode.is_none() {
        return Ok(None);
    }

    let playurl_mode = match explicit_mode {
        Some(BilibiliApiMode::Web) => PlayurlMode::Web,
        Some(BilibiliApiMode::Tv) => PlayurlMode::Tv,
        Some(BilibiliApiMode::App) => PlayurlMode::App,
        Some(BilibiliApiMode::Unspecified) => unreachable!("unspecified mode was filtered out"),
        None if options.is_some_and(|options| options.prefer_tv_api) => PlayurlMode::Tv,
        None => PlayurlMode::Web,
    };
    let credentials = match explicit_profile {
        Some(profile) => bbdown_credentials(
            server_options.bbdown_credential_path.as_deref(),
            Some(profile),
        )?,
        None if request_context.is_some() => Credentials::default(),
        None => bbdown_credentials(
            server_options.bbdown_credential_path.as_deref(),
            server_options.bbdown_credential_profile.as_deref(),
        )?,
    };
    Ok(Some(bbdown_client_config_with_credentials(
        server_options,
        playurl_mode,
        credentials,
    )))
}

fn bbdown_client_config_with_profile(
    options: &CacheServerOptions,
    playurl_mode: PlayurlMode,
    credential_profile: Option<&str>,
) -> Result<ClientConfig, BilibiliDownloadError> {
    let credentials = bbdown_credentials(
        options.bbdown_credential_path.as_deref(),
        credential_profile,
    )?;
    Ok(bbdown_client_config_with_credentials(
        options,
        playurl_mode,
        credentials,
    ))
}

fn bbdown_client_config_with_credentials(
    options: &CacheServerOptions,
    playurl_mode: PlayurlMode,
    credentials: Credentials,
) -> ClientConfig {
    ClientConfig::default()
        .with_credentials(credentials)
        .with_restricted_area(bbdown_restricted_area_config(options))
        .with_playurl_mode(playurl_mode)
}

fn bbdown_credentials(
    path: Option<&Path>,
    profile: Option<&str>,
) -> Result<Credentials, BilibiliDownloadError> {
    let Some(path) = path else {
        if profile.is_some() {
            return Err(BilibiliDownloadError::Failed(
                "A Bilibili credential profile was selected, but credential storage is not configured."
                    .to_owned(),
            ));
        }
        return Ok(Credentials::default());
    };
    let selection = match profile {
        Some(profile) => CredentialProfileSelection::named(profile).map_err(failed)?,
        None => CredentialProfileSelection::default_profile(),
    };
    let store = CredentialStore::new(path.to_path_buf());
    let Some(profile) = selection.profile_name() else {
        return store.load().map_err(failed);
    };
    let profiles = store.load_profiles().map_err(failed)?;
    if !profiles.profile_names().any(|name| name == profile) {
        return Err(failed(format!(
            "configured credential profile `{profile}` does not exist"
        )));
    }
    profiles.profile(profile).map_err(failed)
}

fn bbdown_restricted_area_config(options: &CacheServerOptions) -> RestrictedAreaConfig {
    RestrictedAreaConfig::new(
        options.bbdown_restricted_area.map(core_restricted_area),
        options
            .bbdown_restricted_area_proxies
            .iter()
            .map(core_playurl_proxy)
            .chain(
                options
                    .bbdown_restricted_api_proxies
                    .iter()
                    .map(core_api_proxy),
            ),
    )
}

fn core_playurl_proxy(proxy: &CacheBbdownRestrictedProxy) -> RestrictedAreaProxy {
    RestrictedAreaProxy::playurl(proxy.base_url.clone(), proxy.area.map(core_restricted_area))
}

fn core_api_proxy(proxy: &CacheBbdownRestrictedProxy) -> RestrictedAreaProxy {
    RestrictedAreaProxy::bilibili_api(proxy.base_url.clone(), proxy.area.map(core_restricted_area))
}

fn core_restricted_area(area: CacheBbdownRestrictedArea) -> RestrictedArea {
    match area {
        CacheBbdownRestrictedArea::Cn => RestrictedArea::Cn,
        CacheBbdownRestrictedArea::Th => RestrictedArea::Th,
        CacheBbdownRestrictedArea::Hk => RestrictedArea::Hk,
        CacheBbdownRestrictedArea::Tw => RestrictedArea::Tw,
    }
}

impl BilibiliDownloadAdapter for BbdownBilibiliAdapter {
    fn run<'a>(
        &'a self,
        request: BilibiliDownloadRequest,
        context: BilibiliDownloadContext,
    ) -> BilibiliDownloadFuture<'a> {
        Box::pin(async move { self.run_inner(request, context).await })
    }
}

async fn run_bbdown_until_cancelled<T, E>(
    future: impl Future<Output = Result<T, E>>,
    is_cancel_requested: impl Fn() -> bool,
    cancellation_message: &'static str,
) -> Result<T, BilibiliDownloadError>
where
    E: Display,
{
    tokio::pin!(future);
    loop {
        if is_cancel_requested() {
            return Err(BilibiliDownloadError::Cancelled(
                cancellation_message.to_owned(),
            ));
        }

        tokio::select! {
            result = &mut future => return result.map_err(failed),
            () = sleep(Duration::from_millis(100)) => {}
        }
    }
}

async fn run_bbdown_download_until_cancelled<T>(
    future: impl Future<Output = Result<T, BbdownError>>,
    cancellation: &DownloadCancellationToken,
    is_cancel_requested: impl Fn() -> bool,
    cancellation_message: &'static str,
) -> Result<T, BilibiliDownloadError> {
    run_bbdown_download_until_cancelled_with_grace(
        future,
        cancellation,
        is_cancel_requested,
        cancellation_message,
        BBDOWN_CANCELLATION_GRACE_PERIOD,
    )
    .await
}

async fn run_bbdown_download_until_cancelled_with_grace<T>(
    future: impl Future<Output = Result<T, BbdownError>>,
    cancellation: &DownloadCancellationToken,
    is_cancel_requested: impl Fn() -> bool,
    cancellation_message: &'static str,
    cancellation_grace_period: Duration,
) -> Result<T, BilibiliDownloadError> {
    tokio::pin!(future);
    let mut cancellation_started_at: Option<Instant> = None;
    loop {
        if is_cancel_requested() && !cancellation.is_cancelled() {
            cancellation.cancel_with_reason(cancellation_message);
        }
        if cancellation.is_cancelled() && cancellation_started_at.is_none() {
            cancellation_started_at = Some(Instant::now());
        }
        if cancellation_started_at
            .is_some_and(|started_at| started_at.elapsed() >= cancellation_grace_period)
        {
            return Err(BilibiliDownloadError::Cancelled(cancellation_reason(
                cancellation,
                cancellation_message,
            )));
        }

        let poll_interval = cancellation_started_at
            .map(|started_at| {
                cancellation_grace_period
                    .saturating_sub(started_at.elapsed())
                    .min(BBDOWN_CANCELLATION_POLL_INTERVAL)
            })
            .unwrap_or(BBDOWN_CANCELLATION_POLL_INTERVAL);

        tokio::select! {
            result = &mut future => {
                return match result {
                    Ok(value) => Ok(value),
                    Err(error) => {
                        if is_cancel_requested() && !cancellation.is_cancelled() {
                            cancellation.cancel_with_reason(cancellation_message);
                        }
                        if error.is_cancelled() || cancellation.is_cancelled() {
                            Err(BilibiliDownloadError::Cancelled(
                                cancellation_reason(cancellation, &error.to_string()),
                            ))
                        } else {
                            Err(failed(error))
                        }
                    }
                };
            }
            () = sleep(poll_interval) => {}
        }
    }
}

fn cancellation_reason(cancellation: &DownloadCancellationToken, fallback: &str) -> String {
    cancellation.reason().unwrap_or_else(|| fallback.to_owned())
}

async fn run_bbdown_core_until_cancelled<T>(
    future: impl Future<Output = Result<T, BbdownError>>,
    is_cancel_requested: &impl Fn() -> bool,
    cancellation_message: &'static str,
) -> Result<Result<T, BbdownError>, BilibiliDownloadError> {
    tokio::pin!(future);
    loop {
        if is_cancel_requested() {
            return Err(BilibiliDownloadError::Cancelled(
                cancellation_message.to_owned(),
            ));
        }

        tokio::select! {
            result = &mut future => return Ok(result),
            () = sleep(Duration::from_millis(100)) => {}
        }
    }
}

fn failed(error: impl Display) -> BilibiliDownloadError {
    BilibiliDownloadError::Failed(format!("BBDown adapter failed: {error}"))
}

fn progress(progress: f64, message: impl Into<String>) -> BilibiliTaskProgress {
    BilibiliTaskProgress {
        progress: Some(progress),
        message: Some(message.into()),
        ..BilibiliTaskProgress::default()
    }
}

struct BilibiliBbdownProgressSink {
    context: BilibiliDownloadContext,
    accumulator: StdMutex<BilibiliBbdownProgressAccumulator>,
    v2_window: Option<BilibiliV2ProgressWindow>,
}

impl BilibiliBbdownProgressSink {
    fn new(context: BilibiliDownloadContext) -> Self {
        Self {
            context,
            accumulator: StdMutex::new(BilibiliBbdownProgressAccumulator::default()),
            v2_window: None,
        }
    }

    fn for_v2_candidate(
        context: BilibiliDownloadContext,
        offset: usize,
        total: usize,
        completed_downloaded_bytes: u64,
        total_bytes_floor: u64,
    ) -> Self {
        Self {
            context,
            accumulator: StdMutex::new(BilibiliBbdownProgressAccumulator::default()),
            v2_window: Some(BilibiliV2ProgressWindow {
                offset,
                total,
                completed_downloaded_bytes,
                total_bytes_floor,
            }),
        }
    }

    fn v2_progress_snapshot(&self) -> BilibiliV2ProgressSnapshot {
        let Some(window) = self.v2_window else {
            return BilibiliV2ProgressSnapshot::default();
        };
        let accumulator = self
            .accumulator
            .lock()
            .expect("BBDown progress accumulator lock poisoned");
        let (downloaded_bytes, total_bytes) = accumulator.known_bytes_snapshot();
        BilibiliV2ProgressSnapshot {
            downloaded_bytes: window
                .completed_downloaded_bytes
                .saturating_add(downloaded_bytes),
            total_bytes: window.total_bytes_floor.max(
                window
                    .completed_downloaded_bytes
                    .saturating_add(total_bytes.unwrap_or(downloaded_bytes)),
            ),
        }
    }
}

impl DownloadProgressSink for BilibiliBbdownProgressSink {
    fn on_download_progress(&self, event: &DownloadProgressEvent) {
        let progress = self
            .accumulator
            .lock()
            .ok()
            .and_then(|mut accumulator| accumulator.record(event));
        if let Some(mut progress) = progress {
            if let Some(window) = self.v2_window {
                window.map(&mut progress);
            }
            self.context.report_progress(progress);
        }
    }
}

#[derive(Clone, Copy)]
struct BilibiliV2ProgressWindow {
    offset: usize,
    total: usize,
    completed_downloaded_bytes: u64,
    total_bytes_floor: u64,
}

impl BilibiliV2ProgressWindow {
    fn map(self, progress: &mut BilibiliTaskProgress) {
        if let Some(fraction) = progress.progress.as_mut() {
            *fraction = v2_candidate_progress(self.offset, self.total, *fraction);
        }
        if let Some(downloaded_bytes) = progress.downloaded_bytes.as_mut() {
            *downloaded_bytes = to_i64_saturating(
                self.completed_downloaded_bytes
                    .saturating_add(nonnegative_i64_to_u64(*downloaded_bytes)),
            );
        }
        if let Some(total_bytes) = progress.total_bytes.as_mut() {
            *total_bytes = to_i64_saturating(
                self.total_bytes_floor.max(
                    self.completed_downloaded_bytes
                        .saturating_add(nonnegative_i64_to_u64(*total_bytes)),
                ),
            );
        }
    }
}

#[derive(Clone, Copy, Default)]
struct BilibiliV2ProgressSnapshot {
    downloaded_bytes: u64,
    total_bytes: u64,
}

#[derive(Default)]
struct BilibiliBbdownProgressAccumulator {
    entry_count: Option<usize>,
    completed_entries: usize,
    completed_entry_indices: HashSet<u32>,
    files: HashMap<PathBuf, BilibiliBbdownFileProgress>,
    active_entry_files: HashMap<PathBuf, BilibiliBbdownFileProgress>,
    active_entry_in_progress: bool,
    last_progress: f64,
    last_published_downloaded_bytes: u64,
    last_published_progress: f64,
}

#[derive(Default)]
struct BilibiliBbdownFileProgress {
    resumed_from: u64,
    bytes_written: u64,
    expected_size: Option<u64>,
}

impl BilibiliBbdownFileProgress {
    fn downloaded_bytes(&self) -> u64 {
        self.resumed_from.saturating_add(self.bytes_written)
    }

    fn total_bytes(&self) -> Option<u64> {
        self.expected_size
            .map(|expected_size| expected_size.max(self.downloaded_bytes()))
    }
}

impl BilibiliBbdownProgressAccumulator {
    fn record(&mut self, event: &DownloadProgressEvent) -> Option<BilibiliTaskProgress> {
        match event {
            DownloadProgressEvent::PlanStarted { entry_count, .. } => {
                self.entry_count = Some((*entry_count).max(1));
                self.completed_entries = 0;
                self.completed_entry_indices.clear();
                self.files.clear();
                self.active_entry_files.clear();
                self.active_entry_in_progress = false;
                self.message_progress(
                    DOWNLOAD_PROGRESS_START,
                    format!("Downloading {entry_count} Bilibili entry(s)."),
                )
            }
            DownloadProgressEvent::EntryStarted { index, title, .. } => {
                self.active_entry_files.clear();
                self.active_entry_in_progress = true;
                let progress = self.current_download_progress();
                self.message_progress(
                    progress,
                    format!("Downloading Bilibili entry {index}: {title}."),
                )
            }
            DownloadProgressEvent::FileStarted {
                entry_title,
                kind,
                path,
                resumed_from,
                expected_size,
                ..
            } => {
                self.active_entry_in_progress = true;
                self.files.insert(
                    path.clone(),
                    BilibiliBbdownFileProgress {
                        resumed_from: *resumed_from,
                        bytes_written: 0,
                        expected_size: *expected_size,
                    },
                );
                self.active_entry_files.insert(
                    path.clone(),
                    BilibiliBbdownFileProgress {
                        resumed_from: *resumed_from,
                        bytes_written: 0,
                        expected_size: *expected_size,
                    },
                );
                Some(self.published_bytes_progress(format!(
                    "Downloading {} for {entry_title}.",
                    download_file_kind_label(kind),
                )))
            }
            DownloadProgressEvent::FileProgress {
                entry_title,
                kind,
                path,
                bytes_written,
                resumed_from,
                expected_size,
                ..
            } => {
                self.active_entry_in_progress = true;
                let file = self.files.entry(path.clone()).or_default();
                file.resumed_from = *resumed_from;
                file.bytes_written = *bytes_written;
                file.expected_size = *expected_size;
                let active_file = self.active_entry_files.entry(path.clone()).or_default();
                active_file.resumed_from = *resumed_from;
                active_file.bytes_written = *bytes_written;
                active_file.expected_size = *expected_size;
                self.throttled_bytes_progress(format!(
                    "Downloading {} for {entry_title}.",
                    download_file_kind_label(kind),
                ))
            }
            DownloadProgressEvent::FileCompleted {
                entry_title,
                kind,
                path,
                bytes_written,
                resumed_from,
                total_bytes,
                ..
            } => {
                self.active_entry_in_progress = true;
                self.files.insert(
                    path.clone(),
                    BilibiliBbdownFileProgress {
                        resumed_from: *resumed_from,
                        bytes_written: *bytes_written,
                        expected_size: Some(*total_bytes),
                    },
                );
                self.active_entry_files.insert(
                    path.clone(),
                    BilibiliBbdownFileProgress {
                        resumed_from: *resumed_from,
                        bytes_written: *bytes_written,
                        expected_size: Some(*total_bytes),
                    },
                );
                Some(self.published_bytes_progress(format!(
                    "Downloaded {} for {entry_title}.",
                    download_file_kind_label(kind),
                )))
            }
            DownloadProgressEvent::FileFailed {
                entry_title,
                kind,
                path,
                ..
            } => {
                self.active_entry_in_progress = true;
                self.rollback_file_progress(path);
                Some(self.published_rollback_bytes_progress(format!(
                    "Retrying {} for {entry_title} after a BBDown error.",
                    download_file_kind_label(kind),
                )))
            }
            DownloadProgressEvent::EntryCompleted { index, title, .. } => {
                if self.completed_entry_indices.insert(*index) {
                    self.completed_entries = self
                        .completed_entries
                        .saturating_add(1)
                        .min(self.entry_count.unwrap_or(usize::MAX));
                }
                self.active_entry_files.clear();
                self.active_entry_in_progress = false;
                Some(self.published_bytes_progress(format!(
                    "Downloaded Bilibili entry {index}: {title}."
                )))
            }
            DownloadProgressEvent::PlanCompleted { entry_count, .. } => {
                self.completed_entries = *entry_count;
                self.active_entry_in_progress = false;
                self.message_progress(
                    DOWNLOAD_PROGRESS_END,
                    format!("Downloaded {entry_count} Bilibili entry(s)."),
                )
            }
            DownloadProgressEvent::PlanCancelled { .. } => {
                self.active_entry_in_progress = false;
                let progress = self.current_download_progress();
                self.message_progress(progress, "BBDown download cancelled.".to_owned())
            }
            DownloadProgressEvent::PlanFailed { .. } => {
                self.active_entry_in_progress = false;
                let progress = self.current_download_progress();
                self.message_progress(progress, "BBDown download failed.".to_owned())
            }
            DownloadProgressEvent::MuxStarted { .. }
            | DownloadProgressEvent::MuxCompleted { .. }
            | DownloadProgressEvent::MuxFailed { .. }
            | DownloadProgressEvent::EntryFailed { .. } => None,
            _ => None,
        }
    }

    fn published_bytes_progress(&mut self, message: String) -> BilibiliTaskProgress {
        let progress = self.current_download_progress();
        let (tracked_downloaded_bytes, _) = self.known_bytes_snapshot();
        let (downloaded_bytes, total_bytes) = self.reported_bytes_snapshot();
        self.mark_published(tracked_downloaded_bytes, progress);
        self.bytes_progress(downloaded_bytes, total_bytes, progress, message)
    }

    fn published_rollback_bytes_progress(&mut self, message: String) -> BilibiliTaskProgress {
        let progress = self.current_download_progress_allowing_rollback();
        let (tracked_downloaded_bytes, _) = self.known_bytes_snapshot();
        let (downloaded_bytes, total_bytes) = self.reported_bytes_snapshot();
        self.mark_published(tracked_downloaded_bytes, progress);
        self.bytes_progress(downloaded_bytes, total_bytes, progress, message)
    }

    fn throttled_bytes_progress(&mut self, message: String) -> Option<BilibiliTaskProgress> {
        let progress = self.current_download_progress();
        let (tracked_downloaded_bytes, _) = self.known_bytes_snapshot();
        let (downloaded_bytes, total_bytes) = self.reported_bytes_snapshot();
        if !self.should_publish_file_progress(tracked_downloaded_bytes, progress) {
            return None;
        }
        self.mark_published(tracked_downloaded_bytes, progress);
        Some(self.bytes_progress(downloaded_bytes, total_bytes, progress, message))
    }

    fn bytes_progress(
        &self,
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
        progress: f64,
        message: String,
    ) -> BilibiliTaskProgress {
        BilibiliTaskProgress {
            progress: Some(progress),
            downloaded_bytes: Some(to_i64_saturating(downloaded_bytes)),
            total_bytes: total_bytes.map(to_i64_saturating),
            message: Some(message),
        }
    }

    fn message_progress(&mut self, progress: f64, message: String) -> Option<BilibiliTaskProgress> {
        self.last_progress = self.last_progress.max(progress.clamp(0.0, 1.0));
        self.last_published_progress = self.last_progress;
        Some(BilibiliTaskProgress {
            progress: Some(self.last_progress),
            message: Some(message),
            ..BilibiliTaskProgress::default()
        })
    }

    fn current_download_progress(&mut self) -> f64 {
        let progress = self.download_progress_from_current_state();
        self.last_progress = self
            .last_progress
            .max(progress.clamp(0.0, DOWNLOAD_PROGRESS_END));
        self.last_progress
    }

    fn current_download_progress_allowing_rollback(&mut self) -> f64 {
        self.last_progress = self
            .download_progress_from_current_state()
            .clamp(0.0, DOWNLOAD_PROGRESS_END);
        self.last_progress
    }

    fn download_progress_from_current_state(&self) -> f64 {
        let entry_count = self.entry_count.unwrap_or(1).max(1);
        let completed_entries = self.completed_entries.min(entry_count);
        let active_entry_ratio = if completed_entries < entry_count {
            // BBDown reports files as they start, so a DASH/FLV entry can have unstarted files
            // that are not yet represented in byte totals. Keep active entry progress conservative
            // until EntryCompleted confirms the whole entry is finished.
            self.known_bytes_ratio()
                .min(ACTIVE_ENTRY_INCOMPLETE_PROGRESS_CAP)
        } else {
            0.0
        };
        let download_ratio =
            ((completed_entries as f64) + active_entry_ratio) / (entry_count as f64);
        DOWNLOAD_PROGRESS_START
            + ((DOWNLOAD_PROGRESS_END - DOWNLOAD_PROGRESS_START) * download_ratio.clamp(0.0, 1.0))
    }

    fn known_bytes_ratio(&self) -> f64 {
        let (downloaded_bytes, total_bytes) =
            self.known_bytes_snapshot_for(&self.active_entry_files);
        let Some(total_bytes) = total_bytes else {
            return 0.0;
        };
        if total_bytes == 0 {
            return 0.0;
        }
        (downloaded_bytes as f64 / total_bytes as f64).clamp(0.0, 1.0)
    }

    fn known_bytes_snapshot(&self) -> (u64, Option<u64>) {
        self.known_bytes_snapshot_for(&self.files)
    }

    fn completed_entries_bytes_snapshot(&self) -> (u64, Option<u64>) {
        let mut downloaded_bytes = 0_u64;
        let mut total_bytes = 0_u64;
        let mut any_file = false;
        let mut all_totals_known = true;
        for (path, file) in &self.files {
            if self.active_entry_files.contains_key(path) {
                continue;
            }
            any_file = true;
            downloaded_bytes = downloaded_bytes.saturating_add(file.downloaded_bytes());
            if let Some(file_total_bytes) = file.total_bytes() {
                total_bytes = total_bytes.saturating_add(file_total_bytes);
            } else {
                all_totals_known = false;
            }
        }
        (
            downloaded_bytes,
            (any_file && all_totals_known).then_some(total_bytes.max(downloaded_bytes)),
        )
    }

    fn known_bytes_snapshot_for(
        &self,
        files: &HashMap<PathBuf, BilibiliBbdownFileProgress>,
    ) -> (u64, Option<u64>) {
        let mut downloaded_bytes = 0_u64;
        let mut total_bytes = 0_u64;
        let mut all_totals_known = !files.is_empty();
        for file in files.values() {
            downloaded_bytes = downloaded_bytes.saturating_add(file.downloaded_bytes());
            if let Some(file_total_bytes) = file.total_bytes() {
                total_bytes = total_bytes.saturating_add(file_total_bytes);
            } else {
                all_totals_known = false;
            }
        }
        (
            downloaded_bytes,
            all_totals_known.then_some(total_bytes.max(downloaded_bytes)),
        )
    }

    fn reported_bytes_snapshot(&self) -> (u64, Option<u64>) {
        let (downloaded_bytes, known_total_bytes) = if self.active_entry_in_progress {
            self.completed_entries_bytes_snapshot()
        } else {
            self.known_bytes_snapshot()
        };
        if let Some(known_total_bytes) = known_total_bytes {
            return (downloaded_bytes, Some(known_total_bytes));
        }
        (0, Some(0))
    }

    fn should_publish_file_progress(&self, downloaded_bytes: u64, progress: f64) -> bool {
        downloaded_bytes == 0
            || downloaded_bytes.saturating_sub(self.last_published_downloaded_bytes)
                >= DOWNLOAD_PROGRESS_PUBLISH_MIN_BYTES
            || (progress - self.last_published_progress) >= DOWNLOAD_PROGRESS_PUBLISH_MIN_FRACTION
    }

    fn mark_published(&mut self, downloaded_bytes: u64, progress: f64) {
        self.last_published_downloaded_bytes = downloaded_bytes;
        self.last_published_progress = progress;
    }

    fn rollback_file_progress(&mut self, path: &Path) {
        if let Some(file) = self.files.get_mut(path) {
            file.bytes_written = 0;
        }
        if let Some(file) = self.active_entry_files.get_mut(path) {
            file.bytes_written = 0;
        }
    }
}

fn download_file_kind_label(kind: &DownloadFileKind) -> &'static str {
    match kind {
        DownloadFileKind::Video => "video",
        DownloadFileKind::Audio => "audio",
        DownloadFileKind::FlvSegment => "FLV segment",
        DownloadFileKind::Cover => "cover",
        DownloadFileKind::Subtitle => "subtitle",
        DownloadFileKind::Danmaku => "danmaku",
        DownloadFileKind::DanmakuAss => "danmaku ASS",
        _ => "download file",
    }
}

fn default_selection_for_input(input: &Input) -> Option<Selection> {
    match input {
        Input::Season(_)
        | Input::Media(_)
        | Input::CheeseSeason(_)
        | Input::SpaceVideos(_)
        | Input::FavoriteList { .. }
        | Input::CollectionList(_)
        | Input::SeriesList(_)
        | Input::SpaceCollectionList { .. }
        | Input::SpaceSeriesList { .. }
        | Input::RecommendationFeed
        | Input::FollowingFeed
        | Input::SpaceDynamic(_)
        | Input::History
        | Input::WatchLater => Some(Selection::Latest),
        Input::Aid(_)
        | Input::Bvid(_)
        | Input::Episode(_)
        | Input::CheeseEpisode(_)
        | Input::IntlEpisode(_) => Some(Selection::Current),
        Input::ShortLink(_) => None,
    }
}

fn bounded_resolve_selection(limit: u32) -> Result<Selection, BilibiliDownloadError> {
    IndexSelection::range(1, limit)
        .map(Selection::Indices)
        .map_err(failed)
}

fn resolve_selection_for_input(
    input: &Input,
    candidate_window: BilibiliResolveCandidateWindow,
) -> Result<Option<Selection>, BilibiliDownloadError> {
    match input {
        Input::Aid(_) | Input::Bvid(_) => Ok(None),
        Input::Episode(_) | Input::CheeseEpisode(_) | Input::IntlEpisode(_) => {
            Ok(Some(Selection::Current))
        }
        Input::Season(_) | Input::Media(_) | Input::CheeseSeason(_) => Ok(Some(Selection::Page(1))),
        Input::SpaceVideos(_)
        | Input::FavoriteList { .. }
        | Input::CollectionList(_)
        | Input::SeriesList(_)
        | Input::SpaceCollectionList { .. }
        | Input::SpaceSeriesList { .. }
        | Input::RecommendationFeed
        | Input::FollowingFeed
        | Input::SpaceDynamic(_)
        | Input::History
        | Input::WatchLater => {
            bounded_resolve_selection(candidate_window.truncation_probe_limit).map(Some)
        }
        Input::ShortLink(_) => Ok(None),
    }
}

fn should_retry_bounded_resolve(error: &BbdownError) -> bool {
    matches!(
        error,
        BbdownError::MissingField(
            "selected page" | "selected episode" | "selected collection item"
        )
    )
}

#[derive(Debug)]
struct BoundedPrefixSearch {
    next_low: u32,
    next_high: u32,
    best_success: Option<u32>,
}

impl BoundedPrefixSearch {
    fn after_failed_limit(failed_limit: u32) -> Self {
        Self {
            next_low: 1,
            next_high: failed_limit.saturating_sub(1),
            best_success: None,
        }
    }

    fn next_limit(&self) -> Option<u32> {
        if self.next_low > self.next_high {
            return None;
        }
        Some(self.next_low + (self.next_high - self.next_low).div_ceil(2))
    }

    fn record_success(&mut self, limit: u32) {
        self.best_success = Some(limit);
        self.next_low = limit.saturating_add(1);
    }

    fn record_missing(&mut self, limit: u32) {
        self.next_high = limit.saturating_sub(1);
    }

    #[cfg(test)]
    fn best_success(&self) -> Option<u32> {
        self.best_success
    }
}

fn playback_input_for_planning(source: &str) -> Result<Input, BilibiliDownloadError> {
    let input = Input::parse(source).map_err(failed)?;
    if matches!(input, Input::ShortLink(_)) {
        return Err(BilibiliDownloadError::Failed(
            "BBDown playback planning does not support short links yet; expand the b23.tv URL before submitting it.".to_owned(),
        ));
    }
    Ok(input)
}

#[derive(Debug, Default, Eq, PartialEq)]
struct PlaybackInputSelection {
    input_override: Option<Input>,
    selection: Option<Selection>,
    expected_identity: Option<PlaybackExpectedIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PlaybackExpectedIdentity {
    bvid: Option<String>,
    aid: Option<u64>,
    cid: Option<u64>,
}

impl PlaybackExpectedIdentity {
    fn is_valid_page_identity(&self) -> bool {
        self.cid.is_some() && (self.bvid.is_some() || self.aid.is_some())
    }

    fn is_valid_collection_item_identity(&self) -> bool {
        self.is_valid_page_identity()
    }

    fn direct_video_input(&self) -> Option<Input> {
        self.bvid
            .as_ref()
            .map(|bvid| Input::Bvid(bvid.clone()))
            .or_else(|| self.aid.map(Input::Aid))
    }

    fn same_collection_item(&self, other: &Self) -> bool {
        self.aid == other.aid
            && self.cid == other.cid
            && match (self.bvid.as_deref(), other.bvid.as_deref()) {
                (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
                _ => true,
            }
    }

    fn matches(&self, entry: &BilibiliPlaybackEntry) -> bool {
        if let Some(expected_bvid) = self.bvid.as_deref()
            && let Some(actual_bvid) = entry.bvid.as_deref()
            && !actual_bvid.eq_ignore_ascii_case(expected_bvid)
        {
            return false;
        }

        if let Some(expected_aid) = self.aid
            && entry.aid != expected_aid
        {
            return false;
        }

        if let Some(expected_cid) = self.cid
            && entry.cid != expected_cid
        {
            return false;
        }

        self.cid.is_some() || self.aid.is_some() || (self.bvid.is_some() && entry.bvid.is_some())
    }
}

fn playback_selection_from_id(
    input: &Input,
    selection_id: Option<&str>,
) -> Result<PlaybackInputSelection, BilibiliDownloadError> {
    let Some(selection_id) = selection_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(PlaybackInputSelection::default());
    };

    if let Some(page) = selection_id.strip_prefix("page:") {
        if !playback_input_accepts_page_selection(input) {
            return Err(invalid_selection_id(selection_id));
        }
        return playback_page_selection_from_id(page, selection_id);
    }
    if let Some(item) = selection_id.strip_prefix("item:") {
        if !playback_input_accepts_collection_item_selection(input) {
            return Err(invalid_selection_id(selection_id));
        }
        return playback_collection_item_selection_from_id(input, item, selection_id);
    }
    if let Some(episode) = selection_id.strip_prefix("episode:") {
        if !playback_input_accepts_episode_selection(input) {
            return Err(invalid_selection_id(selection_id));
        }
        return parse_selection_u64(episode, selection_id).map(|episode| PlaybackInputSelection {
            input_override: None,
            selection: Some(Selection::Episode(episode)),
            expected_identity: None,
        });
    }

    Err(invalid_selection_id(selection_id))
}

fn playback_input_accepts_page_selection(input: &Input) -> bool {
    matches!(input, Input::Aid(_) | Input::Bvid(_))
}

fn playback_input_accepts_episode_selection(input: &Input) -> bool {
    matches!(
        input,
        Input::Episode(_)
            | Input::Season(_)
            | Input::Media(_)
            | Input::CheeseEpisode(_)
            | Input::CheeseSeason(_)
            | Input::IntlEpisode(_)
    )
}

fn playback_input_accepts_collection_item_selection(input: &Input) -> bool {
    matches!(
        input,
        Input::SpaceVideos(_)
            | Input::FavoriteList { .. }
            | Input::CollectionList(_)
            | Input::SeriesList(_)
            | Input::SpaceCollectionList { .. }
            | Input::SpaceSeriesList { .. }
            | Input::RecommendationFeed
            | Input::FollowingFeed
            | Input::SpaceDynamic(_)
            | Input::History
            | Input::WatchLater
    )
}

fn playback_page_selection_from_id(
    page: &str,
    selection_id: &str,
) -> Result<PlaybackInputSelection, BilibiliDownloadError> {
    let mut parts = page.split(':');
    let index_text = parts
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_selection_id(selection_id))?;
    let index = parse_selection_index(index_text, selection_id)?;
    let parsed = playback_selection_parts_from_parts(parts, selection_id)?;
    if parsed.source_token.is_some() {
        return Err(invalid_selection_id(selection_id));
    }
    let expected_identity = parsed.expected_identity;
    if !expected_identity
        .as_ref()
        .is_some_and(PlaybackExpectedIdentity::is_valid_page_identity)
    {
        return Err(invalid_selection_id(selection_id));
    }

    Ok(PlaybackInputSelection {
        input_override: None,
        selection: Some(Selection::Page(index)),
        expected_identity,
    })
}

fn playback_collection_item_selection_from_id(
    input: &Input,
    item: &str,
    selection_id: &str,
) -> Result<PlaybackInputSelection, BilibiliDownloadError> {
    let parsed = parse_collection_item_selection(item, selection_id)?;
    let expected_source_token =
        collection_source_token(input).ok_or_else(|| invalid_selection_id(selection_id))?;
    if parsed.source_token.as_deref() != Some(expected_source_token.as_str()) {
        return Err(invalid_selection_id(selection_id));
    }

    Ok(PlaybackInputSelection {
        input_override: parsed.expected_identity.direct_video_input(),
        selection: None,
        expected_identity: Some(parsed.expected_identity),
    })
}

#[derive(Debug, Eq, PartialEq)]
struct ParsedCollectionItemSelection {
    index: u32,
    source_token: Option<String>,
    expected_identity: PlaybackExpectedIdentity,
}

fn parse_collection_item_selection(
    item: &str,
    selection_id: &str,
) -> Result<ParsedCollectionItemSelection, BilibiliDownloadError> {
    let mut parts = item.split(':');
    let index_text = parts
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_selection_id(selection_id))?;
    let index = parse_selection_index(index_text, selection_id)?;
    let parsed = playback_selection_parts_from_parts(parts, selection_id)?;
    let expected_identity = parsed.expected_identity;
    if !expected_identity
        .as_ref()
        .is_some_and(PlaybackExpectedIdentity::is_valid_collection_item_identity)
    {
        return Err(invalid_selection_id(selection_id));
    }
    Ok(ParsedCollectionItemSelection {
        index,
        source_token: parsed.source_token,
        expected_identity: expected_identity.expect("validated collection identity should exist"),
    })
}

pub(crate) fn recover_stable_collection_candidate(
    selection_id: &str,
    source: &str,
    source_kind: &str,
    current_candidates: &[BilibiliResolvedCandidate],
) -> Option<BilibiliResolvedCandidate> {
    let item = selection_id.strip_prefix("item:")?;
    let parsed = parse_collection_item_selection(item, selection_id).ok()?;
    let input = playback_input_for_planning(source).ok()?;
    let expected_source_token = collection_source_token(&input)?;
    if parsed.source_token.as_deref() != Some(expected_source_token.as_str()) {
        return None;
    }
    let mut candidate = current_candidates
        .iter()
        .find(|candidate| {
            candidate
                .selection_id
                .strip_prefix("item:")
                .and_then(|item| {
                    parse_collection_item_selection(item, &candidate.selection_id).ok()
                })
                .is_some_and(|current| {
                    current.source_token.as_deref() == Some(expected_source_token.as_str())
                        && parsed
                            .expected_identity
                            .same_collection_item(&current.expected_identity)
                })
        })
        .cloned()
        .unwrap_or_else(|| {
            let content_id = parsed.expected_identity.bvid.clone().unwrap_or_else(|| {
                format!(
                    "av{}",
                    parsed
                        .expected_identity
                        .aid
                        .expect("validated collection identity should include aid without bvid")
                )
            });
            BilibiliResolvedCandidate {
                selection_id: selection_id.to_owned(),
                title: content_id.clone(),
                subtitle: "Resolved Bilibili collection item".to_owned(),
                source_kind: source_kind.to_owned(),
                content_id,
                identity: BilibiliContentIdentity {
                    kind: BilibiliContentKind::CollectionItem,
                    aid: parsed.expected_identity.aid,
                    bvid: parsed.expected_identity.bvid.clone(),
                    cid: parsed.expected_identity.cid,
                    epid: None,
                },
                index: parsed.index,
                duration_seconds: None,
                cover_uri: String::new(),
            }
        });
    candidate.selection_id = selection_id.to_owned();
    candidate.index = parsed.index;
    Some(candidate)
}

struct ParsedSelectionParts {
    source_token: Option<String>,
    expected_identity: Option<PlaybackExpectedIdentity>,
}

fn playback_selection_parts_from_parts<'a>(
    mut parts: impl Iterator<Item = &'a str>,
    selection_id: &str,
) -> Result<ParsedSelectionParts, BilibiliDownloadError> {
    let mut source_token = None;
    let mut bvid = None;
    let mut aid = None;
    let mut cid = None;
    while let Some(kind) = parts.next() {
        let Some(value) = parts.next() else {
            return Err(invalid_selection_id(selection_id));
        };
        match kind {
            "source" if source_token.is_none() && !value.trim().is_empty() => {
                source_token = Some(value.trim().to_owned());
            }
            "bvid" if bvid.is_none() && !value.trim().is_empty() => {
                bvid = Some(value.trim().to_owned());
            }
            "aid" if aid.is_none() => {
                aid = Some(parse_selection_u64(value, selection_id)?);
            }
            "cid" if cid.is_none() => {
                cid = Some(parse_selection_u64(value, selection_id)?);
            }
            _ => return Err(invalid_selection_id(selection_id)),
        }
    }

    if bvid.is_none() && aid.is_none() && cid.is_none() {
        return Ok(ParsedSelectionParts {
            source_token,
            expected_identity: None,
        });
    }
    Ok(ParsedSelectionParts {
        source_token,
        expected_identity: Some(PlaybackExpectedIdentity { bvid, aid, cid }),
    })
}

fn parse_selection_index(text: &str, selection_id: &str) -> Result<u32, BilibiliDownloadError> {
    let index = parse_selection_u32(text, selection_id)?;
    let maximum_index = u32::try_from(MAX_BILIBILI_RESOLVE_CANDIDATE_LIMIT)
        .map_err(|_| invalid_selection_id(selection_id))?;
    if index == 0 || index > maximum_index {
        return Err(invalid_selection_id(selection_id));
    }
    Ok(index)
}

fn parse_selection_u32(text: &str, selection_id: &str) -> Result<u32, BilibiliDownloadError> {
    text.parse::<u32>()
        .map_err(|_| invalid_selection_id(selection_id))
}

fn parse_selection_u64(text: &str, selection_id: &str) -> Result<u64, BilibiliDownloadError> {
    text.parse::<u64>()
        .map_err(|_| invalid_selection_id(selection_id))
}

fn invalid_selection_id(selection_id: &str) -> BilibiliDownloadError {
    BilibiliDownloadError::Failed(format!("Invalid selection_id: {selection_id}"))
}

const MAX_U32_DECIMAL_BYTES: usize = 10;
const MAX_U64_DECIMAL_BYTES: usize = 20;
const COLLECTION_OWNER_SEPARATOR_BYTES: usize = 4;

#[derive(Default)]
struct BilibiliResolutionMaterializationBudget {
    bytes: usize,
}

impl BilibiliResolutionMaterializationBudget {
    fn charge_allocation(&mut self, bytes: usize) -> Result<(), BilibiliDownloadError> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(resolution_materialization_overflow)?;
        if self.bytes > MAX_BILIBILI_RESOLUTION_SNAPSHOT_BYTES {
            return Err(resolution_materialization_limit_exceeded());
        }
        Ok(())
    }

    fn charge_string(&mut self, label: &str, bytes: usize) -> Result<(), BilibiliDownloadError> {
        if bytes > MAX_BILIBILI_RESOLUTION_STRING_BYTES {
            return Err(BilibiliDownloadError::ResourceExhausted(format!(
                "{label} exceeds the resolution-session limit."
            )));
        }
        self.charge_allocation(bytes)
    }

    fn charge_candidate_buffer(
        &mut self,
        candidate_count: usize,
    ) -> Result<(), BilibiliDownloadError> {
        let bytes = mem::size_of::<BilibiliResolvedCandidate>()
            .checked_mul(candidate_count)
            .ok_or_else(resolution_materialization_overflow)?;
        self.charge_allocation(bytes)
    }
}

fn resolution_materialization_limit_exceeded() -> BilibiliDownloadError {
    BilibiliDownloadError::ResourceExhausted(
        "Bilibili resolution snapshot exceeds the server byte limit.".to_owned(),
    )
}

fn resolution_materialization_overflow() -> BilibiliDownloadError {
    BilibiliDownloadError::ResourceExhausted(
        "Bilibili resolution snapshot byte accounting overflowed.".to_owned(),
    )
}

fn checked_materialized_string_bytes<const N: usize>(
    parts: [usize; N],
) -> Result<usize, BilibiliDownloadError> {
    parts.into_iter().try_fold(0_usize, |total, bytes| {
        total
            .checked_add(bytes)
            .ok_or_else(resolution_materialization_overflow)
    })
}

fn normalized_string(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn non_empty_or_ref<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback
    } else {
        trimmed
    }
}

fn ensure_resolution_materialization_budget(
    source: &str,
    input: &Input,
    resolved: &ResolvedContent,
    candidate_limit: usize,
    include_candidate_cover_uri: bool,
) -> Result<(), BilibiliDownloadError> {
    let mut budget = BilibiliResolutionMaterializationBudget::default();
    budget.charge_allocation(mem::size_of::<BilibiliInputResolution>())?;
    budget.charge_string("Bilibili resolution source", source.len())?;

    match resolved {
        ResolvedContent::Video(video) => {
            let candidate_count = video.pages.len().min(candidate_limit);
            let bvid = normalized_string(video.bvid.as_deref());
            let cover_uri_bytes = if include_candidate_cover_uri {
                video.cover_url.as_deref().map_or(0, str::len)
            } else {
                0
            };
            budget.charge_string("Bilibili resolution title", video.title.len())?;
            budget.charge_string("Bilibili resolution source kind", "video".len())?;
            budget.charge_candidate_buffer(candidate_count)?;

            for page in video.pages.iter().take(candidate_limit) {
                let selection_bytes = checked_materialized_string_bytes([
                    "page:".len(),
                    MAX_U32_DECIMAL_BYTES,
                    ":cid:".len(),
                    MAX_U64_DECIMAL_BYTES,
                    if bvid.is_some() { ":bvid:".len() } else { 0 },
                    bvid.map_or(0, str::len),
                    ":aid:".len(),
                    MAX_U64_DECIMAL_BYTES,
                ])?;
                budget.charge_string("Bilibili candidate selection id", selection_bytes)?;
                budget.charge_string(
                    "Bilibili candidate title",
                    non_empty_or_ref(&page.title, &video.title).len(),
                )?;
                budget.charge_string(
                    "Bilibili candidate subtitle",
                    "Page ".len() + MAX_U32_DECIMAL_BYTES,
                )?;
                budget.charge_string("Bilibili candidate source kind", "video_page".len())?;
                budget.charge_string("Bilibili candidate content id", MAX_U64_DECIMAL_BYTES)?;
                budget.charge_string("Bilibili candidate bvid", bvid.map_or(0, str::len))?;
                budget.charge_string("Bilibili candidate cover URI", cover_uri_bytes)?;
                if candidate_count == 1 {
                    budget.charge_string(
                        "Bilibili default candidate selection id",
                        selection_bytes,
                    )?;
                }
            }
        }
        ResolvedContent::Season(season) => {
            let episodes = if resolve_should_use_full_episode_list(input) {
                &season.season.episodes
            } else {
                &season.selected_episodes
            };
            let candidate_count = episodes.len().min(candidate_limit);
            let cover_uri_bytes = if include_candidate_cover_uri {
                season.season.cover_url.as_deref().map_or(0, str::len)
            } else {
                0
            };
            budget.charge_string("Bilibili resolution title", season.season.title.len())?;
            budget.charge_string("Bilibili resolution source kind", "season".len())?;
            budget.charge_candidate_buffer(candidate_count)?;

            for episode in episodes.iter().take(candidate_limit) {
                let selection_bytes = "episode:".len() + MAX_U64_DECIMAL_BYTES;
                let bvid = normalized_string(episode.bvid.as_deref());
                let subtitle_bytes = episode
                    .long_title
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .map_or("Episode ".len() + MAX_U32_DECIMAL_BYTES, str::len);
                budget.charge_string("Bilibili candidate selection id", selection_bytes)?;
                budget.charge_string(
                    "Bilibili candidate title",
                    non_empty_or_ref(&episode.title, &season.season.title).len(),
                )?;
                budget.charge_string("Bilibili candidate subtitle", subtitle_bytes)?;
                budget.charge_string("Bilibili candidate source kind", "season_episode".len())?;
                budget.charge_string("Bilibili candidate content id", MAX_U64_DECIMAL_BYTES)?;
                budget.charge_string("Bilibili candidate bvid", bvid.map_or(0, str::len))?;
                budget.charge_string("Bilibili candidate cover URI", cover_uri_bytes)?;
                if candidate_count == 1 {
                    budget.charge_string(
                        "Bilibili default candidate selection id",
                        selection_bytes,
                    )?;
                }
            }
        }
        ResolvedContent::Collection(collection) => {
            let source_kind = collection_kind_name(&collection.collection.kind);
            let source_token = collection_source_token(input).ok_or_else(|| {
                BilibiliDownloadError::Failed(
                    "Resolved Bilibili collection is missing a stable source token.".to_owned(),
                )
            })?;
            let candidate_count = collection.selected_items.len().min(candidate_limit);
            budget.charge_string(
                "Bilibili resolution title",
                collection.collection.title.len(),
            )?;
            budget.charge_string("Bilibili resolution source kind", source_kind.len())?;
            budget.charge_candidate_buffer(candidate_count)?;

            for item in collection.selected_items.iter().take(candidate_limit) {
                let bvid = normalized_string(item.bvid.as_deref());
                let selection_bytes = checked_materialized_string_bytes([
                    "item:".len(),
                    MAX_U32_DECIMAL_BYTES,
                    ":source:".len(),
                    source_token.len(),
                    ":cid:".len(),
                    MAX_U64_DECIMAL_BYTES,
                    if bvid.is_some() { ":bvid:".len() } else { 0 },
                    bvid.map_or(0, str::len),
                    ":aid:".len(),
                    MAX_U64_DECIMAL_BYTES,
                ])?;
                let owner_bytes = item
                    .owner
                    .as_ref()
                    .map(|owner| owner.name.trim())
                    .filter(|name| !name.is_empty())
                    .map_or(0, |name| COLLECTION_OWNER_SEPARATOR_BYTES + name.len());
                let subtitle_bytes = checked_materialized_string_bytes([
                    source_kind.len(),
                    " #".len(),
                    MAX_U32_DECIMAL_BYTES,
                    owner_bytes,
                ])?;
                let content_id_bytes = item
                    .bvid
                    .as_deref()
                    .map_or("av".len() + MAX_U64_DECIMAL_BYTES, str::len);
                let cover_uri_bytes = if include_candidate_cover_uri {
                    item.cover_url.as_deref().map_or(0, str::len)
                } else {
                    0
                };
                budget.charge_string("Bilibili candidate selection id", selection_bytes)?;
                budget.charge_string("Bilibili candidate title", item.title.len())?;
                budget.charge_string("Bilibili candidate subtitle", subtitle_bytes)?;
                budget.charge_string("Bilibili candidate source kind", source_kind.len())?;
                budget.charge_string("Bilibili candidate content id", content_id_bytes)?;
                budget.charge_string("Bilibili candidate bvid", bvid.map_or(0, str::len))?;
                budget.charge_string("Bilibili candidate cover URI", cover_uri_bytes)?;
                if candidate_count == 1 {
                    budget.charge_string(
                        "Bilibili default candidate selection id",
                        selection_bytes,
                    )?;
                }
            }
        }
    }

    Ok(())
}

#[allow(dead_code)]
impl BilibiliPlaybackPlan {
    fn from_core(
        plan: PlaybackPlan,
        options: Option<&BilibiliDownloadOptions>,
    ) -> Result<Self, BilibiliDownloadError> {
        let preferences = playback_variant_preferences_from_options(options)?;
        Self::from_core_with_preferences(plan, &preferences)
    }

    fn from_core_with_preferences(
        plan: PlaybackPlan,
        preferences: &PlaybackVariantPreferences,
    ) -> Result<Self, BilibiliDownloadError> {
        Ok(Self {
            title: plan.title,
            entries: plan
                .entries
                .iter()
                .map(|entry| BilibiliPlaybackEntry::from_core(entry, preferences))
                .collect::<Result<Vec<_>, _>>()?,
        })
    }

    fn validate_expected_identity(
        &self,
        expected_identity: &PlaybackExpectedIdentity,
    ) -> Result<(), BilibiliDownloadError> {
        if self
            .entries
            .iter()
            .any(|entry| expected_identity.matches(entry))
        {
            return Ok(());
        }

        Err(BilibiliDownloadError::Failed(
            "Selected Bilibili item no longer matches the resolved candidate. Resolve the input again and retry.".to_owned(),
        ))
    }
}

impl BilibiliInputResolution {
    fn from_resolved_content(
        source: String,
        input: &Input,
        resolved: ResolvedContent,
        candidate_limit: usize,
        include_candidate_cover_uri: bool,
    ) -> Result<Self, BilibiliDownloadError> {
        ensure_resolution_materialization_budget(
            &source,
            input,
            &resolved,
            candidate_limit,
            include_candidate_cover_uri,
        )?;
        Ok(match resolved {
            ResolvedContent::Video(video) => {
                let candidates_truncated = video.pages.len() > candidate_limit;
                let candidates = video
                    .pages
                    .iter()
                    .take(candidate_limit)
                    .map(|page| BilibiliResolvedCandidate {
                        selection_id: page_selection_id(page, video.bvid.as_deref()),
                        title: non_empty_or(&page.title, &video.title),
                        subtitle: format!("Page {}", page.index),
                        source_kind: "video_page".to_owned(),
                        content_id: page.cid.to_string(),
                        identity: BilibiliContentIdentity {
                            kind: BilibiliContentKind::VideoPage,
                            aid: Some(page.aid),
                            bvid: normalized_bvid(video.bvid.as_deref()),
                            cid: Some(page.cid),
                            epid: None,
                        },
                        index: page.index,
                        duration_seconds: page.duration_seconds,
                        cover_uri: if include_candidate_cover_uri {
                            video.cover_url.clone().unwrap_or_default()
                        } else {
                            String::new()
                        },
                    })
                    .collect::<Vec<_>>();
                Self::with_candidates(
                    source,
                    video.title,
                    "video",
                    candidates,
                    candidates_truncated,
                )
            }
            ResolvedContent::Season(season) => {
                let episodes = if resolve_should_use_full_episode_list(input) {
                    &season.season.episodes
                } else {
                    &season.selected_episodes
                };
                let candidates_truncated = episodes.len() > candidate_limit;
                let candidates = episodes
                    .iter()
                    .take(candidate_limit)
                    .map(|episode| {
                        let subtitle = episode
                            .long_title
                            .as_ref()
                            .filter(|value| !value.trim().is_empty())
                            .cloned()
                            .unwrap_or_else(|| format!("Episode {}", episode.index));
                        BilibiliResolvedCandidate {
                            selection_id: episode_selection_id(episode.epid),
                            title: non_empty_or(&episode.title, &season.season.title),
                            subtitle,
                            source_kind: "season_episode".to_owned(),
                            content_id: episode.epid.to_string(),
                            identity: BilibiliContentIdentity {
                                kind: BilibiliContentKind::SeasonEpisode,
                                aid: Some(episode.aid),
                                bvid: normalized_bvid(episode.bvid.as_deref()),
                                cid: Some(episode.cid),
                                epid: Some(episode.epid),
                            },
                            index: episode.index,
                            duration_seconds: None,
                            cover_uri: if include_candidate_cover_uri {
                                season.season.cover_url.clone().unwrap_or_default()
                            } else {
                                String::new()
                            },
                        }
                    })
                    .collect::<Vec<_>>();
                Self::with_candidates(
                    source,
                    season.season.title,
                    "season",
                    candidates,
                    candidates_truncated,
                )
            }
            ResolvedContent::Collection(collection) => {
                let source_kind = collection_kind_name(&collection.collection.kind);
                let candidates_truncated = collection.selected_items.len() > candidate_limit;
                let candidates = collection
                    .selected_items
                    .iter()
                    .take(candidate_limit)
                    .map(|item| BilibiliResolvedCandidate {
                        selection_id: collection_item_selection_id(input, item),
                        title: item.title.clone(),
                        subtitle: collection_item_subtitle(source_kind, item.index, &item.owner),
                        source_kind: source_kind.to_owned(),
                        content_id: item
                            .bvid
                            .clone()
                            .unwrap_or_else(|| format!("av{}", item.aid)),
                        identity: BilibiliContentIdentity {
                            kind: BilibiliContentKind::CollectionItem,
                            aid: Some(item.aid),
                            bvid: normalized_bvid(item.bvid.as_deref()),
                            cid: Some(item.cid),
                            epid: None,
                        },
                        index: item.index,
                        duration_seconds: item.duration_seconds,
                        cover_uri: if include_candidate_cover_uri {
                            item.cover_url.clone().unwrap_or_default()
                        } else {
                            String::new()
                        },
                    })
                    .collect::<Vec<_>>();
                Self::with_candidates(
                    source,
                    collection.collection.title,
                    source_kind,
                    candidates,
                    candidates_truncated,
                )
            }
        })
    }

    fn with_candidates(
        source: String,
        title: String,
        source_kind: impl Into<String>,
        candidates: Vec<BilibiliResolvedCandidate>,
        candidates_truncated: bool,
    ) -> Self {
        let default_selection_id = if candidates.len() == 1 {
            candidates
                .first()
                .map(|candidate| candidate.selection_id.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };
        Self {
            source,
            title,
            source_kind: source_kind.into(),
            candidates,
            default_selection_id,
            candidates_truncated,
        }
    }
}

fn resolve_should_use_full_episode_list(input: &Input) -> bool {
    matches!(
        input,
        Input::Season(_) | Input::Media(_) | Input::CheeseSeason(_)
    )
}

fn normalized_bvid(bvid: Option<&str>) -> Option<String> {
    normalized_string(bvid).map(str::to_owned)
}

fn page_selection_id(page: &bbdown_core::PageMetadata, video_bvid: Option<&str>) -> String {
    video_bvid
        .map(str::trim)
        .filter(|bvid| !bvid.is_empty())
        .map_or_else(
            || format!("page:{}:cid:{}:aid:{}", page.index, page.cid, page.aid),
            |bvid| {
                format!(
                    "page:{}:cid:{}:bvid:{}:aid:{}",
                    page.index, page.cid, bvid, page.aid
                )
            },
        )
}

fn collection_item_selection_id(input: &Input, item: &bbdown_core::VideoCollectionItem) -> String {
    let source_token = collection_source_token(input)
        .expect("resolved Bilibili collection should have a stable source token");
    item.bvid
        .as_deref()
        .map(str::trim)
        .filter(|bvid| !bvid.is_empty())
        .map_or_else(
            || {
                format!(
                    "item:{}:source:{}:cid:{}:aid:{}",
                    item.index, source_token, item.cid, item.aid
                )
            },
            |bvid| {
                format!(
                    "item:{}:source:{}:cid:{}:bvid:{}:aid:{}",
                    item.index, source_token, item.cid, bvid, item.aid
                )
            },
        )
}

fn collection_source_token(input: &Input) -> Option<String> {
    match input {
        Input::SpaceVideos(owner_mid) => Some(format!("space-videos-{owner_mid}")),
        Input::FavoriteList {
            media_id,
            owner_mid,
        } => Some(format!(
            "favorite-{}-{}",
            optional_u64_source_component(*media_id),
            optional_u64_source_component(*owner_mid)
        )),
        Input::CollectionList(list_id) => Some(format!("collection-{list_id}")),
        Input::SeriesList(list_id) => Some(format!("series-{list_id}")),
        Input::SpaceCollectionList { list_id, owner_mid } => {
            Some(format!("space-collection-{owner_mid}-{list_id}"))
        }
        Input::SpaceSeriesList { list_id, owner_mid } => {
            Some(format!("space-series-{owner_mid}-{list_id}"))
        }
        Input::RecommendationFeed => Some("recommendation".to_owned()),
        Input::FollowingFeed => Some("following".to_owned()),
        Input::SpaceDynamic(owner_mid) => Some(format!("space-dynamic-{owner_mid}")),
        Input::History => Some("history".to_owned()),
        Input::WatchLater => Some("watch-later".to_owned()),
        _ => None,
    }
}

fn optional_u64_source_component(value: Option<u64>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| value.to_string())
}

fn episode_selection_id(epid: u64) -> String {
    format!("episode:{epid}")
}

fn non_empty_or(value: &str, fallback: &str) -> String {
    non_empty_or_ref(value, fallback).to_owned()
}

fn collection_kind_name(kind: &VideoCollectionKind) -> &'static str {
    match kind {
        VideoCollectionKind::Space => "space",
        VideoCollectionKind::Favorite => "favorite",
        VideoCollectionKind::Collection => "collection",
        VideoCollectionKind::Series => "series",
        VideoCollectionKind::Recommendation => "recommendation",
        VideoCollectionKind::Following => "following",
        VideoCollectionKind::SpaceDynamic => "space_dynamic",
        VideoCollectionKind::History => "history",
        VideoCollectionKind::WatchLater => "watch_later",
    }
}

fn collection_item_subtitle(
    source_kind: &str,
    index: u32,
    owner: &Option<bbdown_core::Owner>,
) -> String {
    let prefix = format!("{} #{}", source_kind.replace('_', " "), index);
    match owner
        .as_ref()
        .map(|owner| owner.name.trim())
        .filter(|name| !name.is_empty())
    {
        Some(owner_name) => format!("{prefix} · {owner_name}"),
        None => prefix,
    }
}

#[allow(dead_code)]
impl BilibiliPlaybackEntry {
    fn from_core(
        entry: &bbdown_core::PlaybackEntry,
        preferences: &PlaybackVariantPreferences,
    ) -> Result<Self, BilibiliDownloadError> {
        let selected_variant =
            select_playback_variant(&entry.variants, preferences)?.map(|selected| {
                BilibiliSelectedPlaybackVariant {
                    variant: BilibiliPlaybackVariant::from_core(selected.variant),
                    selection: selected.selection,
                }
            });
        Ok(Self {
            index: entry.index,
            aid: entry.aid,
            bvid: entry.bvid.clone(),
            cid: entry.cid,
            epid: entry.epid,
            title: entry.title.clone(),
            content_id: entry.cache_key.content_id.clone(),
            duration_seconds: entry.duration_seconds,
            abr: BilibiliPlaybackAbrMetadata::from_core(&entry.abr),
            selected_variant,
            variants: entry
                .variants
                .iter()
                .map(BilibiliPlaybackVariant::from_core)
                .collect(),
        })
    }
}

#[allow(dead_code)]
impl BilibiliPlaybackVariant {
    fn from_core(variant: &PlaybackVariant) -> Self {
        Self {
            id: variant.id.clone(),
            kind: BilibiliPlaybackVariantKind::from(variant.kind),
            content_id: variant.cache_key.content_id.clone(),
            bandwidth: variant.bandwidth,
            codecs: variant.codecs.clone(),
            mime_types: variant.mime_types.clone(),
            width: variant.width,
            height: variant.height,
            frame_rate: variant.frame_rate.clone(),
            duration_seconds: variant.duration_seconds,
            abr: variant
                .abr
                .as_ref()
                .map(BilibiliPlaybackAbrLevel::from_core),
            video: variant.video.as_ref().map(BilibiliMediaRequest::from_core),
            audio: variant.audio.as_ref().map(BilibiliMediaRequest::from_core),
            flv_segments: variant
                .flv_segments
                .iter()
                .map(BilibiliMediaRequest::from_core)
                .collect(),
        }
    }
}

#[allow(dead_code)]
impl BilibiliPlaybackAbrMetadata {
    fn from_core(metadata: &PlaybackAbrMetadata) -> Self {
        Self {
            groups: metadata
                .groups
                .iter()
                .map(BilibiliPlaybackAbrGroup::from_core)
                .collect(),
        }
    }
}

#[allow(dead_code)]
impl BilibiliPlaybackAbrGroup {
    fn from_core(group: &PlaybackAbrGroup) -> Self {
        Self {
            id: group.id.clone(),
            kind: BilibiliPlaybackAbrGroupKind::from(group.kind),
            variant_ids: group.variant_ids.clone(),
            level_count: group.level_count,
            min_bandwidth: group.min_bandwidth,
            max_bandwidth: group.max_bandwidth,
        }
    }
}

#[allow(dead_code)]
impl BilibiliPlaybackAbrLevel {
    fn from_core(level: &PlaybackAbrLevel) -> Self {
        Self {
            group_id: level.group_id.clone(),
            level_index: level.level_index,
            level_count: level.level_count,
            switchable: level.switchable,
        }
    }
}

#[allow(dead_code)]
impl BilibiliMediaRequest {
    fn from_core(request: &MediaRequestSpec) -> Self {
        Self {
            kind: BilibiliMediaRequestKind::from(request.kind),
            stream_id: request.stream_id,
            url: request.url.clone(),
            backup_urls: request.backup_urls.clone(),
            headers: request
                .headers
                .iter()
                .map(BilibiliHttpHeader::from_core)
                .collect(),
            mime_type: request.mime_type.clone(),
            codecs: request.codecs.clone(),
            bandwidth: request.bandwidth,
            width: request.width,
            height: request.height,
            frame_rate: request.frame_rate.clone(),
            size: request.size,
            duration_seconds: request.duration_seconds,
            cache_key: BilibiliMediaCacheKey {
                content_id: request.cache_key.content_id.clone(),
                media_kind: BilibiliMediaRequestKind::from(request.cache_key.media_kind),
                stream_id: request.cache_key.stream_id,
                codecs: request.cache_key.codecs.clone(),
                source_hash: request.cache_key.source_hash.clone(),
            },
        }
    }
}

#[allow(dead_code)]
impl BilibiliHttpHeader {
    fn from_core(header: &HttpHeaderSpec) -> Self {
        Self {
            name: header.name.clone(),
            value: header.value.clone(),
        }
    }
}

impl From<PlaybackAbrGroupKind> for BilibiliPlaybackAbrGroupKind {
    fn from(kind: PlaybackAbrGroupKind) -> Self {
        match kind {
            PlaybackAbrGroupKind::DashVideo => Self::DashVideo,
            PlaybackAbrGroupKind::DashAudioOnly => Self::DashAudioOnly,
        }
    }
}

impl From<PlaybackVariantKind> for BilibiliPlaybackVariantKind {
    fn from(kind: PlaybackVariantKind) -> Self {
        match kind {
            PlaybackVariantKind::Dash => Self::Dash,
            PlaybackVariantKind::Flv => Self::Flv,
        }
    }
}

impl From<MediaRequestKind> for BilibiliMediaRequestKind {
    fn from(kind: MediaRequestKind) -> Self {
        match kind {
            MediaRequestKind::Video => Self::Video,
            MediaRequestKind::Audio => Self::Audio,
            MediaRequestKind::FlvSegment => Self::FlvSegment,
        }
    }
}

#[allow(dead_code)]
fn playback_variant_preferences_from_options(
    options: Option<&BilibiliDownloadOptions>,
) -> Result<PlaybackVariantPreferences, BilibiliDownloadError> {
    playback_variant_preferences_from_options_with_policy(options, PlaybackPolicy::default())
}

fn playback_variant_preferences_from_options_with_policy(
    options: Option<&BilibiliDownloadOptions>,
    policy: PlaybackPolicy,
) -> Result<PlaybackVariantPreferences, BilibiliDownloadError> {
    let encoding_preference = playback_explicit_encoding_preference(options).map(str::to_owned);
    Ok(PlaybackVariantPreferences {
        codec_candidates: playback_codec_preferences_from_options(options)?,
        quality_preference: playback_quality_preference_from_options(options)?,
        allow_avplayer_hint_fallback: encoding_preference.is_none(),
        prefer_conservative_compatible: encoding_preference.is_none()
            && policy.compatible_variant_preference
                == CompatibleVariantPreference::PreferCompatible,
        encoding_preference,
    })
}

#[allow(dead_code)]
fn playback_explicit_encoding_preference(
    options: Option<&BilibiliDownloadOptions>,
) -> Option<&str> {
    let options = options?;
    let normalized = normalized_preference_token(&options.encoding_preference);
    if matches!(normalized.as_str(), "" | "auto" | "default" | "best") {
        None
    } else {
        Some(options.encoding_preference.trim())
    }
}

#[allow(dead_code)]
fn playback_codec_preferences_from_options(
    options: Option<&BilibiliDownloadOptions>,
) -> Result<Vec<PlaybackCodecPreferenceCandidate>, BilibiliDownloadError> {
    let encoding_preference = options
        .map(|options| normalized_preference_token(&options.encoding_preference))
        .unwrap_or_default();
    let preferences = match encoding_preference.as_str() {
        "" | "auto" | "default" | "best" => vec![
            PlaybackCodecPreferenceCandidate {
                policy: BilibiliPlaybackVariantSelectionPolicy::AvPlayerDefault,
                preference: PlaybackCodecPreference::avplayer_default(),
            },
            PlaybackCodecPreferenceCandidate {
                policy: BilibiliPlaybackVariantSelectionPolicy::H264AacFallback,
                preference: PlaybackCodecPreference::h264_aac(),
            },
        ],
        "h264" | "avc" | "avc1" => vec![PlaybackCodecPreferenceCandidate {
            policy: BilibiliPlaybackVariantSelectionPolicy::ExplicitEncodingPreference,
            preference: PlaybackCodecPreference::h264_aac(),
        }],
        "hevc" | "h265" | "hev1" | "hvc1" => vec![
            PlaybackCodecPreferenceCandidate {
                policy: BilibiliPlaybackVariantSelectionPolicy::ExplicitEncodingPreference,
                preference: PlaybackCodecPreference::hevc_aac(),
            },
            PlaybackCodecPreferenceCandidate {
                policy: BilibiliPlaybackVariantSelectionPolicy::H264AacFallback,
                preference: PlaybackCodecPreference::h264_aac(),
            },
        ],
        "av1" | "av01" => vec![
            PlaybackCodecPreferenceCandidate {
                policy: BilibiliPlaybackVariantSelectionPolicy::ExplicitEncodingPreference,
                preference: PlaybackCodecPreference::av1_aac(),
            },
            PlaybackCodecPreferenceCandidate {
                policy: BilibiliPlaybackVariantSelectionPolicy::H264AacFallback,
                preference: PlaybackCodecPreference::h264_aac(),
            },
        ],
        _ => {
            return Err(BilibiliDownloadError::Failed(format!(
                "BBDown playback planning does not support encoding_preference {encoding_preference:?}."
            )));
        }
    };
    Ok(preferences)
}

#[allow(dead_code)]
fn playback_quality_preference_from_options(
    options: Option<&BilibiliDownloadOptions>,
) -> Result<Option<u32>, BilibiliDownloadError> {
    let Some(options) = options else {
        return Ok(None);
    };

    let quality_preference = normalized_preference_token(&options.quality_preference);
    if matches!(
        quality_preference.as_str(),
        "" | "auto" | "default" | "best"
    ) {
        return Ok(None);
    }

    video_quality_preference(&options.quality_preference).map(Some).ok_or_else(|| {
        BilibiliDownloadError::Failed(format!(
            "BBDown playback planning does not support quality_preference {quality_preference:?}."
        ))
    })
}

#[allow(dead_code)]
fn select_playback_variant<'a>(
    variants: &'a [PlaybackVariant],
    preferences: &PlaybackVariantPreferences,
) -> Result<Option<SelectedCorePlaybackVariant<'a>>, BilibiliDownloadError> {
    let candidate_variants =
        playback_variants_matching_quality(variants, preferences.quality_preference)?;
    let compatible_variant = preferences
        .prefer_conservative_compatible
        .then(|| {
            candidate_variants
                .iter()
                .copied()
                .filter(|variant| variant.selection_hints.avplayer.playable)
                .filter(|variant| core_variant_is_avplayer_h264_aac_hls_compatible(variant))
                .min_by(|left, right| compare_playback_variants(left, 0, right, 0))
                .or_else(|| {
                    variants
                        .iter()
                        .filter(|variant| variant.selection_hints.avplayer.playable)
                        .filter(|variant| {
                            playback_variant_does_not_exceed_requested_quality(
                                variant,
                                preferences.quality_preference,
                            )
                        })
                        .filter(|variant| core_variant_is_avplayer_h264_aac_hls_compatible(variant))
                        .min_by(|left, right| compare_playback_variants(left, 0, right, 0))
                })
        })
        .flatten();

    for candidate in &preferences.codec_candidates {
        if let Some((variant, codec_rank)) = candidate_variants
            .iter()
            .copied()
            .filter(|variant| variant.selection_hints.avplayer.playable)
            .filter_map(|variant| {
                variant
                    .codec_preference_rank(&candidate.preference)
                    .map(|rank| (variant, rank))
            })
            .min_by(|(left, left_rank), (right, right_rank)| {
                compare_playback_variants(left, *left_rank, right, *right_rank)
            })
        {
            let selected = SelectedCorePlaybackVariant {
                variant,
                selection: BilibiliPlaybackVariantSelection {
                    policy: candidate.policy,
                    codec_rank: Some(codec_rank),
                    score: variant.selection_hints.avplayer.score,
                },
            };
            return Ok(Some(prefer_compatible_variant(
                selected,
                compatible_variant,
            )));
        }
    }

    if !preferences.allow_avplayer_hint_fallback {
        let encoding_preference = preferences
            .encoding_preference
            .as_deref()
            .unwrap_or("explicit encoding");
        return Err(BilibiliDownloadError::Failed(format!(
            "BBDown playback planning found no variant matching encoding_preference {encoding_preference:?}."
        )));
    }

    let selected = candidate_variants
        .iter()
        .copied()
        .filter(|variant| variant.selection_hints.avplayer.playable)
        .min_by(|left, right| compare_playback_variants(left, 0, right, 0))
        .map(|variant| SelectedCorePlaybackVariant {
            variant,
            selection: BilibiliPlaybackVariantSelection {
                policy: BilibiliPlaybackVariantSelectionPolicy::AvPlayerHintFallback,
                codec_rank: None,
                score: variant.selection_hints.avplayer.score,
            },
        });
    Ok(selected
        .map(|selected| prefer_compatible_variant(selected, compatible_variant))
        .or_else(|| compatible_variant.map(conservative_compatible_selection)))
}

#[allow(dead_code)]
fn prefer_compatible_variant<'a>(
    selected: SelectedCorePlaybackVariant<'a>,
    compatible_variant: Option<&'a PlaybackVariant>,
) -> SelectedCorePlaybackVariant<'a> {
    if core_variant_is_avplayer_h264_aac_hls_compatible(selected.variant) {
        return selected;
    }
    compatible_variant
        .map(conservative_compatible_selection)
        .unwrap_or(selected)
}

#[allow(dead_code)]
fn conservative_compatible_selection(variant: &PlaybackVariant) -> SelectedCorePlaybackVariant<'_> {
    SelectedCorePlaybackVariant {
        variant,
        selection: BilibiliPlaybackVariantSelection {
            policy: BilibiliPlaybackVariantSelectionPolicy::H264AacFallback,
            codec_rank: None,
            score: variant.selection_hints.avplayer.score,
        },
    }
}

#[allow(dead_code)]
fn core_variant_is_avplayer_h264_aac_hls_compatible(variant: &PlaybackVariant) -> bool {
    variant_is_avplayer_h264_aac_hls_compatible(&BilibiliPlaybackVariant::from_core(variant))
}

#[allow(dead_code)]
fn playback_variant_does_not_exceed_requested_quality(
    variant: &PlaybackVariant,
    quality_preference: Option<u32>,
) -> bool {
    quality_preference.is_none_or(|requested| {
        playback_variant_stream_id(variant).is_some_and(|quality| quality <= requested)
    })
}

#[allow(dead_code)]
fn playback_variants_matching_quality(
    variants: &[PlaybackVariant],
    quality_preference: Option<u32>,
) -> Result<Vec<&PlaybackVariant>, BilibiliDownloadError> {
    let Some(quality_preference) = quality_preference else {
        return Ok(variants.iter().collect());
    };

    let matches = variants
        .iter()
        .filter(|variant| playback_variant_stream_id(variant) == Some(quality_preference))
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Err(BilibiliDownloadError::Failed(format!(
            "BBDown playback planning found no variant matching quality_preference {quality_preference}."
        )));
    }

    Ok(matches)
}

#[allow(dead_code)]
fn playback_variant_stream_id(variant: &PlaybackVariant) -> Option<u32> {
    variant
        .video
        .as_ref()
        .and_then(|video| video.stream_id)
        .or_else(|| {
            variant
                .flv_segments
                .iter()
                .find_map(|segment| segment.stream_id)
        })
}

#[allow(dead_code)]
fn compare_playback_variants(
    left: &PlaybackVariant,
    left_rank: usize,
    right: &PlaybackVariant,
    right_rank: usize,
) -> std::cmp::Ordering {
    left_rank
        .cmp(&right_rank)
        .then_with(|| {
            right
                .selection_hints
                .avplayer
                .preferred
                .cmp(&left.selection_hints.avplayer.preferred)
        })
        .then_with(|| {
            right
                .selection_hints
                .avplayer
                .score
                .cmp(&left.selection_hints.avplayer.score)
        })
        .then_with(|| {
            right
                .bandwidth
                .unwrap_or(0)
                .cmp(&left.bandwidth.unwrap_or(0))
        })
        .then_with(|| left.id.cmp(&right.id))
}

fn validate_supported_download_options(
    options: Option<&BilibiliDownloadOptions>,
) -> Result<(), BilibiliDownloadError> {
    let Some(options) = options else {
        return Ok(());
    };

    let encoding_preference = options.encoding_preference.trim();
    if !encoding_preference.is_empty() {
        return Err(BilibiliDownloadError::Failed(format!(
            "BBDown adapter does not support encoding_preference yet; received {encoding_preference:?}."
        )));
    }

    let subtitle_ai_policy = subtitle_ai_policy_from_options(Some(options))?;
    if !options.download_subtitles && subtitle_ai_policy != SubtitleAiPolicy::Include {
        return Err(BilibiliDownloadError::Failed(
            "Bilibili subtitle_ai_policy requires download_subtitles.".to_owned(),
        ));
    }

    let danmaku_formats = danmaku_formats_from_options(Some(options))?;
    if !options.download_danmaku && danmaku_formats.is_some() {
        return Err(BilibiliDownloadError::Failed(
            "Bilibili danmaku_formats requires download_danmaku.".to_owned(),
        ));
    }

    Ok(())
}

fn download_options_for_output_dir(
    output_dir: PathBuf,
    options: Option<&BilibiliDownloadOptions>,
) -> Result<DownloadOptions, BilibiliDownloadError> {
    validate_supported_download_options(options)?;

    let mut download_options = DownloadOptions::new(output_dir)
        .with_stream_selection(stream_selection_from_options(options))
        .with_download_mode(download_mode_from_options(options)?)
        .with_cover(options.is_some_and(|options| options.download_cover))
        .with_subtitles(options.is_some_and(|options| options.download_subtitles))
        .with_subtitle_ai_policy(subtitle_ai_policy_from_options(options)?)
        .with_danmaku(options.is_some_and(|options| options.download_danmaku))
        .with_mux(MuxOptions::Disabled);

    if let Some(danmaku_formats) = danmaku_formats_from_options(options)? {
        download_options = download_options.with_danmaku_formats(danmaku_formats);
    }

    Ok(download_options)
}

fn stream_selection_from_options(options: Option<&BilibiliDownloadOptions>) -> StreamSelection {
    let video_quality = options
        .and_then(|options| video_quality_preference(&options.quality_preference))
        .map(Some)
        .unwrap_or_default();

    let mut selection = StreamSelection::new(video_quality, None);
    if let Some(audio_language) = options
        .map(|options| options.audio_language.trim())
        .filter(|audio_language| !audio_language.is_empty())
    {
        selection = selection.with_audio_language(audio_language.to_owned());
    }
    selection
}

fn subtitle_ai_policy_from_options(
    options: Option<&BilibiliDownloadOptions>,
) -> Result<SubtitleAiPolicy, BilibiliDownloadError> {
    let Some(options) = options else {
        return Ok(SubtitleAiPolicy::Include);
    };

    match BilibiliSubtitleAiPolicy::try_from(options.subtitle_ai_policy) {
        Ok(BilibiliSubtitleAiPolicy::Unspecified | BilibiliSubtitleAiPolicy::Include) => {
            Ok(SubtitleAiPolicy::Include)
        }
        Ok(BilibiliSubtitleAiPolicy::PreferNonAi) => Ok(SubtitleAiPolicy::PreferNonAi),
        Ok(BilibiliSubtitleAiPolicy::ExcludeAi) => Ok(SubtitleAiPolicy::ExcludeAi),
        Ok(BilibiliSubtitleAiPolicy::OnlyAi) => Ok(SubtitleAiPolicy::OnlyAi),
        Err(_) => Err(BilibiliDownloadError::Failed(format!(
            "Unsupported Bilibili subtitle_ai_policy value: {}.",
            options.subtitle_ai_policy
        ))),
    }
}

fn danmaku_formats_from_options(
    options: Option<&BilibiliDownloadOptions>,
) -> Result<Option<Vec<DanmakuFormat>>, BilibiliDownloadError> {
    let Some(options) = options else {
        return Ok(None);
    };

    if options.danmaku_formats.is_empty() {
        return Ok(None);
    }

    let mut include_xml = false;
    let mut include_ass = false;
    for value in &options.danmaku_formats {
        match BilibiliDanmakuFormat::try_from(*value) {
            Ok(BilibiliDanmakuFormat::Unspecified) => {}
            Ok(BilibiliDanmakuFormat::Xml) => include_xml = true,
            Ok(BilibiliDanmakuFormat::Ass) => include_ass = true,
            Err(_) => {
                return Err(BilibiliDownloadError::Failed(format!(
                    "Unsupported Bilibili danmaku_format value: {value}."
                )));
            }
        }
    }

    let mut formats = Vec::new();
    if include_xml {
        formats.push(DanmakuFormat::Xml);
    }
    if include_ass {
        formats.push(DanmakuFormat::Ass);
    }

    if formats.is_empty() {
        Ok(None)
    } else {
        Ok(Some(formats))
    }
}

fn video_quality_preference(value: &str) -> Option<u32> {
    let normalized = normalized_preference_token(value);
    match normalized.as_str() {
        "" | "auto" | "default" | "best" => None,
        "360" | "360p" => Some(16),
        "480" | "480p" => Some(32),
        "720" | "720p" => Some(64),
        "1080" | "1080p" | "fullhd" | "fhd" => Some(80),
        "1080p+" | "1080plus" | "1080pplus" => Some(112),
        "1080p60" | "108060" => Some(116),
        "4k" | "2160" | "2160p" => Some(120),
        "hdr" => Some(125),
        "dolby" => Some(126),
        "8k" | "4320" | "4320p" => Some(127),
        _ => normalized.parse().ok(),
    }
}

struct MappedV2DownloadResult {
    result: TaskResult,
    library_item_id: String,
    resources: Vec<TaskResourceRecord>,
    resource_bodies: Vec<BilibiliTaskResourceBody>,
    library_item_lease: Option<LibraryItemPublicationLease>,
    unpublished_output_paths: Vec<PathBuf>,
    transient_output_paths: Vec<PathBuf>,
}

#[derive(Default)]
struct RetainedV2DownloadBacking {
    resources: Vec<TaskResourceRecord>,
    resource_bodies: Vec<BilibiliTaskResourceBody>,
    library_item_leases: Vec<LibraryItemPublicationLease>,
    unpublished_output_paths: Vec<PathBuf>,
    transient_output_paths: Vec<PathBuf>,
}

struct MappedV2Artifact {
    artifact: TaskArtifact,
    resource: TaskResourceRecord,
    body: BilibiliTaskResourceBody,
}

fn retain_v2_success(
    mapped: MappedV2DownloadResult,
    primary_library_item_id: &mut String,
    successful_results: &mut usize,
    results: &mut Vec<TaskResult>,
    retained_backing: &mut RetainedV2DownloadBacking,
) {
    if primary_library_item_id.is_empty() && !mapped.library_item_id.is_empty() {
        *primary_library_item_id = mapped.library_item_id.clone();
    }
    *successful_results = successful_results.saturating_add(1);
    retained_backing.resources.extend(mapped.resources);
    retained_backing
        .resource_bodies
        .extend(mapped.resource_bodies);
    retained_backing
        .library_item_leases
        .extend(mapped.library_item_lease);
    retained_backing
        .unpublished_output_paths
        .extend(mapped.unpublished_output_paths);
    retained_backing
        .transient_output_paths
        .extend(mapped.transient_output_paths);
    results.push(mapped.result);
}

fn log_v2_candidate_error(task_id: &str, result_id: &str, error: &BilibiliDownloadError) {
    eprintln!(
        "Bilibili v2 task {task_id} candidate {result_id} failed: {}",
        download_error_detail(error)
    );
}

fn report_v2_candidate_finished(
    context: &BilibiliDownloadContext,
    offset: usize,
    total: usize,
    downloaded_bytes: u64,
    total_bytes: u64,
) {
    context.report_progress(BilibiliTaskProgress {
        progress: Some(v2_candidate_progress(offset, total, 1.0)),
        downloaded_bytes: Some(to_i64_saturating(downloaded_bytes)),
        total_bytes: Some(to_i64_saturating(total_bytes.max(downloaded_bytes))),
        message: Some(format!(
            "Finished Bilibili download {}/{}.",
            offset + 1,
            total
        )),
    });
}

fn bilibili_v2_result_id(task_id: &str, offset: usize) -> String {
    if offset == 0 {
        task_id.to_owned()
    } else {
        format!("{task_id}-result-{}", offset + 1)
    }
}

fn v2_candidate_progress(offset: usize, total: usize, candidate_progress: f64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    ((offset as f64) + candidate_progress.clamp(0.0, 1.0)) / total as f64
}

fn download_entry_matches_candidate(
    entry: &bbdown_core::DownloadEntry,
    candidate: &BilibiliTaskCandidateRecord,
) -> bool {
    let identity = &candidate.identity;
    identity.aid.is_none_or(|aid| aid == entry.aid)
        && identity.cid.is_none_or(|cid| cid == entry.cid)
        && identity.epid.is_none_or(|epid| entry.epid == Some(epid))
        && identity.bvid.as_deref().is_none_or(|bvid| {
            entry
                .bvid
                .as_deref()
                .is_some_and(|actual| actual.eq_ignore_ascii_case(bvid))
        })
}

fn playable_entry_output_candidates(entry: &EntryDownloadReport) -> Vec<PathBuf> {
    let mut candidates = entry
        .mux
        .iter()
        .map(|mux| mux.output_path.clone())
        .collect::<Vec<_>>();
    candidates.extend(
        entry
            .files
            .iter()
            .filter(|file| {
                matches!(
                    &file.kind,
                    DownloadFileKind::Video | DownloadFileKind::FlvSegment
                )
            })
            .map(|file| file.path.clone()),
    );
    candidates
}

fn download_report_output_paths(report: &DownloadReport) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for entry in &report.entries {
        for path in entry
            .mux
            .iter()
            .map(|mux| &mux.output_path)
            .chain(entry.files.iter().map(|file| &file.path))
        {
            if seen.insert(path.clone()) {
                paths.push(path.clone());
            }
        }
    }
    paths
}

fn transient_download_output_paths(
    output_paths: &[PathBuf],
    library_output_path: Option<&Path>,
) -> Vec<PathBuf> {
    output_paths
        .iter()
        .filter(|path| Some(path.as_path()) != library_output_path)
        .cloned()
        .collect()
}

async fn cleanup_unpublished_download_report(report: &DownloadReport) {
    cleanup_unpublished_output_paths(&download_report_output_paths(report)).await;
}

fn successful_download_result(
    id: String,
    candidate: &BilibiliTaskCandidateRecord,
    library_item_id: String,
    artifacts: Vec<TaskArtifact>,
    total_bytes: u64,
) -> TaskResult {
    TaskResult {
        id,
        state: TaskState::Succeeded.into(),
        title: candidate.title.clone(),
        subtitle: candidate.subtitle.clone(),
        progress: Some(TaskResultProgress {
            fraction: 1.0,
            completed_bytes: to_i64_saturating(total_bytes),
            total_bytes: to_i64_saturating(total_bytes),
            total_bytes_known: true,
            phase: "completed".to_owned(),
            message: "Downloaded into the LAN cache.".to_owned(),
        }),
        problem: None,
        library_item_id,
        playback_source: None,
        artifacts,
        created_at: None,
        updated_at: None,
        subject: Some(task_result_subject(candidate)),
        provider_details: Some(task_result_provider_details(candidate)),
    }
}

fn failed_download_result(
    id: String,
    candidate: &BilibiliTaskCandidateRecord,
    error: &BilibiliDownloadError,
) -> TaskResult {
    let (category, code, message, retryable) = match error {
        BilibiliDownloadError::ResourceExhausted(_) => (
            TaskProblemCategory::ResourceLimit,
            "bilibili.resource_limit",
            "The Bilibili download exceeded a server resource limit.",
            true,
        ),
        BilibiliDownloadError::Cancelled(_) => (
            TaskProblemCategory::Cancelled,
            "task.cancelled",
            "Cancelled by request.",
            false,
        ),
        BilibiliDownloadError::Failed(_) => (
            TaskProblemCategory::Upstream,
            "bilibili.download_failed",
            "The Bilibili download failed.",
            true,
        ),
    };
    TaskResult {
        id,
        state: TaskState::Failed.into(),
        title: candidate.title.clone(),
        subtitle: candidate.subtitle.clone(),
        progress: Some(TaskResultProgress {
            fraction: 0.0,
            completed_bytes: 0,
            total_bytes: 0,
            total_bytes_known: false,
            phase: "failed".to_owned(),
            message: "Bilibili download failed.".to_owned(),
        }),
        problem: Some(TaskProblem {
            category: category.into(),
            code: code.to_owned(),
            message: message.to_owned(),
            retryable,
        }),
        library_item_id: String::new(),
        playback_source: None,
        artifacts: Vec::new(),
        created_at: None,
        updated_at: None,
        subject: Some(task_result_subject(candidate)),
        provider_details: Some(task_result_provider_details(candidate)),
    }
}

fn cancelled_download_result(id: String, candidate: &BilibiliTaskCandidateRecord) -> TaskResult {
    TaskResult {
        id,
        state: TaskState::Cancelled.into(),
        title: candidate.title.clone(),
        subtitle: candidate.subtitle.clone(),
        progress: Some(TaskResultProgress {
            fraction: 0.0,
            completed_bytes: 0,
            total_bytes: 0,
            total_bytes_known: false,
            phase: "cancelled".to_owned(),
            message: "Cancelled by request.".to_owned(),
        }),
        problem: Some(TaskProblem {
            category: TaskProblemCategory::Cancelled.into(),
            code: "task.cancelled".to_owned(),
            message: "Cancelled by request.".to_owned(),
            retryable: false,
        }),
        library_item_id: String::new(),
        playback_source: None,
        artifacts: Vec::new(),
        created_at: None,
        updated_at: None,
        subject: Some(task_result_subject(candidate)),
        provider_details: Some(task_result_provider_details(candidate)),
    }
}

fn task_result_subject(candidate: &BilibiliTaskCandidateRecord) -> TaskResultSubject {
    TaskResultSubject {
        provider: "bilibili".to_owned(),
        kind: candidate.source_kind.clone(),
        id: candidate.content_id.clone(),
        index: candidate.index,
    }
}

fn task_result_provider_details(
    candidate: &BilibiliTaskCandidateRecord,
) -> TaskResultProviderDetails {
    TaskResultProviderDetails {
        details: Some(
            crate::generated::tvos_net_player::v1::task_result_provider_details::Details::Bilibili(
                BilibiliTaskResultDetails {
                    identity: Some(proto_candidate_identity(candidate)),
                    playback_session: None,
                },
            ),
        ),
    }
}

fn proto_candidate_identity(
    candidate: &BilibiliTaskCandidateRecord,
) -> ProtoBilibiliContentIdentity {
    use crate::generated::tvos_net_player::v1::BilibiliContentKind as ProtoKind;
    ProtoBilibiliContentIdentity {
        kind: match candidate.identity.kind {
            BilibiliContentKind::VideoPage => ProtoKind::VideoPage.into(),
            BilibiliContentKind::SeasonEpisode => ProtoKind::SeasonEpisode.into(),
            BilibiliContentKind::CollectionItem => ProtoKind::CollectionItem.into(),
        },
        aid: candidate.identity.aid.unwrap_or_default(),
        bvid: candidate.identity.bvid.clone().unwrap_or_default(),
        cid: candidate.identity.cid.unwrap_or_default(),
        epid: candidate.identity.epid.unwrap_or_default(),
    }
}

fn library_media_artifact(entry: &EntryDownloadReport, library_item_id: &str) -> TaskArtifact {
    let format = entry
        .mux
        .as_ref()
        .and_then(|mux| mux.output_path.extension())
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .unwrap_or_else(|| "media".to_owned());
    TaskArtifact {
        id: new_artifact_id(),
        kind: TaskArtifactKind::Media.into(),
        state: TaskArtifactState::Available.into(),
        title: "Media".to_owned(),
        format,
        language_tag: String::new(),
        is_ai_generated: false,
        resource: None,
        problem: None,
        library_item_id: library_item_id.to_owned(),
    }
}

async fn map_sidecar_artifact(
    file: &bbdown_core::DownloadedFile,
    index: usize,
) -> Result<MappedV2Artifact, BilibiliDownloadError> {
    let metadata = fs::symlink_metadata(&file.path).await.map_err(failed)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(BilibiliDownloadError::Failed(
            "BBDown sidecar output is not a regular file.".to_owned(),
        ));
    }
    let (kind, title, format, content_type) = sidecar_description(&file.kind, &file.path, index);
    let resource_id = new_resource_id();
    let resource = task_resource_record(&resource_id, content_type, metadata.len())?;
    let artifact = TaskArtifact {
        id: new_artifact_id(),
        kind: kind.into(),
        state: TaskArtifactState::Available.into(),
        title,
        format,
        language_tag: String::new(),
        is_ai_generated: false,
        resource: Some(resource.resource.clone()),
        problem: None,
        library_item_id: String::new(),
    };
    Ok(MappedV2Artifact {
        artifact,
        resource,
        body: BilibiliTaskResourceBody {
            resource_id,
            source: BilibiliTaskResourceBodySource::CachePath(file.path.clone()),
        },
    })
}

fn map_generated_artifact(
    kind: TaskArtifactKind,
    title: &str,
    format: &str,
    content_type: &str,
    body: Vec<u8>,
) -> Result<MappedV2Artifact, BilibiliDownloadError> {
    let resource_id = new_resource_id();
    let size = u64::try_from(body.len()).map_err(failed)?;
    let resource = task_resource_record(&resource_id, content_type, size)?;
    let artifact = TaskArtifact {
        id: new_artifact_id(),
        kind: kind.into(),
        state: TaskArtifactState::Available.into(),
        title: title.to_owned(),
        format: format.to_owned(),
        language_tag: String::new(),
        is_ai_generated: false,
        resource: Some(resource.resource.clone()),
        problem: None,
        library_item_id: String::new(),
    };
    Ok(MappedV2Artifact {
        artifact,
        resource,
        body: BilibiliTaskResourceBody {
            resource_id,
            source: BilibiliTaskResourceBodySource::Bytes(body),
        },
    })
}

fn task_resource_record(
    id: &str,
    content_type: &str,
    size: u64,
) -> Result<TaskResourceRecord, BilibiliDownloadError> {
    let size_bytes = i64::try_from(size).map_err(|_| {
        BilibiliDownloadError::ResourceExhausted(
            "Bilibili artifact is too large to publish.".to_owned(),
        )
    })?;
    TaskResourceRecord::new(CacheResourceRef {
        id: id.to_owned(),
        uri: String::new(),
        content_type: content_type.to_owned(),
        size_bytes,
        size_known: true,
        supports_byte_ranges: true,
        etag: format!("\"{id}\""),
        expires_at: None,
    })
    .map_err(failed)
}

fn sidecar_description(
    kind: &DownloadFileKind,
    path: &Path,
    index: usize,
) -> (TaskArtifactKind, String, String, &'static str) {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 16
                && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .unwrap_or_else(|| "bin".to_owned());
    match kind {
        DownloadFileKind::Cover => (
            TaskArtifactKind::CoverImage,
            "Cover".to_owned(),
            extension.clone(),
            image_content_type(&extension),
        ),
        DownloadFileKind::Subtitle => (
            TaskArtifactKind::Subtitle,
            format!("Subtitle {}", index + 1),
            extension.clone(),
            text_content_type(&extension),
        ),
        DownloadFileKind::Danmaku => (
            TaskArtifactKind::TimedComments,
            "Danmaku XML".to_owned(),
            "xml".to_owned(),
            "application/xml",
        ),
        DownloadFileKind::DanmakuAss => (
            TaskArtifactKind::TimedComments,
            "Danmaku ASS".to_owned(),
            "ass".to_owned(),
            "text/x-ass; charset=utf-8",
        ),
        _ => (
            TaskArtifactKind::Other,
            format!("Artifact {}", index + 1),
            extension,
            "application/octet-stream",
        ),
    }
}

fn image_content_type(extension: &str) -> &'static str {
    match extension {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    }
}

fn text_content_type(extension: &str) -> &'static str {
    match extension {
        "json" => "application/json",
        "ass" => "text/x-ass; charset=utf-8",
        "srt" => "application/x-subrip; charset=utf-8",
        "vtt" => "text/vtt; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn new_resource_id() -> String {
    format!("task-resource-{}", Uuid::new_v4().simple())
}

fn new_artifact_id() -> String {
    format!("task-artifact-{}", Uuid::new_v4().simple())
}

fn download_error_detail(error: &BilibiliDownloadError) -> &str {
    match error {
        BilibiliDownloadError::Failed(message)
        | BilibiliDownloadError::ResourceExhausted(message)
        | BilibiliDownloadError::Cancelled(message) => message,
    }
}

fn download_mode_requires_media(mode: DownloadMode) -> bool {
    matches!(
        mode,
        DownloadMode::All | DownloadMode::VideoOnly | DownloadMode::AudioOnly
    )
}

fn sidecar_only_required_artifact_kind(mode: DownloadMode) -> Option<TaskArtifactKind> {
    match mode {
        DownloadMode::SubtitleOnly => Some(TaskArtifactKind::Subtitle),
        DownloadMode::DanmakuOnly => Some(TaskArtifactKind::TimedComments),
        DownloadMode::CoverOnly => Some(TaskArtifactKind::CoverImage),
        DownloadMode::All | DownloadMode::VideoOnly | DownloadMode::AudioOnly | _ => None,
    }
}

fn download_mode_from_options(
    options: Option<&BilibiliDownloadOptions>,
) -> Result<DownloadMode, BilibiliDownloadError> {
    let mode = options
        .map(|options| BilibiliDownloadMode::try_from(options.download_mode))
        .transpose()
        .map_err(|_| {
            BilibiliDownloadError::Failed(
                "Bilibili download mode is unknown to this cache server.".to_owned(),
            )
        })?
        .unwrap_or(BilibiliDownloadMode::Unspecified);
    Ok(match mode {
        BilibiliDownloadMode::Unspecified | BilibiliDownloadMode::All => DownloadMode::All,
        BilibiliDownloadMode::VideoOnly => DownloadMode::VideoOnly,
        BilibiliDownloadMode::AudioOnly => DownloadMode::AudioOnly,
        BilibiliDownloadMode::SubtitleOnly => DownloadMode::SubtitleOnly,
        BilibiliDownloadMode::DanmakuOnly => DownloadMode::DanmakuOnly,
        BilibiliDownloadMode::CoverOnly => DownloadMode::CoverOnly,
    })
}

fn normalized_preference_token(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '_', '-'], "")
}

fn playable_output_candidates(report: &DownloadReport) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for entry in &report.entries {
        if let Some(mux) = &entry.mux {
            candidates.push(mux.output_path.clone());
        }
    }
    for entry in &report.entries {
        for file in &entry.files {
            if matches!(
                &file.kind,
                DownloadFileKind::Video | DownloadFileKind::FlvSegment
            ) {
                candidates.push(file.path.clone());
            }
        }
    }
    candidates
}

async fn mux_download_report<F>(
    mut report: DownloadReport,
    ffmpeg_path: &Path,
    is_cancel_requested: &F,
) -> Result<DownloadReport, BilibiliDownloadError>
where
    F: Fn() -> bool,
{
    mux_download_report_in_place(&mut report, ffmpeg_path, is_cancel_requested).await?;
    Ok(report)
}

async fn mux_download_report_in_place<F>(
    report: &mut DownloadReport,
    ffmpeg_path: &Path,
    is_cancel_requested: &F,
) -> Result<(), BilibiliDownloadError>
where
    F: Fn() -> bool,
{
    for entry in &mut report.entries {
        if entry.mux.is_some() {
            continue;
        }
        if is_cancel_requested() {
            return Err(BilibiliDownloadError::Cancelled(
                "Cancelled before BBDown muxing started.".to_owned(),
            ));
        }
        if let Some(mux) = mux_entry_media(entry, ffmpeg_path, is_cancel_requested).await? {
            entry.mux = Some(mux);
        }
    }
    Ok(())
}

async fn mux_entry_media<F>(
    entry: &EntryDownloadReport,
    ffmpeg_path: &Path,
    is_cancel_requested: &F,
) -> Result<Option<MuxReport>, BilibiliDownloadError>
where
    F: Fn() -> bool,
{
    let media_files = mux_source_files(entry);
    if media_files.is_empty() {
        return Ok(None);
    }

    fs::create_dir_all(&entry.directory).await.map_err(failed)?;
    let output_path = playback_output_path(entry);
    let mux_output_path = temporary_mux_output_path(&output_path);
    remove_file_if_exists(&mux_output_path).await?;

    let mut args = Vec::new();
    args.push(OsString::from("-y"));
    args.push(OsString::from("-nostdin"));
    if only_flv_segments(entry) {
        let list_path = entry.directory.join("cache-server-ffmpeg-concat.txt");
        fs::write(&list_path, concat_file_list(&media_files))
            .await
            .map_err(failed)?;
        args.extend([
            OsString::from("-f"),
            OsString::from("concat"),
            OsString::from("-safe"),
            OsString::from("0"),
            OsString::from("-i"),
            list_path.into_os_string(),
        ]);
    } else {
        for media_file in &media_files {
            args.push(OsString::from("-i"));
            args.push(media_file.as_os_str().to_os_string());
        }
    }
    args.extend([
        OsString::from("-c"),
        OsString::from("copy"),
        OsString::from("-f"),
        OsString::from("mp4"),
        mux_output_path.as_os_str().to_os_string(),
    ]);

    let output = match run_ffmpeg_mux(ffmpeg_path, &args, is_cancel_requested).await {
        Ok(output) => output,
        Err(error) => {
            cleanup_failed_mux_files(&media_files, &output_path, &mux_output_path).await;
            return Err(error);
        }
    };
    if !output.status.success() {
        cleanup_failed_mux_files(&media_files, &output_path, &mux_output_path).await;
        return Err(BilibiliDownloadError::Failed(format!(
            "BBDown adapter ffmpeg mux failed with status {}: {}",
            output.status.code().map_or_else(
                || "terminated by signal".to_owned(),
                |code| code.to_string()
            ),
            stderr_tail(&output.stderr)
        )));
    }

    let metadata = match fs::metadata(&mux_output_path).await {
        Ok(metadata) => metadata,
        Err(error) => {
            cleanup_failed_mux_files(&media_files, &output_path, &mux_output_path).await;
            return Err(failed(error));
        }
    };
    if !metadata.is_file() || metadata.len() == 0 {
        cleanup_failed_mux_files(&media_files, &output_path, &mux_output_path).await;
        return Err(BilibiliDownloadError::Failed(
            "BBDown adapter ffmpeg mux produced no playable output.".to_owned(),
        ));
    }

    if let Err(error) = fs::rename(&mux_output_path, &output_path).await {
        if output_path.exists() {
            if let Err(error) = fs::remove_file(&output_path).await {
                cleanup_failed_mux_files(&media_files, &output_path, &mux_output_path).await;
                return Err(failed(error));
            }
            if let Err(error) = fs::rename(&mux_output_path, &output_path).await {
                cleanup_failed_mux_files(&media_files, &output_path, &mux_output_path).await;
                return Err(failed(error));
            }
        } else {
            cleanup_failed_mux_files(&media_files, &output_path, &mux_output_path).await;
            return Err(failed(error));
        }
    }
    cleanup_mux_source_files(&media_files, &output_path, &mux_output_path).await?;

    Ok(Some(MuxReport {
        output_path,
        command: command_report(ffmpeg_path, &args),
        chapter_count: 0,
    }))
}

fn mux_source_files(entry: &EntryDownloadReport) -> Vec<PathBuf> {
    entry
        .files
        .iter()
        .filter(|file| is_media_kind(&file.kind))
        .map(|file| file.path.clone())
        .collect()
}

async fn cleanup_downloaded_media_sources(report: &DownloadReport) {
    for entry in &report.entries {
        let media_files = mux_source_files(entry);
        let output_path = playback_output_path(entry);
        let mux_output_path = temporary_mux_output_path(&output_path);
        let _ = cleanup_mux_source_files(&media_files, &output_path, &mux_output_path).await;
    }
}

async fn cleanup_failed_mux_files(
    media_files: &[PathBuf],
    output_path: &Path,
    mux_output_path: &Path,
) {
    let _ = fs::remove_file(mux_output_path).await;
    let _ = cleanup_mux_source_files(media_files, output_path, mux_output_path).await;
}

async fn cleanup_mux_source_files(
    media_files: &[PathBuf],
    output_path: &Path,
    mux_output_path: &Path,
) -> Result<(), BilibiliDownloadError> {
    for media_file in media_files {
        if media_file == output_path || media_file == mux_output_path {
            continue;
        }
        remove_file_if_exists(media_file).await?;
    }
    Ok(())
}

struct FfmpegMuxOutput {
    status: ExitStatus,
    stderr: Vec<u8>,
}

async fn run_ffmpeg_mux<F>(
    ffmpeg_path: &Path,
    args: &[OsString],
    is_cancel_requested: &F,
) -> Result<FfmpegMuxOutput, BilibiliDownloadError>
where
    F: Fn() -> bool,
{
    if is_cancel_requested() {
        return Err(BilibiliDownloadError::Cancelled(
            "Cancelled before BBDown muxing started.".to_owned(),
        ));
    }

    let mut child = Command::new(ffmpeg_path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(failed)?;

    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| failed("ffmpeg stderr pipe was not captured"))?;
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.map(|_| bytes)
    });

    loop {
        if is_cancel_requested() {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = stderr_task.await;
            return Err(BilibiliDownloadError::Cancelled(
                "Cancelled while BBDown muxing was running.".to_owned(),
            ));
        }

        if let Some(status) = child.try_wait().map_err(failed)? {
            return Ok(FfmpegMuxOutput {
                status,
                stderr: collect_stderr(stderr_task).await?,
            });
        }

        sleep(Duration::from_millis(250)).await;
    }
}

async fn collect_stderr(
    stderr_task: tokio::task::JoinHandle<Result<Vec<u8>, std::io::Error>>,
) -> Result<Vec<u8>, BilibiliDownloadError> {
    match stderr_task.await {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(error)) => Err(failed(error)),
        Err(error) => Err(failed(error)),
    }
}

fn is_media_kind(kind: &DownloadFileKind) -> bool {
    matches!(
        kind,
        DownloadFileKind::Video | DownloadFileKind::Audio | DownloadFileKind::FlvSegment
    )
}

fn only_flv_segments(entry: &EntryDownloadReport) -> bool {
    entry
        .files
        .iter()
        .filter(|file| is_media_kind(&file.kind))
        .all(|file| file.kind == DownloadFileKind::FlvSegment)
}

const MAX_FILE_NAME_BYTES: usize = 255;
const PLAYBACK_EXTENSION: &str = ".mp4";
const MUX_TEMP_SUFFIX: &str = ".cache-server-mux-tmp";
const PLAYBACK_STEM_BYTE_BUDGET: usize =
    MAX_FILE_NAME_BYTES - 1 - PLAYBACK_EXTENSION.len() - MUX_TEMP_SUFFIX.len();

fn temporary_mux_output_path(output_path: &Path) -> PathBuf {
    let file_name = output_path
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_else(|| "cache-server-playback.mp4".into());
    output_path.with_file_name(format!(".{file_name}{MUX_TEMP_SUFFIX}"))
}

fn playback_output_path(entry: &EntryDownloadReport) -> PathBuf {
    entry
        .directory
        .join(format!("{}{PLAYBACK_EXTENSION}", safe_playback_stem(entry)))
}

fn safe_playback_stem(entry: &EntryDownloadReport) -> String {
    let cleaned = entry
        .title
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            character if character.is_control() => '_',
            character => character,
        })
        .collect::<String>();
    let trimmed = cleaned.trim_matches(|character: char| {
        character.is_whitespace() || character == '.' || character == '_'
    });
    if trimmed.is_empty() {
        format!("Entry {}", entry.index)
    } else {
        truncate_utf8_bytes(trimmed, PLAYBACK_STEM_BYTE_BUDGET)
    }
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> String {
    let mut output = String::new();
    let mut used_bytes = 0usize;
    for character in value.chars() {
        let character_bytes = character.len_utf8();
        if used_bytes + character_bytes > max_bytes {
            break;
        }
        output.push(character);
        used_bytes += character_bytes;
    }
    output
}

fn concat_file_list(media_files: &[PathBuf]) -> String {
    media_files
        .iter()
        .map(|path| {
            format!(
                "file '{}'\n",
                path.display().to_string().replace('\'', "'\\''")
            )
        })
        .collect()
}

fn command_report(ffmpeg_path: &Path, args: &[OsString]) -> Vec<String> {
    std::iter::once(ffmpeg_path.as_os_str().to_string_lossy().into_owned())
        .chain(args.iter().map(|arg| arg.to_string_lossy().into_owned()))
        .collect()
}

fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    let max_chars = 1200;
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_owned();
    }

    format!(
        "...{}",
        text.chars()
            .skip(char_count.saturating_sub(max_chars))
            .collect::<String>()
    )
}

async fn remove_file_if_exists(path: &Path) -> Result<(), BilibiliDownloadError> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(failed(error)),
    }
}

fn to_i64_saturating(value: u64) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}

fn nonnegative_i64_to_u64(value: i64) -> u64 {
    value.try_into().unwrap_or_default()
}

fn success_message(report: &DownloadReport) -> String {
    let entry_count = report.entries.len();
    if entry_count == 1 {
        "Downloaded 1 Bilibili entry into the cache library.".to_owned()
    } else {
        format!("Downloaded {entry_count} Bilibili entries into the cache library.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbdown_core::{
        DownloadArchiveRecord, DownloadedFile, EntryDownloadReport, MuxReport,
        RestrictedAreaProxyKind,
    };
    use std::fs as std_fs;

    const LEGACY_RESOLVE_CANDIDATE_LIMIT: usize = 100;

    fn v2_test_candidate() -> BilibiliTaskCandidateRecord {
        BilibiliTaskCandidateRecord {
            selection_id: "episode:33".to_owned(),
            title: "Episode 2".to_owned(),
            subtitle: "Season".to_owned(),
            source_kind: "season_episode".to_owned(),
            content_id: "33".to_owned(),
            identity: BilibiliContentIdentity {
                kind: BilibiliContentKind::SeasonEpisode,
                aid: Some(11),
                bvid: Some("BV1Test".to_owned()),
                cid: Some(22),
                epid: Some(33),
            },
            index: 2,
            duration_seconds: Some(90),
        }
    }

    fn v2_test_download_entry() -> bbdown_core::DownloadEntry {
        serde_json::from_value(serde_json::json!({
            "index": 2,
            "aid": 11,
            "bvid": "BV1Test",
            "cid": 22,
            "epid": 33,
            "title": "Episode 2",
            "source": "normal_web",
            "streams": {
                "videos": [],
                "audios": [],
                "flv_segments": [],
                "accept_quality": [],
                "duration_seconds": 90
            },
            "subtitles": [],
            "chapters": [{
                "title": "Opening",
                "start_seconds": 0,
                "end_seconds": 15
            }],
            "danmaku": {
                "cid": 22,
                "xml_url": "https://upstream.invalid/private/danmaku.xml"
            }
        }))
        .expect("test download entry should deserialize")
    }

    fn bilibili_options_with_download_mode(mode: BilibiliDownloadMode) -> BilibiliDownloadOptions {
        BilibiliDownloadOptions {
            quality_preference: String::new(),
            encoding_preference: String::new(),
            prefer_tv_api: false,
            download_subtitles: false,
            download_danmaku: false,
            audio_language: String::new(),
            subtitle_ai_policy: BilibiliSubtitleAiPolicy::Unspecified.into(),
            download_cover: false,
            danmaku_formats: Vec::new(),
            download_mode: mode.into(),
        }
    }

    fn encoded_message_contains<M: prost::Message>(message: &M, value: &str) -> bool {
        let encoded = message.encode_to_vec();
        encoded
            .windows(value.len())
            .any(|window| window == value.as_bytes())
    }

    fn assert_progress_near(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("progress should be set");
        assert!(
            (actual - expected).abs() < 0.000_001,
            "expected progress {expected}, got {actual}"
        );
    }

    fn accumulator_after_failed_bbdown_file_attempt()
    -> (BilibiliBbdownProgressAccumulator, BilibiliTaskProgress) {
        let mut accumulator = BilibiliBbdownProgressAccumulator::default();
        let path = PathBuf::from("out/entry/video.m4s");

        accumulator
            .record(&DownloadProgressEvent::PlanStarted {
                title: "Example".to_owned(),
                output_dir: PathBuf::from("out"),
                entry_count: 1,
            })
            .expect("plan start should report progress");
        accumulator
            .record(&DownloadProgressEvent::EntryStarted {
                index: 1,
                title: "Entry".to_owned(),
                directory: PathBuf::from("out/entry"),
            })
            .expect("entry start should report progress");
        accumulator
            .record(&DownloadProgressEvent::FileStarted {
                entry_index: 1,
                entry_title: "Entry".to_owned(),
                kind: DownloadFileKind::Video,
                path: path.clone(),
                resumed_from: 0,
                expected_size: Some(100),
                attempt: 1,
                max_attempts: 2,
            })
            .expect("file start should report progress");
        let file_progress = accumulator
            .record(&DownloadProgressEvent::FileProgress {
                entry_index: 1,
                entry_title: "Entry".to_owned(),
                kind: DownloadFileKind::Video,
                path: path.clone(),
                bytes_delta: 50,
                bytes_written: 50,
                resumed_from: 0,
                expected_size: Some(100),
            })
            .expect("file progress should report progress");
        assert_progress_near(file_progress.progress, 0.45);
        assert_eq!(Some(0), file_progress.downloaded_bytes);
        assert_eq!(Some(0), file_progress.total_bytes);

        let failed_progress = accumulator
            .record(&DownloadProgressEvent::FileFailed {
                entry_index: 1,
                entry_title: "Entry".to_owned(),
                kind: DownloadFileKind::Video,
                path,
                attempt: 1,
                max_attempts: 2,
                error: "stream reset".to_owned(),
            })
            .expect("file failure should publish rolled-back byte progress");
        assert_progress_near(failed_progress.progress, DOWNLOAD_PROGRESS_START);
        assert_eq!(Some(0), failed_progress.downloaded_bytes);
        assert_eq!(Some(0), failed_progress.total_bytes);

        (accumulator, failed_progress)
    }

    #[test]
    fn maps_common_video_quality_preferences() {
        assert_eq!(video_quality_preference("1080p"), Some(80));
        assert_eq!(video_quality_preference("1080p60"), Some(116));
        assert_eq!(video_quality_preference("4k"), Some(120));
        assert_eq!(video_quality_preference("80"), Some(80));
        assert_eq!(video_quality_preference("best"), None);
    }

    #[test]
    fn accepts_supported_download_options() {
        let options = BilibiliDownloadOptions {
            quality_preference: "1080p".to_owned(),
            encoding_preference: String::new(),
            prefer_tv_api: false,
            download_subtitles: true,
            download_danmaku: false,
            audio_language: String::new(),
            subtitle_ai_policy: BilibiliSubtitleAiPolicy::Unspecified.into(),
            download_cover: false,
            danmaku_formats: Vec::new(),
            download_mode: 0,
        };

        assert!(validate_supported_download_options(Some(&options)).is_ok());
    }

    #[test]
    fn maps_extended_download_options_to_bbdown_core() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let options = BilibiliDownloadOptions {
            quality_preference: "1080p".to_owned(),
            encoding_preference: String::new(),
            prefer_tv_api: false,
            download_subtitles: true,
            download_danmaku: true,
            audio_language: "ja-JP".to_owned(),
            subtitle_ai_policy: BilibiliSubtitleAiPolicy::ExcludeAi.into(),
            download_cover: true,
            danmaku_formats: vec![
                BilibiliDanmakuFormat::Ass.into(),
                BilibiliDanmakuFormat::Xml.into(),
            ],
            download_mode: 0,
        };

        let download_options =
            download_options_for_output_dir(temp.path().join("bbdown-output"), Some(&options))
                .expect("extended options should be supported");

        assert_eq!(download_options.stream_selection.video_quality, Some(80));
        assert_eq!(
            download_options.stream_selection.audio_language.as_deref(),
            Some("ja-JP")
        );
        assert!(download_options.include_subtitles);
        assert_eq!(
            download_options.subtitle_ai_policy,
            SubtitleAiPolicy::ExcludeAi
        );
        assert!(download_options.include_danmaku);
        assert!(download_options.sidecars.cover);
        assert!(download_options.sidecars.subtitles);
        assert!(download_options.sidecars.danmaku);
        assert_eq!(
            download_options.danmaku_formats.as_slice(),
            &[DanmakuFormat::Xml, DanmakuFormat::Ass]
        );
    }

    #[test]
    fn pr6d_maps_all_v2_download_modes_to_bbdown_core() {
        let cases = [
            (BilibiliDownloadMode::Unspecified, DownloadMode::All),
            (BilibiliDownloadMode::All, DownloadMode::All),
            (BilibiliDownloadMode::VideoOnly, DownloadMode::VideoOnly),
            (BilibiliDownloadMode::AudioOnly, DownloadMode::AudioOnly),
            (
                BilibiliDownloadMode::SubtitleOnly,
                DownloadMode::SubtitleOnly,
            ),
            (BilibiliDownloadMode::DanmakuOnly, DownloadMode::DanmakuOnly),
            (BilibiliDownloadMode::CoverOnly, DownloadMode::CoverOnly),
        ];
        let temp = tempfile::tempdir().expect("temp dir should be created");

        assert_eq!(download_mode_from_options(None).unwrap(), DownloadMode::All);
        for (proto_mode, core_mode) in cases {
            let options = bilibili_options_with_download_mode(proto_mode);
            assert_eq!(
                download_mode_from_options(Some(&options)).unwrap(),
                core_mode
            );
            assert_eq!(
                download_options_for_output_dir(temp.path().to_path_buf(), Some(&options))
                    .unwrap()
                    .mode,
                core_mode
            );
        }

        let mut unknown = bilibili_options_with_download_mode(BilibiliDownloadMode::All);
        unknown.download_mode = i32::MAX;
        assert!(matches!(
            download_mode_from_options(Some(&unknown)),
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("download mode is unknown")
        ));
    }

    #[test]
    fn rejects_unsupported_encoding_preference() {
        let options = BilibiliDownloadOptions {
            quality_preference: String::new(),
            encoding_preference: "hevc".to_owned(),
            prefer_tv_api: false,
            download_subtitles: false,
            download_danmaku: false,
            audio_language: String::new(),
            subtitle_ai_policy: BilibiliSubtitleAiPolicy::Unspecified.into(),
            download_cover: false,
            danmaku_formats: Vec::new(),
            download_mode: 0,
        };

        let result = validate_supported_download_options(Some(&options));
        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("does not support encoding_preference")
        ));
    }

    #[test]
    fn accepts_tv_api_preference() {
        let options = BilibiliDownloadOptions {
            quality_preference: String::new(),
            encoding_preference: String::new(),
            prefer_tv_api: true,
            download_subtitles: false,
            download_danmaku: false,
            audio_language: String::new(),
            subtitle_ai_policy: BilibiliSubtitleAiPolicy::Unspecified.into(),
            download_cover: false,
            danmaku_formats: Vec::new(),
            download_mode: 0,
        };

        let result = validate_supported_download_options(Some(&options));
        assert!(result.is_ok());
    }

    #[test]
    fn rejects_subtitle_ai_policy_without_subtitles() {
        let options = BilibiliDownloadOptions {
            quality_preference: String::new(),
            encoding_preference: String::new(),
            prefer_tv_api: false,
            download_subtitles: false,
            download_danmaku: false,
            audio_language: String::new(),
            subtitle_ai_policy: BilibiliSubtitleAiPolicy::OnlyAi.into(),
            download_cover: false,
            danmaku_formats: Vec::new(),
            download_mode: 0,
        };

        let result = validate_supported_download_options(Some(&options));
        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("subtitle_ai_policy requires download_subtitles")
        ));
    }

    #[test]
    fn rejects_danmaku_formats_without_danmaku() {
        let options = BilibiliDownloadOptions {
            quality_preference: String::new(),
            encoding_preference: String::new(),
            prefer_tv_api: false,
            download_subtitles: false,
            download_danmaku: false,
            audio_language: String::new(),
            subtitle_ai_policy: BilibiliSubtitleAiPolicy::Unspecified.into(),
            download_cover: false,
            danmaku_formats: vec![BilibiliDanmakuFormat::Ass.into()],
            download_mode: 0,
        };

        let result = validate_supported_download_options(Some(&options));
        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("danmaku_formats requires download_danmaku")
        ));
    }

    #[test]
    fn builds_bbdown_client_config_from_cache_server_options() {
        let temp = tempfile::tempdir().unwrap();
        let credentials_path = temp.path().join("credentials.json");
        std_fs::write(
            &credentials_path,
            r#"{"cookie":"SESSDATA=secret","access_key":"access-token","tv_access_key":"tv-token"}"#,
        )
        .unwrap();
        let options = CacheServerOptions::from_args([
            "--Cache:BBDownCredentialPath".to_owned(),
            credentials_path.display().to_string(),
            "--Cache:BBDownRestrictedArea".to_owned(),
            "hk".to_owned(),
            "--Cache:BBDownRestrictedAreaProxy".to_owned(),
            "hk=https://play.example/proxy".to_owned(),
            "--Cache:BBDownRestrictedApiProxy".to_owned(),
            "https://api.example/proxy".to_owned(),
        ])
        .expect("options should parse");

        let config =
            bbdown_client_config(&options, PlayurlMode::Tv).expect("client config should build");

        assert_eq!(PlayurlMode::Tv, config.playurl_mode);
        assert_eq!(
            Some("SESSDATA=secret"),
            config.credentials.cookie.as_deref()
        );
        assert_eq!(
            Some("access-token"),
            config.credentials.access_key.as_deref()
        );
        assert_eq!(
            Some("tv-token"),
            config.credentials.tv_access_key.as_deref()
        );
        assert_eq!(Some(RestrictedArea::Hk), config.restricted_area.area_hint);
        assert_eq!(2, config.restricted_area.proxies.len());
        assert_eq!(
            RestrictedAreaProxyKind::PlayUrl,
            config.restricted_area.proxies[0].kind
        );
        assert_eq!(
            Some(RestrictedArea::Hk),
            config.restricted_area.proxies[0].area
        );
        assert_eq!(
            "https://play.example/proxy",
            config.restricted_area.proxies[0].base_url
        );
        assert_eq!(
            RestrictedAreaProxyKind::BilibiliApi,
            config.restricted_area.proxies[1].kind
        );
        assert_eq!(None, config.restricted_area.proxies[1].area);
        assert_eq!(
            "https://api.example/proxy",
            config.restricted_area.proxies[1].base_url
        );
    }

    #[test]
    fn builds_bbdown_client_config_from_selected_credential_profile() {
        let temp = tempfile::tempdir().unwrap();
        let credentials_path = temp.path().join("credentials.json");
        std_fs::write(
            &credentials_path,
            r#"{
                "version": 1,
                "default_profile": "default",
                "profiles": {
                    "default": {
                        "cookie": "SESSDATA=default",
                        "access_key": "default-access"
                    },
                    "living-room": {
                        "cookie": "SESSDATA=living-room",
                        "access_key": "living-access",
                        "tv_access_key": "living-tv"
                    }
                }
            }"#,
        )
        .unwrap();
        let options = CacheServerOptions::from_args([
            "--Cache:BBDownCredentialPath".to_owned(),
            credentials_path.display().to_string(),
            "--Cache:BBDownCredentialProfile".to_owned(),
            "living-room".to_owned(),
        ])
        .expect("options should parse");

        let config =
            bbdown_client_config(&options, PlayurlMode::Web).expect("client config should build");

        assert_eq!(
            Some("SESSDATA=living-room"),
            config.credentials.cookie.as_deref()
        );
        assert_eq!(
            Some("living-access"),
            config.credentials.access_key.as_deref()
        );
        assert_eq!(
            Some("living-tv"),
            config.credentials.tv_access_key.as_deref()
        );
    }

    #[test]
    fn pr6d_request_client_config_honors_explicit_api_modes() {
        let server_options = CacheServerOptions::default();
        let mut legacy_options = bilibili_options_with_download_mode(BilibiliDownloadMode::All);
        legacy_options.prefer_tv_api = true;

        for (api_mode, expected_mode) in [
            (BilibiliApiMode::Web, PlayurlMode::Web),
            (BilibiliApiMode::Tv, PlayurlMode::Tv),
            (BilibiliApiMode::App, PlayurlMode::App),
        ] {
            let context = BilibiliRequestContext {
                api_mode: api_mode.into(),
                credential_profile_id: String::new(),
            };
            let config = bbdown_client_config_for_request(
                &server_options,
                Some(&legacy_options),
                Some(&context),
            )
            .expect("explicit API mode should be accepted")
            .expect("explicit API mode should build a request client");
            assert_eq!(config.playurl_mode, expected_mode);
        }

        let legacy_context = BilibiliRequestContext::default();
        let frozen_context_config = bbdown_client_config_for_request(
            &server_options,
            Some(&legacy_options),
            Some(&legacy_context),
        )
        .unwrap()
        .expect("a persisted request context should build an isolated client");
        assert_eq!(PlayurlMode::Tv, frozen_context_config.playurl_mode);
        assert_eq!(Credentials::default(), frozen_context_config.credentials);

        let invalid_context = BilibiliRequestContext {
            api_mode: i32::MAX,
            credential_profile_id: String::new(),
        };
        assert!(matches!(
            bbdown_client_config_for_request(
                &server_options,
                Some(&legacy_options),
                Some(&invalid_context),
            ),
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("API mode is unknown")
        ));
    }

    #[test]
    fn pr6d_empty_frozen_profile_does_not_adopt_a_later_server_default() {
        let temp = tempfile::tempdir().unwrap();
        let credentials_path = temp.path().join("credentials.json");
        std_fs::write(
            &credentials_path,
            r#"{
                "version": 1,
                "default_profile": "default",
                "profiles": {
                    "default": {
                        "cookie": "SESSDATA=default"
                    }
                }
            }"#,
        )
        .unwrap();
        let server_options = CacheServerOptions {
            bbdown_credential_path: Some(credentials_path),
            ..CacheServerOptions::default()
        };
        let context = BilibiliRequestContext {
            api_mode: BilibiliApiMode::Web.into(),
            credential_profile_id: String::new(),
        };

        let config = bbdown_client_config_for_request(&server_options, None, Some(&context))
            .expect("frozen no-profile context should be valid")
            .expect("an explicit API mode should build a request client");

        assert_eq!(PlayurlMode::Web, config.playurl_mode);
        assert_eq!(Credentials::default(), config.credentials);
    }

    #[test]
    fn pr6d_profile_only_request_preserves_legacy_tv_mode() {
        let temp = tempfile::tempdir().unwrap();
        let credentials_path = temp.path().join("credentials.json");
        std_fs::write(
            &credentials_path,
            r#"{
                "version": 1,
                "default_profile": "default",
                "profiles": {
                    "default": {
                        "cookie": "SESSDATA=default"
                    },
                    "living-room": {
                        "cookie": "SESSDATA=living-room",
                        "access_key": "living-access",
                        "tv_access_key": "living-tv"
                    }
                }
            }"#,
        )
        .unwrap();
        let server_options = CacheServerOptions {
            bbdown_credential_path: Some(credentials_path),
            ..CacheServerOptions::default()
        };
        let mut legacy_options = bilibili_options_with_download_mode(BilibiliDownloadMode::All);
        legacy_options.prefer_tv_api = true;
        let profile_only_context = BilibiliRequestContext {
            api_mode: BilibiliApiMode::Unspecified.into(),
            credential_profile_id: "  living-room  ".to_owned(),
        };

        let config = bbdown_client_config_for_request(
            &server_options,
            Some(&legacy_options),
            Some(&profile_only_context),
        )
        .expect("profile-only request should build a client")
        .expect("explicit profile should require a request client");

        assert_eq!(config.playurl_mode, PlayurlMode::Tv);
        assert_eq!(
            config.credentials.cookie.as_deref(),
            Some("SESSDATA=living-room")
        );
        assert_eq!(
            config.credentials.tv_access_key.as_deref(),
            Some("living-tv")
        );

        let explicit_app_context = BilibiliRequestContext {
            api_mode: BilibiliApiMode::App.into(),
            credential_profile_id: "living-room".to_owned(),
        };
        let app_config = bbdown_client_config_for_request(
            &server_options,
            Some(&legacy_options),
            Some(&explicit_app_context),
        )
        .unwrap()
        .unwrap();
        assert_eq!(app_config.playurl_mode, PlayurlMode::App);
        assert_eq!(
            app_config.credentials.access_key.as_deref(),
            Some("living-access")
        );
    }

    #[test]
    fn pr6d_accepted_candidate_filter_requires_exact_identity() {
        let entry = v2_test_download_entry();
        let candidate = v2_test_candidate();
        assert!(candidate.identity.is_complete());
        assert!(download_entry_matches_candidate(&entry, &candidate));

        let mut case_insensitive_bvid = candidate.clone();
        case_insensitive_bvid.identity.bvid = Some("bv1test".to_owned());
        assert!(download_entry_matches_candidate(
            &entry,
            &case_insensitive_bvid
        ));

        let mut mismatches = Vec::new();
        for mutate in [
            |identity: &mut BilibiliContentIdentity| identity.aid = Some(12),
            |identity: &mut BilibiliContentIdentity| identity.bvid = Some("BV1Other".to_owned()),
            |identity: &mut BilibiliContentIdentity| identity.cid = Some(23),
            |identity: &mut BilibiliContentIdentity| identity.epid = Some(34),
        ] {
            let mut mismatch = candidate.clone();
            mutate(&mut mismatch.identity);
            mismatches.push(mismatch);
        }
        assert!(
            mismatches
                .iter()
                .all(|candidate| !download_entry_matches_candidate(&entry, candidate))
        );

        let mut missing_bvid = entry.clone();
        missing_bvid.bvid = None;
        assert!(!download_entry_matches_candidate(&missing_bvid, &candidate));
    }

    #[test]
    fn pr6d_v2_result_maps_generic_subject_and_bilibili_identity() {
        use crate::generated::tvos_net_player::v1::{
            BilibiliContentKind as ProtoBilibiliContentKind, task_result_provider_details,
        };

        let mut candidate = v2_test_candidate();
        candidate.selection_id = "https://upstream.invalid/private/selection".to_owned();
        let result = successful_download_result(
            "task-one".to_owned(),
            &candidate,
            "library-one".to_owned(),
            Vec::new(),
            42,
        );

        let subject = result
            .subject
            .as_ref()
            .expect("result subject should exist");
        assert_eq!(subject.provider, "bilibili");
        assert_eq!(subject.kind, "season_episode");
        assert_eq!(subject.id, "33");
        assert_eq!(subject.index, 2);

        let details = result
            .provider_details
            .as_ref()
            .and_then(|details| details.details.as_ref())
            .expect("provider details should exist");
        let task_result_provider_details::Details::Bilibili(details) = details;
        let identity = details
            .identity
            .as_ref()
            .expect("Bilibili identity should exist");
        assert_eq!(
            identity.kind,
            ProtoBilibiliContentKind::SeasonEpisode as i32
        );
        assert_eq!(identity.aid, 11);
        assert_eq!(identity.bvid, "BV1Test");
        assert_eq!(identity.cid, 22);
        assert_eq!(identity.epid, 33);
        assert!(details.playback_session.is_none());
        assert!(!encoded_message_contains(
            &result,
            "https://upstream.invalid/private/selection"
        ));
    }

    #[test]
    fn pr6d_v2_multi_result_helpers_keep_stable_ids_progress_and_partial_states() {
        let candidate = v2_test_candidate();
        assert_eq!(bilibili_v2_result_id("task", 0), "task");
        assert_eq!(bilibili_v2_result_id("task", 1), "task-result-2");
        assert_eq!(bilibili_v2_result_id("task", 9), "task-result-10");
        assert_eq!(v2_candidate_progress(0, 2, 0.5), 0.25);
        assert_eq!(v2_candidate_progress(1, 2, 0.5), 0.75);
        assert_eq!(v2_candidate_progress(2, 0, 1.0), 0.0);

        let results = [
            successful_download_result(
                bilibili_v2_result_id("task", 0),
                &candidate,
                "library-one".to_owned(),
                Vec::new(),
                100,
            ),
            failed_download_result(
                bilibili_v2_result_id("task", 1),
                &candidate,
                &BilibiliDownloadError::Failed("offline failure".to_owned()),
            ),
            cancelled_download_result(bilibili_v2_result_id("task", 2), &candidate),
        ];

        assert_eq!(results[0].state(), TaskState::Succeeded);
        assert_eq!(results[1].state(), TaskState::Failed);
        assert_eq!(results[2].state(), TaskState::Cancelled);
        assert_eq!(results[0].id, "task");
        assert_eq!(results[1].id, "task-result-2");
        assert_eq!(results[2].id, "task-result-3");
        assert!(results.iter().all(|result| result.subject.is_some()));
        assert!(
            results
                .iter()
                .all(|result| result.provider_details.is_some())
        );
        assert_eq!(
            "The Bilibili download failed.",
            results[1]
                .problem
                .as_ref()
                .expect("failed result should include a problem")
                .message
        );
        assert!(!encoded_message_contains(&results[1], "offline failure"));
    }

    #[test]
    fn v2_candidate_progress_scales_fraction_and_preserves_aggregate_bytes() {
        let mut first = BilibiliTaskProgress {
            progress: Some(DOWNLOAD_PROGRESS_END),
            downloaded_bytes: Some(100),
            total_bytes: Some(120),
            message: None,
        };
        BilibiliV2ProgressWindow {
            offset: 0,
            total: 2,
            completed_downloaded_bytes: 0,
            total_bytes_floor: 0,
        }
        .map(&mut first);
        assert_progress_near(first.progress, 0.40);
        assert_eq!(Some(100), first.downloaded_bytes);
        assert_eq!(Some(120), first.total_bytes);

        let mut second = BilibiliTaskProgress {
            progress: Some(DOWNLOAD_PROGRESS_START),
            downloaded_bytes: Some(0),
            total_bytes: Some(0),
            message: None,
        };
        BilibiliV2ProgressWindow {
            offset: 1,
            total: 2,
            completed_downloaded_bytes: 100,
            total_bytes_floor: 120,
        }
        .map(&mut second);
        assert_progress_near(second.progress, 0.55);
        assert_eq!(Some(100), second.downloaded_bytes);
        assert_eq!(Some(120), second.total_bytes);
    }

    #[test]
    fn v2_task_archive_only_accepts_successful_candidate_state() {
        let archive_record = |content_key: &str| DownloadArchiveRecord {
            content_key: content_key.to_owned(),
            title: content_key.to_owned(),
            output_dir: PathBuf::from(content_key),
            completed_at_unix: 1,
            entries: Vec::new(),
        };
        let mut task_archive = V2TaskArchive::default();

        let mut rejected_candidate = task_archive.stage_candidate();
        rejected_candidate.records.push(archive_record("rejected"));
        drop(rejected_candidate);
        assert!(task_archive.stage_candidate().records.is_empty());

        let mut accepted_candidate = task_archive.stage_candidate();
        accepted_candidate.records.push(archive_record("accepted"));
        task_archive.accept_candidate(accepted_candidate);
        assert_eq!(
            vec!["accepted"],
            task_archive
                .stage_candidate()
                .records
                .iter()
                .map(|record| record.content_key.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn v2_transient_outputs_exclude_the_retained_library_media() {
        let media_path = PathBuf::from("library/video.mp4");
        let sidecar_path = PathBuf::from("output/video.srt");
        let output_paths = vec![media_path.clone(), sidecar_path.clone()];

        assert_eq!(
            vec![sidecar_path],
            transient_download_output_paths(&output_paths, Some(&media_path))
        );
        assert_eq!(
            output_paths,
            transient_download_output_paths(&output_paths, None)
        );
    }

    #[test]
    fn v2_retained_backing_preserves_all_successes() {
        let candidate = v2_test_candidate();
        let mut results = Vec::new();
        let mut retained_backing = RetainedV2DownloadBacking::default();
        let mut primary_library_item_id = String::new();
        let mut successful_results = 0;

        for (offset, library_item_id) in ["library-one", "library-two"].into_iter().enumerate() {
            retain_v2_success(
                MappedV2DownloadResult {
                    result: successful_download_result(
                        bilibili_v2_result_id("task", offset),
                        &candidate,
                        library_item_id.to_owned(),
                        Vec::new(),
                        100,
                    ),
                    library_item_id: library_item_id.to_owned(),
                    resources: Vec::new(),
                    resource_bodies: Vec::new(),
                    library_item_lease: None,
                    unpublished_output_paths: Vec::new(),
                    transient_output_paths: Vec::new(),
                },
                &mut primary_library_item_id,
                &mut successful_results,
                &mut results,
                &mut retained_backing,
            );
        }

        assert_eq!(2, successful_results);
        assert_eq!(2, results.len());
        assert!(
            results
                .iter()
                .all(|result| result.state() == TaskState::Succeeded)
        );
        assert_eq!("library-one", primary_library_item_id);
    }

    #[tokio::test]
    async fn pr6d_v2_artifact_resources_do_not_publish_local_or_upstream_paths() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let sidecar_path = temp.path().join("private-local-marker.subtitle.srt");
        std_fs::write(&sidecar_path, b"subtitle").expect("sidecar should be written");
        let server_options = Arc::new(CacheServerOptions {
            root_path: temp.path().join("library"),
            bbdown_output_dir: Some(temp.path().join("bbdown-output")),
            bbdown_archive_path: Some(temp.path().join("bbdown-archive.json")),
            ..CacheServerOptions::default()
        });
        let adapter = BbdownBilibiliAdapter::new(
            server_options.clone(),
            Arc::new(LocalMediaLibrary::new(server_options)),
        );
        let mut candidate = v2_test_candidate();
        candidate.selection_id = "https://upstream.invalid/private/selection".to_owned();
        let plan = DownloadPlan {
            title: "Season".to_owned(),
            entries: vec![v2_test_download_entry()],
        };
        let report = DownloadReport {
            title: "Season".to_owned(),
            output_dir: temp.path().join("private-output-marker"),
            entries: vec![EntryDownloadReport {
                index: 2,
                title: "Episode 2".to_owned(),
                directory: temp.path().join("private-entry-marker"),
                files: vec![DownloadedFile {
                    kind: DownloadFileKind::Subtitle,
                    path: sidecar_path.clone(),
                    bytes_written: 8,
                    resumed_from: 0,
                }],
                mux: None,
            }],
        };

        let mapped = adapter
            .map_v2_download_result(
                "task-one".to_owned(),
                &candidate,
                &plan,
                &report,
                DownloadMode::SubtitleOnly,
            )
            .await
            .expect("sidecar-only result should map without media or network access");

        assert!(mapped.library_item_id.is_empty());
        assert_eq!(vec![sidecar_path.clone()], mapped.unpublished_output_paths);
        assert_eq!(vec![sidecar_path.clone()], mapped.transient_output_paths);
        assert_eq!(mapped.result.state(), TaskState::Succeeded);
        assert_eq!(mapped.result.artifacts.len(), 3);
        assert_eq!(mapped.resources.len(), 3);
        assert_eq!(mapped.resource_bodies.len(), 3);
        let private_values = [
            temp.path().to_string_lossy().into_owned(),
            "private-local-marker".to_owned(),
            "https://upstream.invalid/private/selection".to_owned(),
            "https://upstream.invalid/private/danmaku.xml".to_owned(),
        ];
        for private_value in &private_values {
            assert!(!encoded_message_contains(&mapped.result, private_value));
            assert!(
                mapped
                    .resources
                    .iter()
                    .all(|resource| !encoded_message_contains(&resource.resource, private_value))
            );
        }
        assert!(mapped.resources.iter().all(|resource| {
            resource
                .resource
                .uri
                .starts_with("/resources/task-resource-")
        }));

        let mut cache_path_bodies = 0;
        let mut metadata_body_found = false;
        for body in &mapped.resource_bodies {
            match &body.source {
                BilibiliTaskResourceBodySource::CachePath(path) => {
                    cache_path_bodies += 1;
                    assert_eq!(path, &sidecar_path);
                }
                BilibiliTaskResourceBodySource::Bytes(bytes) => {
                    let text = std::str::from_utf8(bytes).expect("generated JSON should be UTF-8");
                    assert!(private_values.iter().all(|value| !text.contains(value)));
                    let json: serde_json::Value =
                        serde_json::from_slice(bytes).expect("generated body should be JSON");
                    metadata_body_found |=
                        json.get("provider").and_then(|value| value.as_str()) == Some("bilibili");
                }
            }
        }
        assert_eq!(cache_path_bodies, 1);
        assert!(metadata_body_found);

        let media_entry = EntryDownloadReport {
            index: 2,
            title: "Episode 2".to_owned(),
            directory: temp.path().join("private-media-entry"),
            files: Vec::new(),
            mux: Some(MuxReport {
                output_path: temp.path().join("private-media-marker.mp4"),
                command: vec!["https://upstream.invalid/private/media".to_owned()],
                chapter_count: 0,
            }),
        };
        let media_artifact = library_media_artifact(&media_entry, "library-media-one");
        assert_eq!(media_artifact.format, "mp4");
        assert_eq!(media_artifact.library_item_id, "library-media-one");
        assert!(media_artifact.resource.is_none());
        assert!(!encoded_message_contains(
            &media_artifact,
            "private-media-marker"
        ));
        assert!(!encoded_message_contains(
            &media_artifact,
            "https://upstream.invalid/private/media"
        ));
    }

    #[tokio::test]
    async fn v2_sidecar_only_modes_require_the_requested_artifact() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let server_options = Arc::new(CacheServerOptions {
            root_path: temp.path().join("library"),
            bbdown_output_dir: Some(temp.path().join("bbdown-output")),
            bbdown_archive_path: Some(temp.path().join("bbdown-archive.json")),
            ..CacheServerOptions::default()
        });
        let adapter = BbdownBilibiliAdapter::new(
            server_options.clone(),
            Arc::new(LocalMediaLibrary::new(server_options)),
        );
        let candidate = v2_test_candidate();
        let plan = DownloadPlan {
            title: "Season".to_owned(),
            entries: vec![v2_test_download_entry()],
        };

        for (offset, mode) in [
            DownloadMode::SubtitleOnly,
            DownloadMode::DanmakuOnly,
            DownloadMode::CoverOnly,
        ]
        .into_iter()
        .enumerate()
        {
            let report = DownloadReport {
                title: "Season".to_owned(),
                output_dir: temp.path().join(format!("empty-sidecar-{offset}")),
                entries: vec![EntryDownloadReport {
                    index: 2,
                    title: "Episode 2".to_owned(),
                    directory: temp.path().join(format!("empty-entry-{offset}")),
                    files: Vec::new(),
                    mux: None,
                }],
            };

            let error = match adapter
                .map_v2_download_result(
                    format!("task-sidecar-{offset}"),
                    &candidate,
                    &plan,
                    &report,
                    mode,
                )
                .await
            {
                Ok(_) => panic!("an empty sidecar-only result must fail"),
                Err(error) => error,
            };
            assert!(
                matches!(error, BilibiliDownloadError::Failed(message) if message.contains("requested sidecar artifact"))
            );
        }
    }

    #[tokio::test]
    async fn v2_finalization_removes_outputs_after_cancellation_or_mapping_failure() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let server_options = Arc::new(CacheServerOptions {
            root_path: temp.path().join("library"),
            bbdown_output_dir: Some(temp.path().join("bbdown-output")),
            bbdown_archive_path: Some(temp.path().join("bbdown-archive.json")),
            ..CacheServerOptions::default()
        });
        let adapter = BbdownBilibiliAdapter::new(
            server_options.clone(),
            Arc::new(LocalMediaLibrary::new(server_options)),
        );
        let candidate = v2_test_candidate();
        let plan = DownloadPlan {
            title: "Season".to_owned(),
            entries: vec![v2_test_download_entry()],
        };

        let cancelled_media = temp.path().join("cancelled.mp4");
        let cancelled_sidecar = temp.path().join("cancelled.srt");
        std_fs::write(&cancelled_media, b"media").expect("media should be written");
        std_fs::write(&cancelled_sidecar, b"subtitle").expect("sidecar should be written");
        let cancelled_report = DownloadReport {
            title: "Season".to_owned(),
            output_dir: temp.path().join("cancelled-output"),
            entries: vec![EntryDownloadReport {
                index: 2,
                title: "Episode 2".to_owned(),
                directory: temp.path().join("cancelled-entry"),
                files: vec![DownloadedFile {
                    kind: DownloadFileKind::Subtitle,
                    path: cancelled_sidecar.clone(),
                    bytes_written: 8,
                    resumed_from: 0,
                }],
                mux: Some(MuxReport {
                    output_path: cancelled_media.clone(),
                    command: Vec::new(),
                    chapter_count: 0,
                }),
            }],
        };
        let cancelled = adapter
            .finalize_v2_download_result(
                "task-cancelled".to_owned(),
                &candidate,
                &plan,
                cancelled_report,
                DownloadMode::All,
                true,
            )
            .await;
        assert!(matches!(
            cancelled,
            Err(BilibiliDownloadError::Cancelled(_))
        ));
        assert!(!cancelled_media.exists());
        assert!(!cancelled_sidecar.exists());

        let failed_sidecar = temp.path().join("mapping-failed.srt");
        std_fs::write(&failed_sidecar, b"subtitle").expect("sidecar should be written");
        let failed_report = DownloadReport {
            title: "Season".to_owned(),
            output_dir: temp.path().join("failed-output"),
            entries: vec![EntryDownloadReport {
                index: 2,
                title: "Episode 2".to_owned(),
                directory: temp.path().join("failed-entry"),
                files: vec![DownloadedFile {
                    kind: DownloadFileKind::Subtitle,
                    path: failed_sidecar.clone(),
                    bytes_written: 8,
                    resumed_from: 0,
                }],
                mux: None,
            }],
        };
        let failed = adapter
            .finalize_v2_download_result(
                "task-failed".to_owned(),
                &candidate,
                &plan,
                failed_report,
                DownloadMode::VideoOnly,
                false,
            )
            .await;
        assert!(matches!(failed, Err(BilibiliDownloadError::Failed(_))));
        assert!(
            !failed_sidecar.exists(),
            "mapping failures must remove unpublished BBDown files"
        );
    }

    #[test]
    fn rejects_selected_credential_profile_removed_after_startup() {
        let temp = tempfile::tempdir().unwrap();
        let credentials_path = temp.path().join("credentials.json");
        std_fs::write(
            &credentials_path,
            r#"{
                "version": 1,
                "default_profile": "default",
                "profiles": {
                    "default": {
                        "cookie": "SESSDATA=default"
                    }
                }
            }"#,
        )
        .unwrap();
        let options = CacheServerOptions {
            bbdown_credential_path: Some(credentials_path),
            bbdown_credential_profile: Some("living-room".to_owned()),
            ..CacheServerOptions::default()
        };

        let result = bbdown_client_config(&options, PlayurlMode::Web);

        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("living-room") && message.contains("does not exist")
        ));
    }

    #[test]
    fn uses_latest_selection_for_collection_inputs() {
        assert_eq!(
            default_selection_for_input(&Input::Season(1)),
            Some(Selection::Latest)
        );
        assert_eq!(
            default_selection_for_input(&Input::Media(1)),
            Some(Selection::Latest)
        );
        assert_eq!(
            default_selection_for_input(&Input::Bvid("BV1qt4y1X7TW".to_owned())),
            Some(Selection::Current)
        );
        assert_eq!(
            default_selection_for_input(&Input::CheeseSeason(202)),
            Some(Selection::Latest)
        );
        assert_eq!(
            default_selection_for_input(&Input::FavoriteList {
                media_id: Some(456),
                owner_mid: None,
            }),
            Some(Selection::Latest)
        );
        assert_eq!(
            default_selection_for_input(&Input::SpaceVideos(123)),
            Some(Selection::Latest)
        );
        assert_eq!(
            default_selection_for_input(&Input::CollectionList(456)),
            Some(Selection::Latest)
        );
        assert_eq!(
            default_selection_for_input(&Input::SeriesList(789)),
            Some(Selection::Latest)
        );
        assert_eq!(
            default_selection_for_input(&Input::SpaceCollectionList {
                list_id: 456,
                owner_mid: 123,
            }),
            Some(Selection::Latest)
        );
        assert_eq!(
            default_selection_for_input(&Input::SpaceSeriesList {
                list_id: 789,
                owner_mid: 123,
            }),
            Some(Selection::Latest)
        );
        assert_eq!(
            default_selection_for_input(&Input::CheeseEpisode(101)),
            Some(Selection::Current)
        );
        assert_eq!(
            default_selection_for_input(&Input::RecommendationFeed),
            Some(Selection::Latest)
        );
        assert_eq!(
            default_selection_for_input(&Input::FollowingFeed),
            Some(Selection::Latest)
        );
        assert_eq!(
            default_selection_for_input(&Input::SpaceDynamic(123)),
            Some(Selection::Latest)
        );
        assert_eq!(
            default_selection_for_input(&Input::History),
            Some(Selection::Latest)
        );
        assert_eq!(
            default_selection_for_input(&Input::WatchLater),
            Some(Selection::Latest)
        );
    }

    #[test]
    fn bounded_resolve_selection_uses_requested_limit() {
        let selection = bounded_resolve_selection(37).expect("resolve selection should be valid");

        let Selection::Indices(indices) = selection else {
            panic!("resolve selection should use bounded indices");
        };
        assert!(indices.contains(1));
        assert!(indices.contains(37));
        assert!(!indices.contains(38));
    }

    #[test]
    fn legacy_resolve_window_probes_one_item_beyond_100() {
        let candidate_window = BilibiliResolveCandidateWindow::new(LEGACY_RESOLVE_CANDIDATE_LIMIT)
            .expect("legacy candidate limit should be valid");
        let selection = resolve_selection_for_input(&Input::History, candidate_window)
            .expect("probe selection should be valid")
            .expect("collection inputs should use a bounded selection");

        let Selection::Indices(indices) = selection else {
            panic!("probe selection should use bounded indices");
        };
        assert!(indices.contains(100));
        assert!(indices.contains(101));
        assert!(!indices.contains(102));
    }

    #[test]
    fn resolve_candidate_window_rejects_zero_oversized_and_overflowing_limits() {
        for candidate_limit in [0, MAX_BILIBILI_RESOLVE_CANDIDATE_LIMIT + 1, usize::MAX] {
            assert!(matches!(
                BilibiliResolveCandidateWindow::new(candidate_limit),
                Err(BilibiliDownloadError::Failed(message))
                    if message.contains("candidate limit")
            ));
        }

        let maximum = BilibiliResolveCandidateWindow::new(MAX_BILIBILI_RESOLVE_CANDIDATE_LIMIT)
            .expect("v2 maximum candidate limit should be valid");
        assert_eq!(10_000, maximum.candidate_limit);
        assert_eq!(10_001, maximum.truncation_probe_limit);
    }

    #[test]
    fn resolve_selection_preserves_current_episode_inputs() {
        let candidate_window = BilibiliResolveCandidateWindow::new(7).unwrap();
        assert_eq!(
            resolve_selection_for_input(&Input::Episode(123), candidate_window).unwrap(),
            Some(Selection::Current)
        );
        assert_eq!(
            resolve_selection_for_input(&Input::CheeseEpisode(456), candidate_window).unwrap(),
            Some(Selection::Current)
        );
        assert_eq!(
            resolve_selection_for_input(&Input::IntlEpisode(789), candidate_window).unwrap(),
            Some(Selection::Current)
        );
    }

    #[test]
    fn resolve_selection_uses_full_video_metadata_for_common_video_inputs() {
        let candidate_window = BilibiliResolveCandidateWindow::new(7).unwrap();
        assert_eq!(
            resolve_selection_for_input(&Input::Bvid("BV1qt4y1X7TW".to_owned()), candidate_window,)
                .unwrap(),
            None
        );
        assert_eq!(
            resolve_selection_for_input(&Input::Aid(123), candidate_window).unwrap(),
            None
        );
    }

    #[test]
    fn resolve_selection_uses_bounded_windows_for_list_inputs() {
        let candidate_window = BilibiliResolveCandidateWindow::new(37).unwrap();
        assert_eq!(
            resolve_selection_for_input(&Input::Season(123), candidate_window).unwrap(),
            Some(Selection::Page(1))
        );
        assert_eq!(
            resolve_selection_for_input(&Input::Media(456), candidate_window).unwrap(),
            Some(Selection::Page(1))
        );
        assert_eq!(
            resolve_selection_for_input(&Input::CheeseSeason(789), candidate_window).unwrap(),
            Some(Selection::Page(1))
        );

        let list_inputs = [
            Input::SpaceVideos(123),
            Input::FavoriteList {
                media_id: Some(456),
                owner_mid: None,
            },
            Input::CollectionList(456),
            Input::SeriesList(789),
            Input::SpaceCollectionList {
                list_id: 456,
                owner_mid: 123,
            },
            Input::SpaceSeriesList {
                list_id: 789,
                owner_mid: 123,
            },
            Input::RecommendationFeed,
            Input::FollowingFeed,
            Input::SpaceDynamic(123),
            Input::History,
            Input::WatchLater,
        ];
        let bounded_selection =
            Some(bounded_resolve_selection(candidate_window.truncation_probe_limit).unwrap());

        for input in list_inputs {
            assert_eq!(
                resolve_selection_for_input(&input, candidate_window).unwrap(),
                bounded_selection
            );
        }
    }

    #[test]
    fn parses_video_page_selection_id() {
        let input_selection = playback_selection_from_id(
            &Input::Bvid("BV1xx411c7mD".to_owned()),
            Some("page:7:cid:270001:bvid:BV1xx411c7mD:aid:170001"),
        )
        .unwrap();

        assert_eq!(input_selection.input_override, None);
        assert_eq!(input_selection.selection, Some(Selection::Page(7)));
        assert_eq!(
            input_selection.expected_identity,
            Some(PlaybackExpectedIdentity {
                bvid: Some("BV1xx411c7mD".to_owned()),
                aid: Some(170_001),
                cid: Some(270_001),
            })
        );
    }

    #[test]
    fn parses_video_page_aid_selection_id() {
        let input_selection =
            playback_selection_from_id(&Input::Aid(170_001), Some("page:7:cid:270001:aid:170001"))
                .unwrap();

        assert_eq!(input_selection.input_override, None);
        assert_eq!(input_selection.selection, Some(Selection::Page(7)));
        assert_eq!(
            input_selection.expected_identity,
            Some(PlaybackExpectedIdentity {
                bvid: None,
                aid: Some(170_001),
                cid: Some(270_001),
            })
        );
    }

    #[test]
    fn parses_episode_selection_id() {
        let input_selection =
            playback_selection_from_id(&Input::Season(1), Some("episode:170001")).unwrap();

        assert_eq!(input_selection.input_override, None);
        assert_eq!(input_selection.expected_identity, None);
        assert_eq!(input_selection.selection, Some(Selection::Episode(170_001)));
    }

    #[test]
    fn parses_collection_item_bvid_selection_id_as_direct_video_selection() {
        let input_selection = playback_selection_from_id(
            &Input::History,
            Some("item:7:source:history:cid:270001:bvid:BV1xx411c7mD:aid:170001"),
        )
        .unwrap();

        assert_eq!(
            input_selection.input_override,
            Some(Input::Bvid("BV1xx411c7mD".to_owned()))
        );
        assert_eq!(input_selection.selection, None);
        assert_eq!(
            input_selection.expected_identity,
            Some(PlaybackExpectedIdentity {
                bvid: Some("BV1xx411c7mD".to_owned()),
                aid: Some(170_001),
                cid: Some(270_001),
            })
        );
    }

    #[test]
    fn parses_collection_item_aid_selection_id_as_direct_video_selection() {
        let input_selection = playback_selection_from_id(
            &Input::History,
            Some("item:7:source:history:cid:270001:aid:170001"),
        )
        .unwrap();

        assert_eq!(input_selection.input_override, Some(Input::Aid(170_001)));
        assert_eq!(input_selection.selection, None);
        assert_eq!(
            input_selection.expected_identity,
            Some(PlaybackExpectedIdentity {
                bvid: None,
                aid: Some(170_001),
                cid: Some(270_001),
            })
        );
    }

    #[test]
    fn recovers_reordered_collection_candidate_by_stable_identity() {
        let selection_id = "item:7:source:recommendation:cid:270001:bvid:BV1xx411c7mD:aid:170001";
        let current = BilibiliResolvedCandidate {
            selection_id: "item:1:source:recommendation:cid:270001:bvid:BV1xx411c7mD:aid:170001"
                .to_owned(),
            title: "Current recommendation title".to_owned(),
            subtitle: "Current owner".to_owned(),
            source_kind: "recommendation".to_owned(),
            content_id: "BV1xx411c7mD".to_owned(),
            identity: BilibiliContentIdentity {
                kind: BilibiliContentKind::CollectionItem,
                aid: Some(170_001),
                bvid: Some("BV1xx411c7mD".to_owned()),
                cid: Some(270_001),
                epid: None,
            },
            index: 1,
            duration_seconds: Some(120),
            cover_uri: "https://example.invalid/cover.jpg".to_owned(),
        };

        let recovered = recover_stable_collection_candidate(
            selection_id,
            "https://www.bilibili.com/",
            "recommendation",
            &[current],
        )
        .expect("stable identity should recover a reordered candidate");

        assert_eq!(recovered.selection_id, selection_id);
        assert_eq!(recovered.index, 7);
        assert_eq!(recovered.title, "Current recommendation title");
        assert_eq!(recovered.duration_seconds, Some(120));
        assert_eq!(
            recovered.identity,
            BilibiliContentIdentity {
                kind: BilibiliContentKind::CollectionItem,
                aid: Some(170_001),
                bvid: Some("BV1xx411c7mD".to_owned()),
                cid: Some(270_001),
                epid: None,
            }
        );
    }

    #[test]
    fn recovers_missing_collection_candidate_from_server_owned_selection_id() {
        let selection_id = "item:7:source:recommendation:cid:270001:bvid:BV1xx411c7mD:aid:170001";

        let recovered = recover_stable_collection_candidate(
            selection_id,
            "https://www.bilibili.com/",
            "recommendation",
            &[],
        )
        .expect("valid stable identity should recover without a refreshed feed match");

        assert_eq!(recovered.selection_id, selection_id);
        assert_eq!(recovered.index, 7);
        assert_eq!(recovered.title, "BV1xx411c7mD");
        assert_eq!(recovered.content_id, "BV1xx411c7mD");
        assert_eq!(recovered.source_kind, "recommendation");
        assert_eq!(
            recovered.identity,
            BilibiliContentIdentity {
                kind: BilibiliContentKind::CollectionItem,
                aid: Some(170_001),
                bvid: Some("BV1xx411c7mD".to_owned()),
                cid: Some(270_001),
                epid: None,
            }
        );
    }

    #[test]
    fn rejects_collection_item_selection_bound_to_another_source() {
        let history_selection = "item:7:source:history:cid:270001:bvid:BV1xx411c7mD:aid:170001";

        assert!(playback_selection_from_id(&Input::WatchLater, Some(history_selection)).is_err());
        assert!(
            recover_stable_collection_candidate(
                history_selection,
                "https://www.bilibili.com/",
                "recommendation",
                &[],
            )
            .is_none()
        );
    }

    #[test]
    fn direct_collection_item_selection_uses_the_page_matching_cid() {
        let resolved = ResolvedContent::Video(bbdown_core::VideoMetadata {
            aid: 170_001,
            bvid: Some("BV1xx411c7mD".to_owned()),
            title: "Multi page video".to_owned(),
            description: String::new(),
            cover_url: None,
            pub_time: None,
            owner: None,
            tags: Vec::new(),
            pages: vec![
                bbdown_core::PageMetadata {
                    index: 1,
                    aid: 170_001,
                    cid: 270_000,
                    epid: None,
                    title: "Part 1".to_owned(),
                    duration_seconds: Some(60),
                },
                bbdown_core::PageMetadata {
                    index: 2,
                    aid: 170_001,
                    cid: 270_001,
                    epid: None,
                    title: "Part 2".to_owned(),
                    duration_seconds: Some(90),
                },
            ],
        });

        let selection = direct_collection_item_page_selection(
            resolved,
            &PlaybackExpectedIdentity {
                bvid: Some("BV1xx411c7mD".to_owned()),
                aid: Some(170_001),
                cid: Some(270_001),
            },
        )
        .expect("matching cid should select its actual video page");

        assert_eq!(selection, Selection::Page(2));
    }

    #[test]
    fn direct_collection_item_selection_rejects_missing_cid() {
        let resolved = ResolvedContent::Video(bbdown_core::VideoMetadata {
            aid: 170_001,
            bvid: Some("BV1xx411c7mD".to_owned()),
            title: "Changed multi page video".to_owned(),
            description: String::new(),
            cover_url: None,
            pub_time: None,
            owner: None,
            tags: Vec::new(),
            pages: vec![bbdown_core::PageMetadata {
                index: 1,
                aid: 170_001,
                cid: 270_000,
                epid: None,
                title: "Remaining part".to_owned(),
                duration_seconds: Some(60),
            }],
        });

        let error = direct_collection_item_page_selection(
            resolved,
            &PlaybackExpectedIdentity {
                bvid: Some("BV1xx411c7mD".to_owned()),
                aid: Some(170_001),
                cid: Some(270_001),
            },
        )
        .expect_err("a missing cid must not fall back to another page");

        assert!(
            matches!(error, BilibiliDownloadError::Failed(message) if message.contains("no longer matches"))
        );
    }

    #[test]
    fn rejects_arbitrary_playback_selection_ids() {
        for selection_id in ["all", "current", "latest", "1-100", "page:1-2"] {
            assert!(
                matches!(
                    playback_selection_from_id(&Input::Bvid("BV1xx411c7mD".to_owned()), Some(selection_id)),
                    Err(BilibiliDownloadError::Failed(message))
                        if message.contains("Invalid selection_id")
                ),
                "expected {selection_id:?} to be rejected"
            );
        }
    }

    #[test]
    fn rejects_selection_ids_for_mismatched_source_kinds() {
        for (input, selection_id) in [
            (Input::History, "page:1"),
            (Input::History, "episode:170001"),
            (Input::Bvid("BV1xx411c7mD".to_owned()), "item:1:aid:170001"),
            (Input::Bvid("BV1xx411c7mD".to_owned()), "episode:170001"),
            (Input::Season(1), "page:1"),
            (Input::Season(1), "item:1:aid:170001"),
        ] {
            assert!(
                matches!(
                    playback_selection_from_id(&input, Some(selection_id)),
                    Err(BilibiliDownloadError::Failed(message))
                        if message.contains("Invalid selection_id")
                ),
                "expected {selection_id:?} to be rejected for {input:?}"
            );
        }
    }

    #[test]
    fn rejects_unstable_or_unbounded_selection_indices() {
        for (input, selection_id) in [
            (Input::History, "item:1"),
            (Input::History, "item:1:cid:270001"),
            (Input::History, "item:0:aid:170001"),
            (
                Input::History,
                "item:10001:source:history:cid:270001:aid:170001",
            ),
            (Input::History, "item:1:aid:170001"),
            (Input::Bvid("BV1xx411c7mD".to_owned()), "page:1"),
            (Input::Bvid("BV1xx411c7mD".to_owned()), "page:1:cid:270001"),
            (Input::Bvid("BV1xx411c7mD".to_owned()), "page:0"),
            (
                Input::Bvid("BV1xx411c7mD".to_owned()),
                "page:10001:cid:270001:aid:170001",
            ),
        ] {
            assert!(
                matches!(
                    playback_selection_from_id(&input, Some(selection_id)),
                    Err(BilibiliDownloadError::Failed(message))
                        if message.contains("Invalid selection_id")
                ),
                "expected {selection_id:?} to be rejected for {input:?}"
            );
        }
    }

    #[test]
    fn accepts_selection_indices_through_v2_candidate_limit() {
        let page = playback_selection_from_id(
            &Input::Bvid("BV1xx411c7mD".to_owned()),
            Some("page:10000:cid:270001:aid:170001"),
        )
        .expect("v2 maximum page index should be valid");
        assert_eq!(page.selection, Some(Selection::Page(10_000)));

        let item = playback_selection_from_id(
            &Input::History,
            Some("item:10000:source:history:cid:270001:aid:170001"),
        )
        .expect("v2 maximum collection index should be valid");
        assert_eq!(item.input_override, Some(Input::Aid(170_001)));
    }

    #[test]
    fn bounded_resolve_fallback_only_retries_short_selection_errors() {
        assert!(should_retry_bounded_resolve(&BbdownError::MissingField(
            "selected page"
        )));
        assert!(should_retry_bounded_resolve(&BbdownError::MissingField(
            "selected episode"
        )));
        assert!(should_retry_bounded_resolve(&BbdownError::MissingField(
            "selected collection item"
        )));
        assert!(!should_retry_bounded_resolve(&BbdownError::MissingField(
            "playurl streams"
        )));
        assert!(!should_retry_bounded_resolve(&BbdownError::InvalidInput(
            "bad input".to_owned()
        )));
    }

    #[test]
    fn bounded_resolve_probe_finds_largest_valid_prefix() {
        let candidate_window = BilibiliResolveCandidateWindow::new(37).unwrap();
        let candidate_limit_u32 = u32::try_from(candidate_window.candidate_limit).unwrap();
        for available_count in [1, 2, 4, 9, 24, 36, 37] {
            let mut search =
                BoundedPrefixSearch::after_failed_limit(candidate_window.truncation_probe_limit);
            let mut attempts = Vec::new();

            while let Some(limit) = search.next_limit() {
                attempts.push(limit);
                if limit <= available_count {
                    search.record_success(limit);
                } else {
                    search.record_missing(limit);
                }
            }

            assert_eq!(search.best_success(), Some(available_count));
            assert!(attempts.iter().all(|limit| *limit > 0));
            assert!(attempts.iter().all(|limit| *limit <= candidate_limit_u32));
        }
    }

    #[test]
    fn bounded_resolve_probe_returns_no_success_when_no_prefix_exists() {
        let candidate_window = BilibiliResolveCandidateWindow::new(37).unwrap();
        let mut search =
            BoundedPrefixSearch::after_failed_limit(candidate_window.truncation_probe_limit);

        while let Some(limit) = search.next_limit() {
            search.record_missing(limit);
        }

        assert_eq!(search.best_success(), None);
    }

    #[test]
    fn video_resolution_candidates_include_multiple_pages_from_all_selection() {
        let resolution = BilibiliInputResolution::from_resolved_content(
            "BV1multi".to_owned(),
            &Input::Bvid("BV1multi".to_owned()),
            ResolvedContent::Video(bbdown_core::VideoMetadata {
                aid: 170_001,
                bvid: Some("BV1multi".to_owned()),
                title: "Multi page video".to_owned(),
                description: String::new(),
                cover_url: Some("https://example.invalid/cover.jpg".to_owned()),
                pub_time: None,
                owner: None,
                tags: Vec::new(),
                pages: vec![
                    bbdown_core::PageMetadata {
                        index: 1,
                        aid: 170_001,
                        cid: 270_001,
                        epid: None,
                        title: "Part 1".to_owned(),
                        duration_seconds: Some(60),
                    },
                    bbdown_core::PageMetadata {
                        index: 2,
                        aid: 170_001,
                        cid: 270_002,
                        epid: None,
                        title: "Part 2".to_owned(),
                        duration_seconds: Some(90),
                    },
                ],
            }),
            LEGACY_RESOLVE_CANDIDATE_LIMIT,
            true,
        )
        .unwrap();

        assert_eq!(resolution.source_kind, "video");
        assert_eq!(
            resolution
                .candidates
                .iter()
                .map(|candidate| candidate.selection_id.as_str())
                .collect::<Vec<_>>(),
            [
                "page:1:cid:270001:bvid:BV1multi:aid:170001",
                "page:2:cid:270002:bvid:BV1multi:aid:170001"
            ]
        );
        assert_eq!(resolution.default_selection_id, "");
        assert_eq!(
            resolution.candidates[1].identity,
            BilibiliContentIdentity {
                kind: BilibiliContentKind::VideoPage,
                aid: Some(170_001),
                bvid: Some("BV1multi".to_owned()),
                cid: Some(270_002),
                epid: None,
            }
        );

        let input_selection = playback_selection_from_id(
            &Input::Bvid("BV1multi".to_owned()),
            Some(&resolution.candidates[1].selection_id),
        )
        .unwrap();
        assert_eq!(input_selection.selection, Some(Selection::Page(2)));
        assert_eq!(
            input_selection.expected_identity,
            Some(PlaybackExpectedIdentity {
                bvid: Some("BV1multi".to_owned()),
                aid: Some(170_001),
                cid: Some(270_002),
            })
        );
    }

    #[test]
    fn v2_resolution_omits_cover_allocations_while_materializing_candidates() {
        let pages = (1..=u32::try_from(MAX_BILIBILI_RESOLVE_CANDIDATE_LIMIT).unwrap())
            .map(|index| bbdown_core::PageMetadata {
                index,
                aid: 170_001,
                cid: 270_000 + u64::from(index),
                epid: None,
                title: format!("Part {index}"),
                duration_seconds: Some(60),
            })
            .collect();
        let resolution = BilibiliInputResolution::from_resolved_content(
            "BV1bounded-cover".to_owned(),
            &Input::Bvid("BV1bounded-cover".to_owned()),
            ResolvedContent::Video(bbdown_core::VideoMetadata {
                aid: 170_001,
                bvid: Some("BV1bounded-cover".to_owned()),
                title: "Large paginated video".to_owned(),
                description: String::new(),
                cover_url: Some("x".repeat(64 * 1024)),
                pub_time: None,
                owner: None,
                tags: Vec::new(),
                pages,
            }),
            MAX_BILIBILI_RESOLVE_CANDIDATE_LIMIT,
            false,
        )
        .unwrap();

        assert_eq!(
            MAX_BILIBILI_RESOLVE_CANDIDATE_LIMIT,
            resolution.candidates.len()
        );
        assert!(
            resolution
                .candidates
                .iter()
                .all(|candidate| candidate.cover_uri.capacity() == 0)
        );
    }

    #[test]
    fn resolution_rejects_repeated_fallback_title_before_candidate_materialization() {
        let pages = (1..=u32::try_from(MAX_BILIBILI_RESOLVE_CANDIDATE_LIMIT).unwrap())
            .map(|index| bbdown_core::PageMetadata {
                index,
                aid: 170_001,
                cid: 270_000 + u64::from(index),
                epid: None,
                title: String::new(),
                duration_seconds: Some(60),
            })
            .collect();
        let result = BilibiliInputResolution::from_resolved_content(
            "BV1oversized-title".to_owned(),
            &Input::Bvid("BV1oversized-title".to_owned()),
            ResolvedContent::Video(bbdown_core::VideoMetadata {
                aid: 170_001,
                bvid: Some("BV1oversized-title".to_owned()),
                title: "x".repeat(MAX_BILIBILI_RESOLUTION_STRING_BYTES),
                description: String::new(),
                cover_url: None,
                pub_time: None,
                owner: None,
                tags: Vec::new(),
                pages,
            }),
            MAX_BILIBILI_RESOLVE_CANDIDATE_LIMIT,
            false,
        );

        assert!(matches!(
            result,
            Err(BilibiliDownloadError::ResourceExhausted(message))
                if message.contains("server byte limit")
        ));
    }

    #[test]
    fn season_resolution_candidates_include_typed_episode_identity() {
        let episode = bbdown_core::EpisodeMetadata {
            index: 2,
            aid: 170_002,
            bvid: Some("BV1episode".to_owned()),
            cid: 270_002,
            epid: 370_002,
            title: "Episode 2".to_owned(),
            long_title: Some("A typed identity".to_owned()),
            pub_time: None,
        };
        let resolution = BilibiliInputResolution::from_resolved_content(
            "season:123".to_owned(),
            &Input::Season(123),
            ResolvedContent::Season(bbdown_core::SeasonResolution {
                season: bbdown_core::SeasonMetadata {
                    season_id: Some(123),
                    media_id: Some(456),
                    title: "Example season".to_owned(),
                    description: String::new(),
                    cover_url: None,
                    main_episode_count: 1,
                    areas: Vec::new(),
                    tags: Vec::new(),
                    episodes: vec![episode.clone()],
                },
                selected_episodes: vec![episode],
            }),
            LEGACY_RESOLVE_CANDIDATE_LIMIT,
            true,
        )
        .unwrap();

        assert_eq!(resolution.candidates.len(), 1);
        assert_eq!(
            resolution.candidates[0].identity,
            BilibiliContentIdentity {
                kind: BilibiliContentKind::SeasonEpisode,
                aid: Some(170_002),
                bvid: Some("BV1episode".to_owned()),
                cid: Some(270_002),
                epid: Some(370_002),
            }
        );
    }

    #[test]
    fn collection_resolution_candidates_round_trip_as_stable_item_selection() {
        let owner = bbdown_core::Owner {
            mid: 123,
            name: "Owner".to_owned(),
        };
        let selected_item = bbdown_core::VideoCollectionItem {
            index: 3,
            aid: 170_001,
            bvid: Some("BV1xx411c7mD".to_owned()),
            cid: 270_001,
            title: "Selected Item".to_owned(),
            cover_url: None,
            description: String::new(),
            pub_time: None,
            owner: Some(owner.clone()),
            duration_seconds: Some(120),
        };
        let unselected_item = bbdown_core::VideoCollectionItem {
            index: 4,
            aid: 170_002,
            bvid: Some("BV1yy411c7mD".to_owned()),
            cid: 270_002,
            title: "Unselected Item".to_owned(),
            cover_url: None,
            description: String::new(),
            pub_time: None,
            owner: Some(owner.clone()),
            duration_seconds: Some(90),
        };
        let resolution = BilibiliInputResolution::from_resolved_content(
            "favorite:456".to_owned(),
            &Input::FavoriteList {
                media_id: Some(456),
                owner_mid: None,
            },
            ResolvedContent::Collection(bbdown_core::VideoCollectionResolution {
                collection: bbdown_core::VideoCollectionMetadata {
                    id: Some(456),
                    kind: VideoCollectionKind::Favorite,
                    title: "Favorite Videos".to_owned(),
                    description: String::new(),
                    cover_url: Some("https://example.invalid/cover.jpg".to_owned()),
                    pub_time: None,
                    owner: Some(owner.clone()),
                    items: vec![selected_item.clone(), unselected_item.clone()],
                },
                selected_items: vec![selected_item, unselected_item],
            }),
            LEGACY_RESOLVE_CANDIDATE_LIMIT,
            true,
        )
        .unwrap();

        assert_eq!(resolution.source_kind, "favorite");
        assert_eq!(resolution.candidates.len(), 2);
        let candidate = &resolution.candidates[0];
        assert_eq!(
            candidate.selection_id,
            "item:3:source:favorite-456-none:cid:270001:bvid:BV1xx411c7mD:aid:170001"
        );
        assert_eq!(candidate.title, "Selected Item");
        assert_eq!(candidate.index, 3);
        assert_eq!(
            candidate.identity,
            BilibiliContentIdentity {
                kind: BilibiliContentKind::CollectionItem,
                aid: Some(170_001),
                bvid: Some("BV1xx411c7mD".to_owned()),
                cid: Some(270_001),
                epid: None,
            }
        );

        let input_selection = playback_selection_from_id(
            &Input::FavoriteList {
                media_id: Some(456),
                owner_mid: None,
            },
            Some(&candidate.selection_id),
        )
        .unwrap();
        assert_eq!(
            input_selection.input_override,
            Some(Input::Bvid("BV1xx411c7mD".to_owned()))
        );
        assert_eq!(input_selection.selection, None);
        assert_eq!(
            input_selection.expected_identity,
            Some(PlaybackExpectedIdentity {
                bvid: Some("BV1xx411c7mD".to_owned()),
                aid: Some(170_001),
                cid: Some(270_001),
            })
        );
    }

    #[test]
    fn collection_resolution_maps_list_kinds_to_source_kinds() {
        let owner = bbdown_core::Owner {
            mid: 123,
            name: "Owner".to_owned(),
        };

        for (kind, input, expected_source_kind) in [
            (
                VideoCollectionKind::Favorite,
                Input::FavoriteList {
                    media_id: Some(456),
                    owner_mid: None,
                },
                "favorite",
            ),
            (VideoCollectionKind::Space, Input::SpaceVideos(123), "space"),
            (
                VideoCollectionKind::Collection,
                Input::CollectionList(456),
                "collection",
            ),
            (
                VideoCollectionKind::Series,
                Input::SeriesList(456),
                "series",
            ),
            (
                VideoCollectionKind::Recommendation,
                Input::RecommendationFeed,
                "recommendation",
            ),
        ] {
            let selected_item = test_collection_item(
                1,
                &format!("{expected_source_kind} item"),
                Some(owner.clone()),
            );
            let resolution = BilibiliInputResolution::from_resolved_content(
                expected_source_kind.to_owned(),
                &input,
                ResolvedContent::Collection(bbdown_core::VideoCollectionResolution {
                    collection: bbdown_core::VideoCollectionMetadata {
                        id: Some(456),
                        kind,
                        title: format!("{expected_source_kind} list"),
                        description: String::new(),
                        cover_url: None,
                        pub_time: None,
                        owner: Some(owner.clone()),
                        items: vec![selected_item.clone()],
                    },
                    selected_items: vec![selected_item],
                }),
                LEGACY_RESOLVE_CANDIDATE_LIMIT,
                true,
            )
            .unwrap();

            assert_eq!(resolution.source_kind, expected_source_kind);
            assert_eq!(resolution.candidates.len(), 1);
            assert_eq!(resolution.candidates[0].source_kind, expected_source_kind);
            assert!(resolution.candidates[0].selection_id.starts_with("item:1:"));
        }
    }

    #[test]
    fn collection_resolution_does_not_mark_exact_candidate_limit_truncated() {
        let candidate_limit = 3_usize;
        let candidate_limit_u32 = u32::try_from(candidate_limit).unwrap();
        let owner = bbdown_core::Owner {
            mid: 123,
            name: "Owner".to_owned(),
        };
        let selected_items = (1..=candidate_limit_u32)
            .map(|index| test_collection_item(index, &format!("Item {index}"), Some(owner.clone())))
            .collect::<Vec<_>>();
        let resolution = BilibiliInputResolution::from_resolved_content(
            "history".to_owned(),
            &Input::History,
            ResolvedContent::Collection(bbdown_core::VideoCollectionResolution {
                collection: bbdown_core::VideoCollectionMetadata {
                    id: None,
                    kind: VideoCollectionKind::History,
                    title: "History".to_owned(),
                    description: String::new(),
                    cover_url: None,
                    pub_time: None,
                    owner: Some(owner),
                    items: selected_items.clone(),
                },
                selected_items,
            }),
            candidate_limit,
            true,
        )
        .unwrap();

        assert_eq!(candidate_limit, resolution.candidates.len());
        assert!(!resolution.candidates_truncated);
    }

    #[test]
    fn collection_resolution_marks_over_limit_candidate_window_truncated() {
        let candidate_limit = 3_usize;
        let candidate_limit_u32 = u32::try_from(candidate_limit).unwrap();
        let truncation_probe_limit = candidate_limit_u32.checked_add(1).unwrap();
        let owner = bbdown_core::Owner {
            mid: 123,
            name: "Owner".to_owned(),
        };
        let selected_items = (1..=truncation_probe_limit)
            .map(|index| test_collection_item(index, &format!("Item {index}"), Some(owner.clone())))
            .collect::<Vec<_>>();
        let resolution = BilibiliInputResolution::from_resolved_content(
            "history".to_owned(),
            &Input::History,
            ResolvedContent::Collection(bbdown_core::VideoCollectionResolution {
                collection: bbdown_core::VideoCollectionMetadata {
                    id: None,
                    kind: VideoCollectionKind::History,
                    title: "History".to_owned(),
                    description: String::new(),
                    cover_url: None,
                    pub_time: None,
                    owner: Some(owner),
                    items: selected_items.clone(),
                },
                selected_items,
            }),
            candidate_limit,
            true,
        )
        .unwrap();

        assert_eq!(candidate_limit, resolution.candidates.len());
        assert_eq!(
            candidate_limit_u32,
            resolution
                .candidates
                .last()
                .expect("limited candidates should not be empty")
                .index
        );
        assert!(resolution.candidates_truncated);
    }

    #[test]
    fn playback_planning_rejects_short_links_before_core_planning() {
        let result = playback_input_for_planning("https://b23.tv/demo");

        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("does not support short links")
        ));
    }

    #[test]
    fn playback_plan_validates_expected_bvid_identity() {
        let mapped = BilibiliPlaybackPlan::from_core(sample_playback_plan(), None).unwrap();

        mapped
            .validate_expected_identity(&PlaybackExpectedIdentity {
                bvid: Some("BV1test".to_owned()),
                aid: Some(1),
                cid: Some(2),
            })
            .expect("matching identity should be accepted");
    }

    #[test]
    fn playback_plan_rejects_stale_collection_selection_identity() {
        let mapped = BilibiliPlaybackPlan::from_core(sample_playback_plan(), None).unwrap();

        let result = mapped.validate_expected_identity(&PlaybackExpectedIdentity {
            bvid: Some("BV1other".to_owned()),
            aid: Some(1),
            cid: Some(2),
        });

        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("no longer matches the resolved candidate")
        ));
    }

    #[test]
    fn playback_plan_rejects_stale_page_selection_cid() {
        let mapped = BilibiliPlaybackPlan::from_core(sample_playback_plan(), None).unwrap();

        let result = mapped.validate_expected_identity(&PlaybackExpectedIdentity {
            bvid: Some("BV1test".to_owned()),
            aid: Some(1),
            cid: Some(3),
        });

        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("no longer matches the resolved candidate")
        ));
    }

    #[test]
    fn playback_plan_rejects_stale_collection_selection_cid() {
        let mapped = BilibiliPlaybackPlan::from_core(sample_playback_plan(), None).unwrap();

        let result = mapped.validate_expected_identity(&PlaybackExpectedIdentity {
            bvid: Some("BV1test".to_owned()),
            aid: Some(1),
            cid: Some(270_001),
        });

        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("no longer matches the resolved candidate")
        ));
    }

    #[test]
    fn maps_playback_plan_and_selects_avplayer_default_variant() {
        let core_plan = sample_playback_plan();
        let core_entry = &core_plan.entries[0];
        let core_h264_group = core_entry
            .abr
            .groups
            .iter()
            .find(|group| {
                group
                    .variant_ids
                    .iter()
                    .any(|variant_id| variant_id == "h264")
            })
            .unwrap()
            .clone();
        let core_h264_abr = core_entry
            .variants
            .iter()
            .find(|variant| variant.id == "h264")
            .and_then(|variant| variant.abr.as_ref())
            .unwrap()
            .clone();

        let mapped = BilibiliPlaybackPlan::from_core(core_plan, None).unwrap();

        assert_eq!(mapped.title, "Example");
        assert_eq!(mapped.entries.len(), 1);
        let entry = &mapped.entries[0];
        assert_eq!(entry.content_id, "BV1test-cid2");
        assert_eq!(entry.variants.len(), 3);
        let h264_group = entry
            .abr
            .groups
            .iter()
            .find(|group| {
                group
                    .variant_ids
                    .iter()
                    .any(|variant_id| variant_id == "h264")
            })
            .unwrap();
        assert_eq!(h264_group.id, core_h264_group.id);
        assert_eq!(h264_group.kind, BilibiliPlaybackAbrGroupKind::DashVideo);
        assert_eq!(h264_group.variant_ids, core_h264_group.variant_ids);
        assert_eq!(h264_group.level_count, core_h264_group.level_count);
        assert_eq!(h264_group.min_bandwidth, core_h264_group.min_bandwidth);
        assert_eq!(h264_group.max_bandwidth, core_h264_group.max_bandwidth);

        let selected = entry.selected_variant.as_ref().unwrap();
        assert_eq!(selected.variant.id, "h264");
        assert_eq!(
            selected.selection.policy,
            BilibiliPlaybackVariantSelectionPolicy::AvPlayerDefault
        );
        assert_eq!(selected.selection.codec_rank, Some(1_001));
        let h264_abr = selected.variant.abr.as_ref().unwrap();
        assert_eq!(h264_abr.group_id, core_h264_abr.group_id);
        assert_eq!(h264_abr.level_index, core_h264_abr.level_index);
        assert_eq!(h264_abr.level_count, core_h264_abr.level_count);
        assert_eq!(h264_abr.switchable, core_h264_abr.switchable);

        let video = selected.variant.video.as_ref().unwrap();
        assert_eq!(video.kind, BilibiliMediaRequestKind::Video);
        assert_eq!(video.url, "https://example.test/h264.m4s");
        assert_eq!(video.backup_urls, vec!["https://backup.test/h264.m4s"]);
        assert_eq!(
            video.headers,
            vec![BilibiliHttpHeader {
                name: "referer".to_owned(),
                value: "https://www.bilibili.com".to_owned(),
            }]
        );
        assert_eq!(video.cache_key.content_id, "BV1test-cid2");
        assert_eq!(video.cache_key.media_kind, BilibiliMediaRequestKind::Video);
    }

    #[test]
    fn playback_selection_honors_explicit_encoding_preference() {
        let options = bilibili_options_with_encoding("hevc");
        let mapped =
            BilibiliPlaybackPlan::from_core(sample_playback_plan(), Some(&options)).unwrap();

        let selected = mapped.entries[0].selected_variant.as_ref().unwrap();
        assert_eq!(selected.variant.id, "hevc");
        assert_eq!(
            selected.selection.policy,
            BilibiliPlaybackVariantSelectionPolicy::ExplicitEncodingPreference
        );
    }

    #[test]
    fn compatible_policy_can_downgrade_requested_quality_to_safe_variant() {
        let options = bilibili_options_with_quality("4k");
        let requested_preferences = playback_variant_preferences_from_options_with_policy(
            Some(&options),
            PlaybackPolicy {
                compatible_variant_preference: CompatibleVariantPreference::PreferRequested,
                ..PlaybackPolicy::default()
            },
        )
        .unwrap();
        let requested = BilibiliPlaybackPlan::from_core_with_preferences(
            sample_playback_plan(),
            &requested_preferences,
        )
        .unwrap();
        assert_eq!(
            "av1",
            requested.entries[0]
                .selected_variant
                .as_ref()
                .unwrap()
                .variant
                .id
        );

        let compatible_preferences = playback_variant_preferences_from_options_with_policy(
            Some(&options),
            PlaybackPolicy::default(),
        )
        .unwrap();
        let compatible = BilibiliPlaybackPlan::from_core_with_preferences(
            sample_playback_plan(),
            &compatible_preferences,
        )
        .unwrap();
        assert_eq!(
            "h264",
            compatible.entries[0]
                .selected_variant
                .as_ref()
                .unwrap()
                .variant
                .id
        );
    }

    #[test]
    fn compatible_policy_does_not_upgrade_above_requested_quality() {
        let mut plan = sample_playback_plan();
        let entry = &mut plan.entries[0];
        entry
            .variants
            .iter_mut()
            .find(|variant| variant.id == "h264")
            .and_then(|variant| variant.video.as_mut())
            .expect("H.264 video should exist")
            .stream_id = Some(80);
        entry
            .variants
            .iter_mut()
            .find(|variant| variant.id == "hevc")
            .and_then(|variant| variant.video.as_mut())
            .expect("HEVC video should exist")
            .stream_id = Some(64);

        let options = bilibili_options_with_quality("720p");
        let mapped = BilibiliPlaybackPlan::from_core(plan, Some(&options)).unwrap();

        assert_eq!(
            "hevc",
            mapped.entries[0]
                .selected_variant
                .as_ref()
                .unwrap()
                .variant
                .id
        );
    }

    #[test]
    fn compatible_policy_does_not_prefer_variant_with_unknown_envelope() {
        let mut plan = sample_playback_plan();
        let h264 = plan.entries[0]
            .variants
            .iter_mut()
            .find(|variant| variant.id == "h264")
            .expect("H.264 variant should exist");
        h264.width = None;
        h264.height = None;
        h264.bandwidth = None;
        h264.frame_rate = Some("unknown".to_owned());
        let video = h264.video.as_mut().expect("H.264 video should exist");
        video.width = None;
        video.height = None;
        video.bandwidth = None;
        video.frame_rate = Some("unknown".to_owned());
        video.codecs = Some("avc1".to_owned());

        let options = bilibili_options_with_quality("4k");
        let mapped = BilibiliPlaybackPlan::from_core(plan, Some(&options)).unwrap();

        assert_eq!(
            "av1",
            mapped.entries[0]
                .selected_variant
                .as_ref()
                .unwrap()
                .variant
                .id
        );
    }

    #[test]
    fn compatible_policy_ignores_non_dash_h264_candidate() {
        let mut plan = sample_playback_plan();
        let entry = &mut plan.entries[0];
        let mut dash_h264 = entry
            .variants
            .iter()
            .find(|variant| variant.id == "h264")
            .expect("H.264 variant should exist")
            .clone();
        dash_h264.id = "h264-dash".to_owned();
        entry
            .variants
            .iter_mut()
            .find(|variant| variant.id == "h264")
            .expect("H.264 variant should exist")
            .kind = PlaybackVariantKind::Flv;
        entry.variants.push(dash_h264);

        let mapped = BilibiliPlaybackPlan::from_core(plan, None).unwrap();

        assert_eq!(
            "h264-dash",
            mapped.entries[0]
                .selected_variant
                .as_ref()
                .unwrap()
                .variant
                .id
        );
    }

    #[test]
    fn playback_selection_falls_back_to_h264_for_unsupported_explicit_preference() {
        let mut plan = sample_playback_plan();
        plan.entries[0]
            .variants
            .retain(|variant| variant.id == "h264");
        let options = bilibili_options_with_encoding("hevc");

        let mapped = BilibiliPlaybackPlan::from_core(plan, Some(&options)).unwrap();

        let selected = mapped.entries[0].selected_variant.as_ref().unwrap();
        assert_eq!(selected.variant.id, "h264");
        assert_eq!(
            selected.selection.policy,
            BilibiliPlaybackVariantSelectionPolicy::H264AacFallback
        );
    }

    #[test]
    fn playback_selection_rejects_explicit_encoding_without_matching_candidate() {
        let mut plan = sample_playback_plan();
        plan.entries[0]
            .variants
            .retain(|variant| variant.id != "h264");
        let options = bilibili_options_with_encoding("h264");

        let result = BilibiliPlaybackPlan::from_core(plan, Some(&options));

        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("encoding_preference \"h264\"")
        ));
    }

    #[test]
    fn playback_selection_rejects_non_playable_explicit_encoding_candidate() {
        let mut plan = sample_playback_plan();
        plan.entries[0]
            .variants
            .retain(|variant| variant.id == "h264");
        plan.entries[0].variants[0]
            .selection_hints
            .avplayer
            .playable = false;
        let options = bilibili_options_with_encoding("h264");

        let result = BilibiliPlaybackPlan::from_core(plan, Some(&options));

        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("encoding_preference \"h264\"")
        ));
    }

    #[test]
    fn playback_selection_rejects_unknown_encoding_preference() {
        let options = bilibili_options_with_encoding("vp9");

        let result = BilibiliPlaybackPlan::from_core(sample_playback_plan(), Some(&options));

        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("encoding_preference")
        ));
    }

    #[test]
    fn playback_selection_honors_quality_preference() {
        let options = bilibili_options_with_quality("4k");
        let preferences = playback_variant_preferences_from_options_with_policy(
            Some(&options),
            PlaybackPolicy {
                compatible_variant_preference: CompatibleVariantPreference::PreferRequested,
                ..PlaybackPolicy::default()
            },
        )
        .unwrap();
        let mapped =
            BilibiliPlaybackPlan::from_core_with_preferences(sample_playback_plan(), &preferences)
                .unwrap();

        let selected = mapped.entries[0].selected_variant.as_ref().unwrap();
        assert_eq!(selected.variant.id, "av1");
        assert_eq!(
            selected.selection.policy,
            BilibiliPlaybackVariantSelectionPolicy::AvPlayerDefault
        );
    }

    #[test]
    fn playback_selection_rejects_unmatched_quality_preference() {
        let options = bilibili_options_with_quality("8k");

        let result = BilibiliPlaybackPlan::from_core(sample_playback_plan(), Some(&options));

        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("quality_preference 127")
        ));
    }

    #[test]
    fn playback_selection_rejects_unknown_quality_preference() {
        let options = bilibili_options_with_quality("cinema");

        let result = BilibiliPlaybackPlan::from_core(sample_playback_plan(), Some(&options));

        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("quality_preference")
        ));
    }

    #[test]
    fn playback_preferences_reject_invalid_options_before_planning() {
        let mut options = bilibili_options_with_quality("cinema");
        assert!(matches!(
            playback_variant_preferences_from_options(Some(&options)),
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("quality_preference")
        ));

        options = bilibili_options_with_encoding("vp9");
        assert!(matches!(
            playback_variant_preferences_from_options(Some(&options)),
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("encoding_preference")
        ));

        options = bilibili_options_with_quality("360p");
        options.prefer_tv_api = true;
        assert!(playback_variant_preferences_from_options(Some(&options)).is_ok());
    }

    #[tokio::test]
    async fn cancellable_bbdown_future_drops_running_operation() {
        assert_cancellable_bbdown_future_drops_running_operation(
            "Cancelled while BBDown planning was running.",
        )
        .await;
        assert_cancellable_bbdown_future_drops_running_operation(
            "Cancelled while the BBDown download was running.",
        )
        .await;
    }

    async fn assert_cancellable_bbdown_future_drops_running_operation(
        cancellation_message: &'static str,
    ) {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        struct DropProbe(Arc<AtomicBool>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));

        let started_for_future = Arc::clone(&started);
        let dropped_for_future = Arc::clone(&dropped);
        let future = async move {
            let _drop_probe = DropProbe(dropped_for_future);
            started_for_future.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
            Ok::<(), &'static str>(())
        };

        let cancel_probe = Arc::clone(&cancelled);
        let task = tokio::spawn(async move {
            run_bbdown_until_cancelled(
                future,
                || cancel_probe.load(Ordering::SeqCst),
                cancellation_message,
            )
            .await
        });

        for _ in 0..40 {
            if started.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(started.load(Ordering::SeqCst));

        cancelled.store(true, Ordering::SeqCst);
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Cancelled(message))
                if message == cancellation_message
        ));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn bbdown_progress_events_map_file_bytes_into_task_progress() {
        let mut accumulator = BilibiliBbdownProgressAccumulator::default();

        let plan_progress = accumulator
            .record(&DownloadProgressEvent::PlanStarted {
                title: "Example".to_owned(),
                output_dir: PathBuf::from("out"),
                entry_count: 1,
            })
            .expect("plan start should report progress");
        assert_eq!(Some(DOWNLOAD_PROGRESS_START), plan_progress.progress);

        let path = PathBuf::from("out/entry/video.m4s");
        let started_progress = accumulator
            .record(&DownloadProgressEvent::FileStarted {
                entry_index: 1,
                entry_title: "Entry".to_owned(),
                kind: DownloadFileKind::Video,
                path: path.clone(),
                resumed_from: 25,
                expected_size: Some(200),
                attempt: 1,
                max_attempts: 1,
            })
            .expect("file start should report progress");
        assert_eq!(Some(0), started_progress.downloaded_bytes);
        assert_eq!(Some(0), started_progress.total_bytes);

        let file_progress = accumulator
            .record(&DownloadProgressEvent::FileProgress {
                entry_index: 1,
                entry_title: "Entry".to_owned(),
                kind: DownloadFileKind::Video,
                path: path.clone(),
                bytes_delta: 75,
                bytes_written: 75,
                resumed_from: 25,
                expected_size: Some(200),
            })
            .expect("file progress should report progress");
        assert_eq!(Some(0), file_progress.downloaded_bytes);
        assert_eq!(Some(0), file_progress.total_bytes);
        let progress = file_progress.progress.expect("progress should be set");
        assert!(progress > DOWNLOAD_PROGRESS_START);
        assert!(progress < DOWNLOAD_PROGRESS_END);

        let completed_progress = accumulator
            .record(&DownloadProgressEvent::FileCompleted {
                entry_index: 1,
                entry_title: "Entry".to_owned(),
                kind: DownloadFileKind::Video,
                path,
                bytes_written: 175,
                resumed_from: 25,
                total_bytes: 200,
            })
            .expect("file complete should report progress");
        assert_eq!(Some(0), completed_progress.downloaded_bytes);
        assert_eq!(Some(0), completed_progress.total_bytes);

        let entry_completed = accumulator
            .record(&DownloadProgressEvent::EntryCompleted {
                index: 1,
                title: "Entry".to_owned(),
                directory: PathBuf::from("out/entry"),
                file_count: 1,
                mux_output: None,
            })
            .expect("entry complete should report progress");
        assert_eq!(Some(200), entry_completed.downloaded_bytes);
        assert_eq!(Some(200), entry_completed.total_bytes);

        let plan_completed = accumulator
            .record(&DownloadProgressEvent::PlanCompleted {
                title: "Example".to_owned(),
                output_dir: PathBuf::from("out"),
                entry_count: 1,
            })
            .expect("plan complete should report progress");
        assert_eq!(Some(DOWNLOAD_PROGRESS_END), plan_completed.progress);
    }

    #[test]
    fn bbdown_multi_entry_progress_counts_completed_events_and_active_entry_bytes() {
        let mut accumulator = BilibiliBbdownProgressAccumulator::default();
        accumulator
            .record(&DownloadProgressEvent::PlanStarted {
                title: "Example".to_owned(),
                output_dir: PathBuf::from("out"),
                entry_count: 2,
            })
            .expect("plan start should report progress");

        accumulator
            .record(&DownloadProgressEvent::EntryStarted {
                index: 2,
                title: "Page 2".to_owned(),
                directory: PathBuf::from("out/2"),
            })
            .expect("entry start should report progress");
        let first_path = PathBuf::from("out/2/video.m4s");
        accumulator
            .record(&DownloadProgressEvent::FileStarted {
                entry_index: 2,
                entry_title: "Page 2".to_owned(),
                kind: DownloadFileKind::Video,
                path: first_path.clone(),
                resumed_from: 0,
                expected_size: Some(100),
                attempt: 1,
                max_attempts: 1,
            })
            .expect("file start should report progress");
        accumulator
            .record(&DownloadProgressEvent::FileCompleted {
                entry_index: 2,
                entry_title: "Page 2".to_owned(),
                kind: DownloadFileKind::Video,
                path: first_path,
                bytes_written: 100,
                resumed_from: 0,
                total_bytes: 100,
            })
            .expect("file complete should report progress");

        let first_entry_done = accumulator
            .record(&DownloadProgressEvent::EntryCompleted {
                index: 2,
                title: "Page 2".to_owned(),
                directory: PathBuf::from("out/2"),
                file_count: 1,
                mux_output: None,
            })
            .expect("entry complete should report progress");
        assert_progress_near(first_entry_done.progress, 0.45);
        assert_eq!(Some(100), first_entry_done.downloaded_bytes);
        assert_eq!(Some(100), first_entry_done.total_bytes);

        accumulator
            .record(&DownloadProgressEvent::EntryStarted {
                index: 4,
                title: "Page 4".to_owned(),
                directory: PathBuf::from("out/4"),
            })
            .expect("entry start should report progress");
        let second_path = PathBuf::from("out/4/video.m4s");
        let second_started = accumulator
            .record(&DownloadProgressEvent::FileStarted {
                entry_index: 4,
                entry_title: "Page 4".to_owned(),
                kind: DownloadFileKind::Video,
                path: second_path.clone(),
                resumed_from: 0,
                expected_size: Some(100),
                attempt: 1,
                max_attempts: 1,
            })
            .expect("file start should report progress");
        assert_progress_near(second_started.progress, 0.45);
        assert_eq!(Some(100), second_started.downloaded_bytes);
        assert_eq!(Some(100), second_started.total_bytes);
        let second_half_done = accumulator
            .record(&DownloadProgressEvent::FileProgress {
                entry_index: 4,
                entry_title: "Page 4".to_owned(),
                kind: DownloadFileKind::Video,
                path: second_path.clone(),
                bytes_delta: 50,
                bytes_written: 50,
                resumed_from: 0,
                expected_size: Some(100),
            })
            .expect("second entry progress should report progress");
        assert_progress_near(second_half_done.progress, 0.625);
        assert_eq!(Some(100), second_half_done.downloaded_bytes);
        assert_eq!(Some(100), second_half_done.total_bytes);

        let second_file_done = accumulator
            .record(&DownloadProgressEvent::FileCompleted {
                entry_index: 4,
                entry_title: "Page 4".to_owned(),
                kind: DownloadFileKind::Video,
                path: second_path,
                bytes_written: 100,
                resumed_from: 0,
                total_bytes: 100,
            })
            .expect("second file complete should report progress");
        assert_progress_near(second_file_done.progress, 0.625);
        assert_eq!(Some(100), second_file_done.downloaded_bytes);
        assert_eq!(Some(100), second_file_done.total_bytes);
        let all_entries_done = accumulator
            .record(&DownloadProgressEvent::EntryCompleted {
                index: 4,
                title: "Page 4".to_owned(),
                directory: PathBuf::from("out/4"),
                file_count: 1,
                mux_output: None,
            })
            .expect("second entry complete should report progress");
        assert_progress_near(all_entries_done.progress, DOWNLOAD_PROGRESS_END);
        assert_eq!(Some(200), all_entries_done.downloaded_bytes);
        assert_eq!(Some(200), all_entries_done.total_bytes);
    }

    #[test]
    fn bbdown_active_entry_progress_stays_conservative_until_entry_completes() {
        let mut accumulator = BilibiliBbdownProgressAccumulator::default();
        accumulator
            .record(&DownloadProgressEvent::PlanStarted {
                title: "Example".to_owned(),
                output_dir: PathBuf::from("out"),
                entry_count: 1,
            })
            .expect("plan start should report progress");
        accumulator
            .record(&DownloadProgressEvent::EntryStarted {
                index: 1,
                title: "Entry".to_owned(),
                directory: PathBuf::from("out/1"),
            })
            .expect("entry start should report progress");

        let video_path = PathBuf::from("out/1/video.m4s");
        accumulator
            .record(&DownloadProgressEvent::FileStarted {
                entry_index: 1,
                entry_title: "Entry".to_owned(),
                kind: DownloadFileKind::Video,
                path: video_path.clone(),
                resumed_from: 0,
                expected_size: Some(100),
                attempt: 1,
                max_attempts: 1,
            })
            .expect("video start should report progress");
        let video_done = accumulator
            .record(&DownloadProgressEvent::FileCompleted {
                entry_index: 1,
                entry_title: "Entry".to_owned(),
                kind: DownloadFileKind::Video,
                path: video_path,
                bytes_written: 100,
                resumed_from: 0,
                total_bytes: 100,
            })
            .expect("video complete should report progress");
        assert_progress_near(video_done.progress, 0.45);
        assert_eq!(Some(0), video_done.downloaded_bytes);
        assert_eq!(Some(0), video_done.total_bytes);

        let audio_path = PathBuf::from("out/1/audio.m4s");
        let audio_started = accumulator
            .record(&DownloadProgressEvent::FileStarted {
                entry_index: 1,
                entry_title: "Entry".to_owned(),
                kind: DownloadFileKind::Audio,
                path: audio_path.clone(),
                resumed_from: 0,
                expected_size: Some(100),
                attempt: 1,
                max_attempts: 1,
            })
            .expect("audio start should report progress");
        assert_progress_near(audio_started.progress, 0.45);
        assert_eq!(Some(0), audio_started.downloaded_bytes);
        assert_eq!(Some(0), audio_started.total_bytes);

        accumulator
            .record(&DownloadProgressEvent::FileCompleted {
                entry_index: 1,
                entry_title: "Entry".to_owned(),
                kind: DownloadFileKind::Audio,
                path: audio_path,
                bytes_written: 100,
                resumed_from: 0,
                total_bytes: 100,
            })
            .expect("audio complete should report progress");
        let entry_done = accumulator
            .record(&DownloadProgressEvent::EntryCompleted {
                index: 1,
                title: "Entry".to_owned(),
                directory: PathBuf::from("out/1"),
                file_count: 2,
                mux_output: None,
            })
            .expect("entry complete should report progress");
        assert_progress_near(entry_done.progress, DOWNLOAD_PROGRESS_END);
        assert_eq!(Some(200), entry_done.downloaded_bytes);
        assert_eq!(Some(200), entry_done.total_bytes);
    }

    #[test]
    fn bbdown_file_progress_is_throttled_and_unknown_totals_remain_conservative() {
        let mut accumulator = BilibiliBbdownProgressAccumulator::default();
        let path = PathBuf::from("out/entry/video.m4s");

        accumulator
            .record(&DownloadProgressEvent::PlanStarted {
                title: "Example".to_owned(),
                output_dir: PathBuf::from("out"),
                entry_count: 1,
            })
            .expect("plan start should report progress");

        let started_progress = accumulator
            .record(&DownloadProgressEvent::FileStarted {
                entry_index: 1,
                entry_title: "Entry".to_owned(),
                kind: DownloadFileKind::Video,
                path: path.clone(),
                resumed_from: 25,
                expected_size: None,
                attempt: 1,
                max_attempts: 1,
            })
            .expect("file start should report progress");
        assert_eq!(Some(0), started_progress.downloaded_bytes);
        assert_eq!(Some(0), started_progress.total_bytes);

        let tiny_progress = accumulator.record(&DownloadProgressEvent::FileProgress {
            entry_index: 1,
            entry_title: "Entry".to_owned(),
            kind: DownloadFileKind::Video,
            path: path.clone(),
            bytes_delta: 1,
            bytes_written: 1,
            resumed_from: 25,
            expected_size: None,
        });
        assert!(tiny_progress.is_none());

        let published_progress = accumulator
            .record(&DownloadProgressEvent::FileProgress {
                entry_index: 1,
                entry_title: "Entry".to_owned(),
                kind: DownloadFileKind::Video,
                path,
                bytes_delta: DOWNLOAD_PROGRESS_PUBLISH_MIN_BYTES,
                bytes_written: DOWNLOAD_PROGRESS_PUBLISH_MIN_BYTES + 1,
                resumed_from: 25,
                expected_size: None,
            })
            .expect("large byte delta should report progress");
        assert_eq!(Some(0), published_progress.downloaded_bytes);
        assert_eq!(Some(0), published_progress.total_bytes);
    }

    #[test]
    fn bbdown_file_failed_rolls_back_bytes_before_terminal_events() {
        let (mut cancelled_accumulator, failed_progress) =
            accumulator_after_failed_bbdown_file_attempt();
        assert!(
            failed_progress
                .message
                .as_deref()
                .is_some_and(|message| message.contains("Retrying video"))
        );
        assert!(
            failed_progress
                .message
                .as_deref()
                .is_none_or(|message| !message.contains("stream reset"))
        );

        let plan_cancelled = cancelled_accumulator
            .record(&DownloadProgressEvent::PlanCancelled {
                title: "Example".to_owned(),
                output_dir: PathBuf::from("out"),
                completed_entries: 0,
                error: "cancelled".to_owned(),
            })
            .expect("plan cancellation should report progress");
        assert_progress_near(plan_cancelled.progress, DOWNLOAD_PROGRESS_START);
        assert_eq!(None, plan_cancelled.downloaded_bytes);
        assert_eq!(None, plan_cancelled.total_bytes);
        assert_eq!(
            Some("BBDown download cancelled."),
            plan_cancelled.message.as_deref()
        );

        let (mut failed_accumulator, _) = accumulator_after_failed_bbdown_file_attempt();
        let plan_failed = failed_accumulator
            .record(&DownloadProgressEvent::PlanFailed {
                title: "Example".to_owned(),
                output_dir: PathBuf::from("out"),
                completed_entries: 0,
                error: "download failed".to_owned(),
            })
            .expect("plan failure should report progress");
        assert_progress_near(plan_failed.progress, DOWNLOAD_PROGRESS_START);
        assert_eq!(None, plan_failed.downloaded_bytes);
        assert_eq!(None, plan_failed.total_bytes);
        assert_eq!(
            Some("BBDown download failed."),
            plan_failed.message.as_deref()
        );
    }

    #[tokio::test]
    async fn bbdown_download_cancellation_uses_core_token() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let cancellation = DownloadCancellationToken::new();
        let cancellation_for_future = cancellation.clone();
        let observed_cancelled_token = Arc::new(AtomicBool::new(false));
        let observed_cancelled_token_for_future = Arc::clone(&observed_cancelled_token);
        let future = async move {
            cancellation_for_future.cancelled().await;
            observed_cancelled_token_for_future
                .store(cancellation_for_future.is_cancelled(), Ordering::SeqCst);
            Err::<(), BbdownError>(BbdownError::Cancelled {
                reason: cancellation_for_future
                    .reason()
                    .unwrap_or_else(|| "missing reason".to_owned()),
            })
        };

        let result = run_bbdown_download_until_cancelled(
            future,
            &cancellation,
            || true,
            "Cancelled while the BBDown download was running.",
        )
        .await;

        assert!(cancellation.is_cancelled());
        assert!(observed_cancelled_token.load(Ordering::SeqCst));
        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Cancelled(message))
                if message == "Cancelled while the BBDown download was running."
        ));
    }

    #[tokio::test]
    async fn bbdown_download_cancellation_wins_late_core_error() {
        let cancellation = DownloadCancellationToken::new();
        let future =
            async { Err::<(), BbdownError>(BbdownError::InvalidInput("late failure".to_owned())) };

        let result = run_bbdown_download_until_cancelled(
            future,
            &cancellation,
            || true,
            "Cancelled while the BBDown download was running.",
        )
        .await;

        assert!(cancellation.is_cancelled());
        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Cancelled(message))
                if message == "Cancelled while the BBDown download was running."
        ));
    }

    #[tokio::test]
    async fn bbdown_download_cancellation_times_out_nonresponsive_core_future() {
        let cancellation = DownloadCancellationToken::new();
        let result = run_bbdown_download_until_cancelled_with_grace(
            std::future::pending::<Result<(), BbdownError>>(),
            &cancellation,
            || true,
            "Cancelled while the BBDown download was running.",
            Duration::from_millis(10),
        )
        .await;

        assert!(cancellation.is_cancelled());
        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Cancelled(message))
                if message == "Cancelled while the BBDown download was running."
        ));
    }

    #[test]
    fn prefers_mux_outputs_before_raw_downloaded_files() {
        let report = DownloadReport {
            title: "Example".to_owned(),
            output_dir: PathBuf::from("out"),
            entries: vec![EntryDownloadReport {
                index: 1,
                title: "Entry".to_owned(),
                directory: PathBuf::from("out/entry"),
                files: vec![
                    DownloadedFile {
                        kind: DownloadFileKind::Video,
                        path: PathBuf::from("out/entry/video.m4s"),
                        bytes_written: 10,
                        resumed_from: 5,
                    },
                    DownloadedFile {
                        kind: DownloadFileKind::Audio,
                        path: PathBuf::from("out/entry/audio.m4s"),
                        bytes_written: 3,
                        resumed_from: 0,
                    },
                ],
                mux: Some(MuxReport {
                    output_path: PathBuf::from("out/entry/Entry.mp4"),
                    command: Vec::new(),
                    chapter_count: 0,
                }),
            }],
        };

        assert_eq!(
            playable_output_candidates(&report),
            vec![
                PathBuf::from("out/entry/Entry.mp4"),
                PathBuf::from("out/entry/video.m4s")
            ]
        );
        assert_eq!(report.summary().total_bytes, 18);
    }

    #[test]
    fn playback_file_names_fit_common_name_max_with_multibyte_titles() {
        let entry = EntryDownloadReport {
            index: 1,
            title: "标题🙂".repeat(120),
            directory: PathBuf::from("out/entry"),
            files: Vec::new(),
            mux: None,
        };

        let output_path = playback_output_path(&entry);
        let file_name = output_path.file_name().unwrap().to_str().unwrap();
        let temp_file_name = temporary_mux_output_path(&output_path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();

        assert!(file_name.ends_with(PLAYBACK_EXTENSION));
        assert!(file_name.len() <= MAX_FILE_NAME_BYTES);
        assert!(temp_file_name.len() <= MAX_FILE_NAME_BYTES);
        assert!(file_name.is_char_boundary(file_name.trim_end_matches(PLAYBACK_EXTENSION).len()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mux_download_report_uses_entry_title_and_non_indexed_temporary_output() {
        let temp = tempfile::tempdir().unwrap();
        let entry_dir = temp.path().join("entry");
        std::fs::create_dir_all(&entry_dir).unwrap();
        let video_path = entry_dir.join("video.m4s");
        let audio_path = entry_dir.join("audio.m4s");
        let subtitle_path = entry_dir.join("subtitle.srt");
        std::fs::write(&video_path, b"video").unwrap();
        std::fs::write(&audio_path, b"audio").unwrap();
        std::fs::write(&subtitle_path, b"subtitle").unwrap();
        let ffmpeg = write_fake_ffmpeg(temp.path());

        let report = DownloadReport {
            title: "Example".to_owned(),
            output_dir: temp.path().to_path_buf(),
            entries: vec![EntryDownloadReport {
                index: 1,
                title: "Entry: Episode 1?".to_owned(),
                directory: entry_dir.clone(),
                files: vec![
                    DownloadedFile {
                        kind: DownloadFileKind::Video,
                        path: video_path.clone(),
                        bytes_written: 5,
                        resumed_from: 0,
                    },
                    DownloadedFile {
                        kind: DownloadFileKind::Audio,
                        path: audio_path.clone(),
                        bytes_written: 5,
                        resumed_from: 0,
                    },
                    DownloadedFile {
                        kind: DownloadFileKind::Subtitle,
                        path: subtitle_path.clone(),
                        bytes_written: 5,
                        resumed_from: 0,
                    },
                ],
                mux: None,
            }],
        };

        let never_cancelled = || false;
        let report = mux_download_report(report, &ffmpeg, &never_cancelled)
            .await
            .unwrap();
        let mux = report.entries[0].mux.as_ref().unwrap();
        let output_path = entry_dir.join("Entry_ Episode 1.mp4");
        assert_eq!(output_path, mux.output_path);
        assert_eq!(b"muxed", std::fs::read(&output_path).unwrap().as_slice());
        assert!(!video_path.exists());
        assert!(!audio_path.exists());
        assert!(subtitle_path.exists());

        let args_log = std::fs::read_to_string(temp.path().join("ffmpeg-args.log")).unwrap();
        let args = args_log.lines().collect::<Vec<_>>();
        let mux_temp_arg = args.last().unwrap();
        assert_eq!(&["-f", "mp4"], &args[args.len() - 3..args.len() - 1]);
        assert_eq!(
            Some("cache-server-mux-tmp"),
            Path::new(mux_temp_arg)
                .extension()
                .and_then(|value| value.to_str())
        );
        assert!(
            Path::new(mux_temp_arg)
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.starts_with('.'))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mux_download_report_cancels_running_ffmpeg() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let temp = tempfile::tempdir().unwrap();
        let entry_dir = temp.path().join("entry");
        std::fs::create_dir_all(&entry_dir).unwrap();
        let video_path = entry_dir.join("video.m4s");
        let audio_path = entry_dir.join("audio.m4s");
        std::fs::write(&video_path, b"video").unwrap();
        std::fs::write(&audio_path, b"audio").unwrap();
        let ffmpeg = write_blocking_fake_ffmpeg(temp.path());
        let output_path = entry_dir.join("Entry.mp4");
        let temp_output_path = temporary_mux_output_path(&output_path);
        let report = DownloadReport {
            title: "Example".to_owned(),
            output_dir: temp.path().to_path_buf(),
            entries: vec![EntryDownloadReport {
                index: 1,
                title: "Entry".to_owned(),
                directory: entry_dir,
                files: vec![
                    DownloadedFile {
                        kind: DownloadFileKind::Video,
                        path: video_path.clone(),
                        bytes_written: 5,
                        resumed_from: 0,
                    },
                    DownloadedFile {
                        kind: DownloadFileKind::Audio,
                        path: audio_path.clone(),
                        bytes_written: 5,
                        resumed_from: 0,
                    },
                ],
                mux: None,
            }],
        };

        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_probe = Arc::clone(&cancelled);
        let ffmpeg_for_task = ffmpeg.clone();
        let mut mux_task = tokio::spawn(async move {
            let is_cancel_requested = || cancel_probe.load(Ordering::SeqCst);
            mux_download_report(report, &ffmpeg_for_task, &is_cancel_requested).await
        });

        let ffmpeg_started_path = temp.path().join("ffmpeg-started");
        tokio::select! {
            () = wait_for_path(&ffmpeg_started_path) => {}
            result = &mut mux_task => {
                match result {
                    Ok(Ok(_)) => panic!("mux task completed before fake ffmpeg started"),
                    Ok(Err(error)) => {
                        panic!("mux task failed before fake ffmpeg started: {error:?}")
                    }
                    Err(error) => panic!("mux task panicked before fake ffmpeg started: {error}"),
                }
            }
        }
        let pid = std::fs::read_to_string(temp.path().join("ffmpeg.pid"))
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        cancelled.store(true, Ordering::SeqCst);

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), mux_task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Cancelled(message))
                if message == "Cancelled while BBDown muxing was running."
        ));
        assert!(!output_path.exists());
        assert!(!temp_output_path.exists());
        assert!(!video_path.exists());
        assert!(!audio_path.exists());
        wait_for_process_exit(pid).await;
    }

    #[tokio::test]
    async fn mux_download_report_cleans_source_streams_when_ffmpeg_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let entry_dir = temp.path().join("entry");
        std::fs::create_dir_all(&entry_dir).unwrap();
        let video_path = entry_dir.join("video.m4s");
        let audio_path = entry_dir.join("audio.m4s");
        let subtitle_path = entry_dir.join("subtitle.srt");
        std::fs::write(&video_path, b"video").unwrap();
        std::fs::write(&audio_path, b"audio").unwrap();
        std::fs::write(&subtitle_path, b"subtitle").unwrap();
        let output_path = entry_dir.join("Entry.mp4");
        let temp_output_path = temporary_mux_output_path(&output_path);
        let report = DownloadReport {
            title: "Example".to_owned(),
            output_dir: temp.path().to_path_buf(),
            entries: vec![EntryDownloadReport {
                index: 1,
                title: "Entry".to_owned(),
                directory: entry_dir,
                files: vec![
                    DownloadedFile {
                        kind: DownloadFileKind::Video,
                        path: video_path.clone(),
                        bytes_written: 5,
                        resumed_from: 0,
                    },
                    DownloadedFile {
                        kind: DownloadFileKind::Audio,
                        path: audio_path.clone(),
                        bytes_written: 5,
                        resumed_from: 0,
                    },
                    DownloadedFile {
                        kind: DownloadFileKind::Subtitle,
                        path: subtitle_path.clone(),
                        bytes_written: 5,
                        resumed_from: 0,
                    },
                ],
                mux: None,
            }],
        };

        let never_cancelled = || false;
        let result = mux_download_report(
            report,
            &temp.path().join("missing-ffmpeg"),
            &never_cancelled,
        )
        .await;

        assert!(matches!(result, Err(BilibiliDownloadError::Failed(_))));
        assert!(!output_path.exists());
        assert!(!temp_output_path.exists());
        assert!(!video_path.exists());
        assert!(!audio_path.exists());
        assert!(subtitle_path.exists());
    }

    fn bilibili_options_with_encoding(encoding_preference: &str) -> BilibiliDownloadOptions {
        BilibiliDownloadOptions {
            quality_preference: String::new(),
            encoding_preference: encoding_preference.to_owned(),
            prefer_tv_api: false,
            download_subtitles: false,
            download_danmaku: false,
            audio_language: String::new(),
            subtitle_ai_policy: BilibiliSubtitleAiPolicy::Unspecified.into(),
            download_cover: false,
            danmaku_formats: Vec::new(),
            download_mode: 0,
        }
    }

    fn bilibili_options_with_quality(quality_preference: &str) -> BilibiliDownloadOptions {
        BilibiliDownloadOptions {
            quality_preference: quality_preference.to_owned(),
            encoding_preference: String::new(),
            prefer_tv_api: false,
            download_subtitles: false,
            download_danmaku: false,
            audio_language: String::new(),
            subtitle_ai_policy: BilibiliSubtitleAiPolicy::Unspecified.into(),
            download_cover: false,
            danmaku_formats: Vec::new(),
            download_mode: 0,
        }
    }

    fn test_collection_item(
        index: u32,
        title: &str,
        owner: Option<bbdown_core::Owner>,
    ) -> bbdown_core::VideoCollectionItem {
        bbdown_core::VideoCollectionItem {
            index,
            aid: 170_000 + u64::from(index),
            bvid: Some(format!("BV1test{index}")),
            cid: 270_000 + u64::from(index),
            title: title.to_owned(),
            cover_url: None,
            description: String::new(),
            pub_time: None,
            owner,
            duration_seconds: Some(index),
        }
    }

    fn sample_playback_plan() -> PlaybackPlan {
        serde_json::from_str(
            r#"{
              "title": "Example",
              "entries": [
                {
                  "index": 1,
                  "aid": 1,
                  "bvid": "BV1test",
                  "cid": 2,
                  "epid": null,
                  "title": "Episode 1",
                  "cover_url": null,
                  "source": "normal_web",
                  "qualities": [],
                  "duration_seconds": 90,
                  "variants": [
                    {
                      "id": "hevc",
                      "kind": "dash",
                      "video": {
                        "kind": "video",
                        "stream_id": 80,
                        "url": "https://example.test/hevc.m4s",
                        "backup_urls": [],
                        "headers": [
                          {
                            "name": "referer",
                            "value": "https://www.bilibili.com"
                          }
                        ],
                        "mime_type": "video/mp4",
                        "codecs": "hev1.1.6.L120.90",
                        "codec_family": "hevc",
                        "bandwidth": 800000,
                        "width": 1920,
                        "height": 1080,
                        "frame_rate": "60",
                        "size": 1000,
                        "duration_seconds": 90,
                        "cache_key": {
                          "content_id": "BV1test-cid2",
                          "media_kind": "video",
                          "stream_id": 80,
                          "codecs": "hev1.1.6.L120.90",
                          "source_hash": "hevc0001"
                        }
                      },
                      "audio": {
                        "kind": "audio",
                        "stream_id": 30280,
                        "url": "https://example.test/audio.m4s",
                        "backup_urls": [],
                        "headers": [],
                        "mime_type": "audio/mp4",
                        "codecs": "mp4a.40.2",
                        "codec_family": "aac",
                        "bandwidth": 128000,
                        "width": null,
                        "height": null,
                        "frame_rate": null,
                        "size": 100,
                        "duration_seconds": 90,
                        "cache_key": {
                          "content_id": "BV1test-cid2",
                          "media_kind": "audio",
                          "stream_id": 30280,
                          "codecs": "mp4a.40.2",
                          "source_hash": "audio001"
                        }
                      },
                      "flv_segments": [],
                      "bandwidth": 928000,
                      "codecs": ["hev1.1.6.L120.90", "mp4a.40.2"],
                      "mime_types": ["video/mp4", "audio/mp4"],
                      "width": 1920,
                      "height": 1080,
                      "frame_rate": "60",
                      "duration_seconds": 90
                    },
                    {
                      "id": "h264",
                      "kind": "dash",
                      "video": {
                        "kind": "video",
                        "stream_id": 64,
                        "url": "https://example.test/h264.m4s",
                        "backup_urls": ["https://backup.test/h264.m4s"],
                        "headers": [
                          {
                            "name": "referer",
                            "value": "https://www.bilibili.com"
                          }
                        ],
                        "mime_type": "video/mp4",
                        "codecs": "avc1.640028",
                        "codec_family": "h264",
                        "bandwidth": 1200000,
                        "width": 1920,
                        "height": 1080,
                        "frame_rate": "60",
                        "size": 1200,
                        "duration_seconds": 90,
                        "cache_key": {
                          "content_id": "BV1test-cid2",
                          "media_kind": "video",
                          "stream_id": 64,
                          "codecs": "avc1.640028",
                          "source_hash": "h2640001"
                        }
                      },
                      "audio": {
                        "kind": "audio",
                        "stream_id": 30280,
                        "url": "https://example.test/audio.m4s",
                        "backup_urls": [],
                        "headers": [],
                        "mime_type": "audio/mp4",
                        "codecs": "mp4a.40.2",
                        "codec_family": "aac",
                        "bandwidth": 128000,
                        "width": null,
                        "height": null,
                        "frame_rate": null,
                        "size": 100,
                        "duration_seconds": 90,
                        "cache_key": {
                          "content_id": "BV1test-cid2",
                          "media_kind": "audio",
                          "stream_id": 30280,
                          "codecs": "mp4a.40.2",
                          "source_hash": "audio001"
                        }
                      },
                      "flv_segments": [],
                      "bandwidth": 1328000,
                      "codecs": ["avc1.640028", "mp4a.40.2"],
                      "mime_types": ["video/mp4", "audio/mp4"],
                      "width": 1920,
                      "height": 1080,
                      "frame_rate": "60",
                      "duration_seconds": 90
                    },
                    {
                      "id": "av1",
                      "kind": "dash",
                      "video": {
                        "kind": "video",
                        "stream_id": 120,
                        "url": "https://example.test/av1.m4s",
                        "backup_urls": [],
                        "headers": [],
                        "mime_type": "video/mp4",
                        "codecs": "av01.0.08M.08",
                        "codec_family": "av1",
                        "bandwidth": 600000,
                        "width": 1920,
                        "height": 1080,
                        "frame_rate": "60",
                        "size": 900,
                        "duration_seconds": 90,
                        "cache_key": {
                          "content_id": "BV1test-cid2",
                          "media_kind": "video",
                          "stream_id": 120,
                          "codecs": "av01.0.08M.08",
                          "source_hash": "av100001"
                        }
                      },
                      "audio": {
                        "kind": "audio",
                        "stream_id": 30280,
                        "url": "https://example.test/audio.m4s",
                        "backup_urls": [],
                        "headers": [],
                        "mime_type": "audio/mp4",
                        "codecs": "mp4a.40.2",
                        "codec_family": "aac",
                        "bandwidth": 128000,
                        "width": null,
                        "height": null,
                        "frame_rate": null,
                        "size": 100,
                        "duration_seconds": 90,
                        "cache_key": {
                          "content_id": "BV1test-cid2",
                          "media_kind": "audio",
                          "stream_id": 30280,
                          "codecs": "mp4a.40.2",
                          "source_hash": "audio001"
                        }
                      },
                      "flv_segments": [],
                      "bandwidth": 728000,
                      "codecs": ["av01.0.08M.08", "mp4a.40.2"],
                      "mime_types": ["video/mp4", "audio/mp4"],
                      "width": 1920,
                      "height": 1080,
                      "frame_rate": "60",
                      "duration_seconds": 90
                    }
                  ]
                }
              ]
            }"#,
        )
        .unwrap()
    }

    #[cfg(unix)]
    async fn wait_for_path(path: &Path) {
        for _ in 0..600 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {}", path.display());
    }

    #[cfg(unix)]
    async fn wait_for_process_exit(pid: i32) {
        for _ in 0..200 {
            if !process_is_running(pid) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("process {pid} was still running after cancellation");
    }

    #[cfg(unix)]
    fn process_is_running(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[cfg(unix)]
    fn write_fake_ffmpeg(dir: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("fake-ffmpeg");
        std::fs::write(
            &path,
            r#"#!/bin/sh
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
args_log="$script_dir/ffmpeg-args.log"
: > "$args_log"
last=
for arg in "$@"; do
  printf '%s\n' "$arg" >> "$args_log"
  last=$arg
done
printf muxed > "$last"
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(unix)]
    fn write_blocking_fake_ffmpeg(dir: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("blocking-fake-ffmpeg");
        std::fs::write(
            &path,
            r#"#!/bin/sh
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
printf '%s\n' "$$" > "$script_dir/ffmpeg.pid"
: > "$script_dir/ffmpeg-started"
last=
for arg in "$@"; do
  last=$arg
done
printf partial > "$last"
while :; do
  sleep 1
done
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }
}
