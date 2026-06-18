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
    DuplicateDecision, EntryDownloadReport, Error as BbdownError, HttpHeaderSpec, IndexSelection,
    Input, MediaRequestKind, MediaRequestSpec, MuxOptions, MuxReport, PlaybackAbrGroup,
    PlaybackAbrGroupKind, PlaybackAbrLevel, PlaybackAbrMetadata, PlaybackCodecPreference,
    PlaybackPlan, PlaybackVariant, PlaybackVariantKind, ResolvedContent, Selection,
    StreamSelection, VideoCollectionKind,
};
use tokio::{fs, io::AsyncReadExt, process::Command, sync::Mutex, time::sleep};

use crate::{
    bilibili_playback::{BilibiliInputResolution, BilibiliResolvedCandidate},
    bilibili_worker::{
        BilibiliDownloadAdapter, BilibiliDownloadContext, BilibiliDownloadError,
        BilibiliDownloadFuture, BilibiliDownloadOutput, BilibiliDownloadRequest,
    },
    config::CacheServerOptions,
    generated::tvos_net_player::v1::BilibiliDownloadOptions,
    library::LocalMediaLibrary,
    task_registry::BilibiliTaskProgress,
};

const BILIBILI_RESOLVE_CANDIDATE_LIMIT: usize = 100;
const BILIBILI_RESOLVE_CANDIDATE_LIMIT_U32: u32 = BILIBILI_RESOLVE_CANDIDATE_LIMIT as u32;

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
    pub(crate) async fn resolve_playback_input(
        &self,
        source: &str,
        options: Option<&BilibiliDownloadOptions>,
        is_cancel_requested: impl Fn() -> bool,
    ) -> Result<BilibiliInputResolution, BilibiliDownloadError> {
        let _preferences = playback_variant_preferences_from_options(options)?;
        let input = playback_input_for_planning(source)?;
        let selection = resolve_selection_for_input(&input)?;
        let resolved = match run_bbdown_core_until_cancelled(
            self.client.resolve(input.clone(), Some(selection)),
            &is_cancel_requested,
            "Cancelled while BBDown input resolution was running.",
        )
        .await?
        {
            Ok(resolved) => resolved,
            Err(error) if should_retry_bounded_resolve(&error) => {
                self.retry_resolve_with_largest_bounded_prefix(
                    input.clone(),
                    error,
                    &is_cancel_requested,
                )
                .await?
            }
            Err(error) => return Err(failed(error)),
        };
        Ok(BilibiliInputResolution::from_resolved_content(
            source.trim().to_owned(),
            &input,
            resolved,
        ))
    }

    async fn retry_resolve_with_largest_bounded_prefix(
        &self,
        input: Input,
        initial_error: BbdownError,
        is_cancel_requested: &impl Fn() -> bool,
    ) -> Result<ResolvedContent, BilibiliDownloadError> {
        let mut last_error = initial_error;
        let mut search =
            BoundedPrefixSearch::after_failed_limit(BILIBILI_RESOLVE_CANDIDATE_LIMIT_U32);
        let mut best_resolved = None;

        while let Some(limit) = search.next_limit() {
            let selection = bounded_resolve_selection(limit)?;
            match run_bbdown_core_until_cancelled(
                self.client.resolve(input.clone(), Some(selection)),
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
        is_cancel_requested: impl Fn() -> bool,
    ) -> Result<BilibiliPlaybackPlan, BilibiliDownloadError> {
        let preferences = playback_variant_preferences_from_options(options)?;
        let input = playback_input_for_planning(source)?;
        let input_selection = playback_selection_from_id(&input, selection_id)?;
        let input = input_selection.input_override.unwrap_or(input);
        let selection = input_selection
            .selection
            .or_else(|| default_selection_for_input(&input));
        let plan = run_bbdown_until_cancelled(
            self.client.plan_playback_input(input, selection),
            is_cancel_requested,
            "Cancelled while BBDown playback planning was running.",
        )
        .await?;
        let plan = BilibiliPlaybackPlan::from_core_with_preferences(plan, &preferences)?;
        if let Some(expected_identity) = input_selection.expected_identity.as_ref() {
            plan.validate_expected_identity(expected_identity)?;
        }
        Ok(plan)
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

fn resolve_selection() -> Result<Selection, BilibiliDownloadError> {
    bounded_resolve_selection(BILIBILI_RESOLVE_CANDIDATE_LIMIT_U32)
}

fn resolve_selection_for_input(input: &Input) -> Result<Selection, BilibiliDownloadError> {
    match input {
        Input::Aid(_) | Input::Bvid(_) => Ok(Selection::All),
        Input::Episode(_) | Input::CheeseEpisode(_) | Input::IntlEpisode(_) => {
            Ok(Selection::Current)
        }
        Input::Season(_) | Input::Media(_) | Input::CheeseSeason(_) => Ok(Selection::Page(1)),
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
        | Input::WatchLater => Ok(Selection::Latest),
        _ => resolve_selection(),
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
            next_high: failed_limit
                .saturating_sub(1)
                .min(BILIBILI_RESOLVE_CANDIDATE_LIMIT_U32),
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
}

impl PlaybackExpectedIdentity {
    fn matches(&self, entry: &BilibiliPlaybackEntry) -> bool {
        if let Some(expected_bvid) = self.bvid.as_deref()
            && let Some(actual_bvid) = entry.bvid.as_deref()
        {
            return actual_bvid.eq_ignore_ascii_case(expected_bvid);
        }

        self.aid
            .is_some_and(|expected_aid| entry.aid == expected_aid)
    }

    fn input_override(&self) -> Option<Input> {
        self.bvid
            .as_ref()
            .map(|bvid| Input::Bvid(bvid.clone()))
            .or_else(|| self.aid.map(Input::Aid))
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
        return parse_selection_index(page, selection_id).map(|page| PlaybackInputSelection {
            input_override: None,
            selection: Some(Selection::Page(page)),
            expected_identity: None,
        });
    }
    if let Some(item) = selection_id.strip_prefix("item:") {
        if !playback_input_accepts_collection_item_selection(input) {
            return Err(invalid_selection_id(selection_id));
        }
        return playback_collection_item_selection_from_id(item, selection_id);
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

fn playback_collection_item_selection_from_id(
    item: &str,
    selection_id: &str,
) -> Result<PlaybackInputSelection, BilibiliDownloadError> {
    let mut parts = item.split(':');
    let index_text = parts
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_selection_id(selection_id))?;
    let index = parse_selection_index(index_text, selection_id)?;
    let expected_identity = playback_expected_identity_from_parts(parts, selection_id)?;
    if expected_identity.is_none() {
        return Err(invalid_selection_id(selection_id));
    }
    let input_override = expected_identity
        .as_ref()
        .and_then(PlaybackExpectedIdentity::input_override);
    let selection = if input_override.is_some() {
        None
    } else {
        Some(
            IndexSelection::single(index)
                .map(Selection::Indices)
                .map_err(failed)?,
        )
    };

    Ok(PlaybackInputSelection {
        input_override,
        selection,
        expected_identity,
    })
}

fn playback_expected_identity_from_parts<'a>(
    mut parts: impl Iterator<Item = &'a str>,
    selection_id: &str,
) -> Result<Option<PlaybackExpectedIdentity>, BilibiliDownloadError> {
    let mut bvid = None;
    let mut aid = None;
    while let Some(kind) = parts.next() {
        let Some(value) = parts.next() else {
            return Err(invalid_selection_id(selection_id));
        };
        match kind {
            "bvid" if bvid.is_none() && !value.trim().is_empty() => {
                bvid = Some(value.trim().to_owned());
            }
            "aid" if aid.is_none() => {
                aid = Some(parse_selection_u64(value, selection_id)?);
            }
            _ => return Err(invalid_selection_id(selection_id)),
        }
    }

    if bvid.is_none() && aid.is_none() {
        return Ok(None);
    }
    Ok(Some(PlaybackExpectedIdentity { bvid, aid }))
}

fn parse_selection_index(text: &str, selection_id: &str) -> Result<u32, BilibiliDownloadError> {
    let index = parse_selection_u32(text, selection_id)?;
    if index == 0 || index > BILIBILI_RESOLVE_CANDIDATE_LIMIT_U32 {
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
    fn from_resolved_content(source: String, input: &Input, resolved: ResolvedContent) -> Self {
        match resolved {
            ResolvedContent::Video(video) => {
                let candidates = video
                    .pages
                    .iter()
                    .take(BILIBILI_RESOLVE_CANDIDATE_LIMIT)
                    .map(|page| BilibiliResolvedCandidate {
                        selection_id: page_selection_id(page.index),
                        title: non_empty_or(&page.title, &video.title),
                        subtitle: format!("Page {}", page.index),
                        source_kind: "video_page".to_owned(),
                        content_id: page.cid.to_string(),
                        index: page.index,
                        duration_seconds: page.duration_seconds,
                        cover_uri: video.cover_url.clone().unwrap_or_default(),
                    })
                    .collect::<Vec<_>>();
                Self::with_candidates(source, video.title, "video", candidates)
            }
            ResolvedContent::Season(season) => {
                let episodes = if resolve_should_use_full_episode_list(input) {
                    &season.season.episodes
                } else {
                    &season.selected_episodes
                };
                let candidates = episodes
                    .iter()
                    .take(BILIBILI_RESOLVE_CANDIDATE_LIMIT)
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
                            index: episode.index,
                            duration_seconds: None,
                            cover_uri: season.season.cover_url.clone().unwrap_or_default(),
                        }
                    })
                    .collect::<Vec<_>>();
                Self::with_candidates(source, season.season.title, "season", candidates)
            }
            ResolvedContent::Collection(collection) => {
                let source_kind = collection_kind_name(&collection.collection.kind);
                let candidates = collection
                    .selected_items
                    .iter()
                    .take(BILIBILI_RESOLVE_CANDIDATE_LIMIT)
                    .map(|item| BilibiliResolvedCandidate {
                        selection_id: collection_item_selection_id(item),
                        title: item.title.clone(),
                        subtitle: collection_item_subtitle(source_kind, item.index, &item.owner),
                        source_kind: source_kind.to_owned(),
                        content_id: item
                            .bvid
                            .clone()
                            .unwrap_or_else(|| format!("av{}", item.aid)),
                        index: item.index,
                        duration_seconds: item.duration_seconds,
                        cover_uri: item.cover_url.clone().unwrap_or_default(),
                    })
                    .collect::<Vec<_>>();
                Self::with_candidates(source, collection.collection.title, source_kind, candidates)
            }
        }
    }

    fn with_candidates(
        source: String,
        title: String,
        source_kind: impl Into<String>,
        candidates: Vec<BilibiliResolvedCandidate>,
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
        }
    }
}

fn resolve_should_use_full_episode_list(input: &Input) -> bool {
    matches!(
        input,
        Input::Season(_) | Input::Media(_) | Input::CheeseSeason(_)
    )
}

fn page_selection_id(index: u32) -> String {
    format!("page:{index}")
}

fn collection_item_selection_id(item: &bbdown_core::VideoCollectionItem) -> String {
    item.bvid
        .as_deref()
        .map(str::trim)
        .filter(|bvid| !bvid.is_empty())
        .map_or_else(
            || format!("item:{}:aid:{}", item.index, item.aid),
            |bvid| format!("item:{}:bvid:{}:aid:{}", item.index, bvid, item.aid),
        )
}

fn episode_selection_id(epid: u64) -> String {
    format!("episode:{epid}")
}

fn non_empty_or(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_owned()
    } else {
        trimmed.to_owned()
    }
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
    fn bounded_resolve_selection_limits_candidates_to_first_page_window() {
        let selection = resolve_selection().expect("resolve selection should be valid");

        let Selection::Indices(indices) = selection else {
            panic!("resolve selection should use bounded indices");
        };
        assert!(indices.contains(1));
        assert!(indices.contains(BILIBILI_RESOLVE_CANDIDATE_LIMIT_U32));
        assert!(!indices.contains(BILIBILI_RESOLVE_CANDIDATE_LIMIT_U32 + 1));
    }

    #[test]
    fn resolve_selection_preserves_current_episode_inputs() {
        assert_eq!(
            resolve_selection_for_input(&Input::Episode(123)).unwrap(),
            Selection::Current
        );
        assert_eq!(
            resolve_selection_for_input(&Input::CheeseEpisode(456)).unwrap(),
            Selection::Current
        );
        assert_eq!(
            resolve_selection_for_input(&Input::IntlEpisode(789)).unwrap(),
            Selection::Current
        );
    }

    #[test]
    fn resolve_selection_uses_single_overview_requests_for_common_inputs() {
        assert_eq!(
            resolve_selection_for_input(&Input::Bvid("BV1qt4y1X7TW".to_owned())).unwrap(),
            Selection::All
        );
        assert_eq!(
            resolve_selection_for_input(&Input::Aid(123)).unwrap(),
            Selection::All
        );
        assert_eq!(
            resolve_selection_for_input(&Input::Season(123)).unwrap(),
            Selection::Page(1)
        );
        assert_eq!(
            resolve_selection_for_input(&Input::Media(456)).unwrap(),
            Selection::Page(1)
        );
        assert_eq!(
            resolve_selection_for_input(&Input::CheeseSeason(789)).unwrap(),
            Selection::Page(1)
        );

        let list_inputs = [
            Input::FavoriteList {
                media_id: Some(456),
                owner_mid: None,
            },
            Input::RecommendationFeed,
            Input::History,
        ];

        for input in list_inputs {
            assert_eq!(
                resolve_selection_for_input(&input).unwrap(),
                Selection::Latest
            );
        }
    }

    #[test]
    fn parses_video_page_selection_id() {
        let input_selection =
            playback_selection_from_id(&Input::Bvid("BV1xx411c7mD".to_owned()), Some("page:7"))
                .unwrap();

        assert_eq!(input_selection.input_override, None);
        assert_eq!(input_selection.expected_identity, None);
        assert_eq!(input_selection.selection, Some(Selection::Page(7)));
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
    fn parses_collection_item_bvid_selection_id_as_stable_input_override() {
        let input_selection = playback_selection_from_id(
            &Input::History,
            Some("item:7:bvid:BV1xx411c7mD:aid:170001"),
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
            })
        );
    }

    #[test]
    fn parses_collection_item_aid_selection_id_as_stable_input_override() {
        let input_selection =
            playback_selection_from_id(&Input::History, Some("item:7:aid:170001")).unwrap();

        assert_eq!(input_selection.input_override, Some(Input::Aid(170_001)));
        assert_eq!(input_selection.selection, None);
        assert_eq!(
            input_selection.expected_identity,
            Some(PlaybackExpectedIdentity {
                bvid: None,
                aid: Some(170_001),
            })
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
            (Input::History, "item:0:aid:170001"),
            (Input::History, "item:101:aid:170001"),
            (Input::Bvid("BV1xx411c7mD".to_owned()), "page:0"),
            (Input::Bvid("BV1xx411c7mD".to_owned()), "page:101"),
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
        for available_count in [1, 2, 4, 9, 24, 49, 99] {
            let mut search =
                BoundedPrefixSearch::after_failed_limit(BILIBILI_RESOLVE_CANDIDATE_LIMIT_U32);
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
            assert!(
                attempts
                    .iter()
                    .all(|limit| *limit < BILIBILI_RESOLVE_CANDIDATE_LIMIT_U32)
            );
        }
    }

    #[test]
    fn bounded_resolve_probe_returns_no_success_when_no_prefix_exists() {
        let mut search =
            BoundedPrefixSearch::after_failed_limit(BILIBILI_RESOLVE_CANDIDATE_LIMIT_U32);

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
        );

        assert_eq!(resolution.source_kind, "video");
        assert_eq!(
            resolution
                .candidates
                .iter()
                .map(|candidate| candidate.selection_id.as_str())
                .collect::<Vec<_>>(),
            ["page:1", "page:2"]
        );
        assert_eq!(resolution.default_selection_id, "");
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
                    items: vec![selected_item.clone(), unselected_item],
                },
                selected_items: vec![selected_item],
            }),
        );

        assert_eq!(resolution.source_kind, "favorite");
        assert_eq!(resolution.candidates.len(), 1);
        let candidate = &resolution.candidates[0];
        assert_eq!(
            candidate.selection_id,
            "item:3:bvid:BV1xx411c7mD:aid:170001"
        );
        assert_eq!(candidate.title, "Selected Item");
        assert_eq!(candidate.index, 3);

        let input_selection =
            playback_selection_from_id(&Input::History, Some(&candidate.selection_id)).unwrap();
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
            })
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
    fn playback_plan_validates_expected_bvid_identity() {
        let mapped = BilibiliPlaybackPlan::from_core(sample_playback_plan(), None).unwrap();

        mapped
            .validate_expected_identity(&PlaybackExpectedIdentity {
                bvid: Some("BV1test".to_owned()),
                aid: Some(1),
            })
            .expect("matching identity should be accepted");
    }

    #[test]
    fn playback_plan_rejects_stale_collection_selection_identity() {
        let mapped = BilibiliPlaybackPlan::from_core(sample_playback_plan(), None).unwrap();

        let result = mapped.validate_expected_identity(&PlaybackExpectedIdentity {
            bvid: Some("BV1other".to_owned()),
            aid: Some(170_001),
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
        for _ in 0..200 {
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
