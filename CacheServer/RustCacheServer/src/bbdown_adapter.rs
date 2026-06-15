use std::{
    ffi::OsString,
    fmt::Display,
    future::Future,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};

use bbdown_core::{
    BiliClient, ClientConfig, DownloadArchive, DownloadFileKind, DownloadOptions, DownloadReport,
    DuplicateDecision, EntryDownloadReport, HttpHeaderSpec, Input, MediaRequestKind,
    MediaRequestSpec, MuxOptions, MuxReport, PlaybackAbrGroup, PlaybackAbrGroupKind,
    PlaybackAbrLevel, PlaybackAbrMetadata, PlaybackCodecPreference, PlaybackPlan, PlaybackVariant,
    PlaybackVariantKind, Selection, StreamSelection,
};
use tokio::{fs, io::AsyncReadExt, process::Command, sync::Mutex, time::sleep};

use crate::{
    bilibili_worker::{
        BilibiliDownloadAdapter, BilibiliDownloadContext, BilibiliDownloadError,
        BilibiliDownloadFuture, BilibiliDownloadOutput, BilibiliDownloadRequest,
    },
    config::CacheServerOptions,
    generated::tvos_net_player::v1::BilibiliDownloadOptions,
    library::LocalMediaLibrary,
    task_registry::BilibiliTaskProgress,
};

pub struct BbdownBilibiliAdapter {
    client: BiliClient,
    library: Arc<LocalMediaLibrary>,
    output_dir: PathBuf,
    archive_path: PathBuf,
    ffmpeg_path: PathBuf,
    archive_lock: Arc<Mutex<()>>,
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
}

#[allow(dead_code)]
struct SelectedCorePlaybackVariant<'a> {
    variant: &'a PlaybackVariant,
    selection: BilibiliPlaybackVariantSelection,
}

impl BbdownBilibiliAdapter {
    pub fn new(options: Arc<CacheServerOptions>, library: Arc<LocalMediaLibrary>) -> Self {
        Self {
            client: BiliClient::new(ClientConfig::default()),
            library,
            output_dir: options.bbdown_output_dir(),
            archive_path: options.bbdown_archive_path(),
            ffmpeg_path: options.bbdown_ffmpeg_path.clone(),
            archive_lock: Arc::new(Mutex::new(())),
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

        let input = Input::parse(&request.source).map_err(failed)?;
        let download_options = self.download_options(request.options.as_ref())?;

        context.report_progress(progress(
            0.02,
            "Planning Bilibili download with BBDown core.",
        ));
        let selection = default_selection_for_input(&input);
        let plan = run_bbdown_until_cancelled(
            self.client.plan(input, selection),
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
        let report = run_bbdown_until_cancelled(
            self.client.download_plan_with_archive_decision(
                &plan,
                download_options,
                &mut archive,
                DuplicateDecision::KeepBoth,
            ),
            || context.is_cancel_requested(),
            "Cancelled while the BBDown download was running.",
        )
        .await?;

        let downloaded_bytes = downloaded_bytes(&report);
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
                archive.save(&self.archive_path).map_err(failed)?;
                return Ok(BilibiliDownloadOutput {
                    library_item_id,
                    message: success_message(&report),
                });
            }
        }

        Err(BilibiliDownloadError::Failed(format!(
            "BBDown finished but produced no playable cache item under {}. Ensure ffmpeg is installed and muxing outputs .mp4 files.",
            self.library.root_path().display()
        )))
    }

    fn download_options(
        &self,
        options: Option<&BilibiliDownloadOptions>,
    ) -> Result<DownloadOptions, BilibiliDownloadError> {
        validate_supported_download_options(options)?;
        Ok(DownloadOptions::new(self.output_dir.clone())
            .with_stream_selection(stream_selection_from_options(options))
            .with_subtitles(options.is_some_and(|options| options.download_subtitles))
            .with_danmaku(options.is_some_and(|options| options.download_danmaku))
            .with_mux(MuxOptions::Disabled))
    }

    #[allow(dead_code)]
    pub(crate) async fn plan_playback(
        &self,
        source: &str,
        options: Option<&BilibiliDownloadOptions>,
        is_cancel_requested: impl Fn() -> bool,
    ) -> Result<BilibiliPlaybackPlan, BilibiliDownloadError> {
        let preferences = playback_variant_preferences_from_options(options)?;
        let input = playback_input_for_planning(source)?;
        let selection = default_selection_for_input(&input);
        let plan = run_bbdown_until_cancelled(
            self.client.plan_playback_input(input, selection),
            is_cancel_requested,
            "Cancelled while BBDown playback planning was running.",
        )
        .await?;
        BilibiliPlaybackPlan::from_core_with_preferences(plan, &preferences)
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

fn playback_input_for_planning(source: &str) -> Result<Input, BilibiliDownloadError> {
    let input = Input::parse(source).map_err(failed)?;
    if matches!(input, Input::ShortLink(_)) {
        return Err(BilibiliDownloadError::Failed(
            "BBDown playback planning does not support short links yet; expand the b23.tv URL before submitting it.".to_owned(),
        ));
    }
    Ok(input)
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
    let encoding_preference = playback_explicit_encoding_preference(options).map(str::to_owned);
    Ok(PlaybackVariantPreferences {
        codec_candidates: playback_codec_preferences_from_options(options)?,
        quality_preference: playback_quality_preference_from_options(options)?,
        allow_avplayer_hint_fallback: encoding_preference.is_none(),
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
    if options.is_some_and(|options| options.prefer_tv_api) {
        return Err(BilibiliDownloadError::Failed(
            "BBDown playback planning does not support prefer_tv_api yet; set prefer_tv_api=false."
                .to_owned(),
        ));
    }

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
            return Ok(Some(SelectedCorePlaybackVariant {
                variant,
                selection: BilibiliPlaybackVariantSelection {
                    policy: candidate.policy,
                    codec_rank: Some(codec_rank),
                    score: variant.selection_hints.avplayer.score,
                },
            }));
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

    Ok(candidate_variants
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
        }))
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

    if options.prefer_tv_api {
        return Err(BilibiliDownloadError::Failed(
            "BBDown adapter does not support prefer_tv_api yet; set prefer_tv_api=false."
                .to_owned(),
        ));
    }

    Ok(())
}

fn stream_selection_from_options(options: Option<&BilibiliDownloadOptions>) -> StreamSelection {
    options
        .and_then(|options| video_quality_preference(&options.quality_preference))
        .map(StreamSelection::video)
        .unwrap_or_default()
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
    Ok(report)
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

fn downloaded_bytes(report: &DownloadReport) -> u64 {
    report
        .entries
        .iter()
        .flat_map(|entry| &entry.files)
        .map(|file| file.bytes_written.saturating_add(file.resumed_from))
        .sum()
}

fn to_i64_saturating(value: u64) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
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
    use bbdown_core::{DownloadedFile, EntryDownloadReport, MuxReport};

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
        };

        assert!(validate_supported_download_options(Some(&options)).is_ok());
    }

    #[test]
    fn rejects_unsupported_encoding_preference() {
        let options = BilibiliDownloadOptions {
            quality_preference: String::new(),
            encoding_preference: "hevc".to_owned(),
            prefer_tv_api: false,
            download_subtitles: false,
            download_danmaku: false,
        };

        let result = validate_supported_download_options(Some(&options));
        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("does not support encoding_preference")
        ));
    }

    #[test]
    fn rejects_unsupported_tv_api_preference() {
        let options = BilibiliDownloadOptions {
            quality_preference: String::new(),
            encoding_preference: String::new(),
            prefer_tv_api: true,
            download_subtitles: false,
            download_danmaku: false,
        };

        let result = validate_supported_download_options(Some(&options));
        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("does not support prefer_tv_api")
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
    fn playback_planning_rejects_short_links_before_core_planning() {
        let result = playback_input_for_planning("https://b23.tv/demo");

        assert!(matches!(
            result,
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("does not support short links")
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

        let mapped =
            BilibiliPlaybackPlan::from_core(sample_playback_plan(), Some(&options)).unwrap();

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

        options.prefer_tv_api = true;
        assert!(matches!(
            playback_variant_preferences_from_options(Some(&options)),
            Err(BilibiliDownloadError::Failed(message))
                if message.contains("prefer_tv_api")
        ));
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
        assert_eq!(downloaded_bytes(&report), 18);
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
        let mux_task = tokio::spawn(async move {
            let is_cancel_requested = || cancel_probe.load(Ordering::SeqCst);
            mux_download_report(report, &ffmpeg_for_task, &is_cancel_requested).await
        });

        wait_for_path(&temp.path().join("ffmpeg-started")).await;
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
        }
    }

    fn bilibili_options_with_quality(quality_preference: &str) -> BilibiliDownloadOptions {
        BilibiliDownloadOptions {
            quality_preference: quality_preference.to_owned(),
            encoding_preference: String::new(),
            prefer_tv_api: false,
            download_subtitles: false,
            download_danmaku: false,
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
        for _ in 0..40 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {}", path.display());
    }

    #[cfg(unix)]
    async fn wait_for_process_exit(pid: i32) {
        for _ in 0..40 {
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
last=
for arg in "$@"; do
  last=$arg
done
printf partial > "$last"
: > "$script_dir/ffmpeg-started"
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
