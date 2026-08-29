use std::{future::Future, pin::Pin};

use crate::{
    bbdown_adapter::{BbdownBilibiliAdapter, BilibiliPlaybackPlan},
    bilibili_worker::BilibiliDownloadError,
    generated::tvos_net_player::v1::{
        BilibiliDownloadOptions, BilibiliPlaybackOptions, BilibiliRequestContext,
        BilibiliSubtitleAiPolicy,
    },
    playback_policy::PlaybackPolicy,
    task_registry::BilibiliTaskCancellation,
};

pub(crate) type BilibiliPlaybackPlanningFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BilibiliPlaybackPlan, BilibiliDownloadError>> + Send + 'a>>;
pub(crate) type BilibiliInputResolveFuture<'a> = Pin<
    Box<dyn Future<Output = Result<BilibiliInputResolution, BilibiliDownloadError>> + Send + 'a>,
>;

pub(crate) const MAX_BILIBILI_RESOLVE_CANDIDATE_LIMIT: usize = 10_000;
pub(crate) const MAX_BILIBILI_RESOLUTION_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_BILIBILI_RESOLUTION_STRING_BYTES: usize = 64 * 1024;

pub(crate) trait BilibiliPlaybackPlanner: Send + Sync + 'static {
    fn resolve_input<'a>(
        &'a self,
        _request: BilibiliInputResolveRequest,
    ) -> BilibiliInputResolveFuture<'a> {
        Box::pin(async {
            Err(BilibiliDownloadError::Failed(
                "Bilibili input resolver is not configured.".to_owned(),
            ))
        })
    }

    fn plan<'a>(
        &'a self,
        request: BilibiliPlaybackPlanningRequest,
    ) -> BilibiliPlaybackPlanningFuture<'a>;
}

#[derive(Clone)]
pub(crate) struct BilibiliInputResolveRequest {
    pub source: String,
    pub options: Option<BilibiliPlaybackOptions>,
    pub request_context: Option<BilibiliRequestContext>,
    pub candidate_limit: usize,
    pub include_candidate_cover_uri: bool,
    pub cancellation: BilibiliTaskCancellation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BilibiliInputResolution {
    pub source: String,
    pub title: String,
    pub source_kind: String,
    pub candidates: Vec<BilibiliResolvedCandidate>,
    pub default_selection_id: String,
    pub candidates_truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum BilibiliContentKind {
    VideoPage,
    SeasonEpisode,
    CollectionItem,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct BilibiliContentIdentity {
    pub kind: BilibiliContentKind,
    pub aid: Option<u64>,
    pub bvid: Option<String>,
    pub cid: Option<u64>,
    pub epid: Option<u64>,
}

impl BilibiliContentIdentity {
    pub(crate) fn is_complete(&self) -> bool {
        if self.aid == Some(0)
            || self.cid == Some(0)
            || self.epid == Some(0)
            || self
                .bvid
                .as_deref()
                .is_some_and(|bvid| bvid.trim().is_empty())
        {
            return false;
        }

        let aid_or_bvid = self.aid.is_some()
            || self
                .bvid
                .as_deref()
                .is_some_and(|bvid| !bvid.trim().is_empty());
        match self.kind {
            BilibiliContentKind::VideoPage | BilibiliContentKind::CollectionItem => {
                self.cid.is_some() && aid_or_bvid && self.epid.is_none()
            }
            BilibiliContentKind::SeasonEpisode => self.epid.is_some(),
        }
    }

    pub(crate) fn matches_content_id(&self, content_id: &str) -> bool {
        match self.kind {
            BilibiliContentKind::VideoPage => {
                self.cid.is_some_and(|cid| content_id == cid.to_string())
            }
            BilibiliContentKind::SeasonEpisode => {
                self.epid.is_some_and(|epid| content_id == epid.to_string())
            }
            BilibiliContentKind::CollectionItem => {
                self.bvid.as_deref().is_some_and(|bvid| content_id == bvid)
                    || self.aid.is_some_and(|aid| content_id == format!("av{aid}"))
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BilibiliResolvedCandidate {
    pub selection_id: String,
    pub title: String,
    pub subtitle: String,
    pub source_kind: String,
    pub content_id: String,
    pub identity: BilibiliContentIdentity,
    pub index: u32,
    pub duration_seconds: Option<u32>,
    pub cover_uri: String,
}

#[derive(Clone)]
pub(crate) struct BilibiliPlaybackPlanningRequest {
    pub source: String,
    pub options: Option<BilibiliPlaybackOptions>,
    pub request_context: Option<BilibiliRequestContext>,
    pub selection_id: Option<String>,
    pub cancellation: BilibiliTaskCancellation,
}

impl BilibiliPlaybackPlanner for BbdownBilibiliAdapter {
    fn resolve_input<'a>(
        &'a self,
        request: BilibiliInputResolveRequest,
    ) -> BilibiliInputResolveFuture<'a> {
        Box::pin(async move {
            PlaybackPolicy::from_playback_options(request.options.as_ref())
                .map_err(|error| BilibiliDownloadError::Failed(error.to_string()))?;
            let download_options = request.options.as_ref().map(playback_to_download_options);
            self.resolve_playback_input(
                &request.source,
                download_options.as_ref(),
                request.request_context.as_ref(),
                request.candidate_limit,
                request.include_candidate_cover_uri,
                || request.cancellation.is_cancel_requested(),
            )
            .await
        })
    }

    fn plan<'a>(
        &'a self,
        request: BilibiliPlaybackPlanningRequest,
    ) -> BilibiliPlaybackPlanningFuture<'a> {
        Box::pin(async move {
            let download_options = request.options.as_ref().map(playback_to_download_options);
            let playback_policy =
                PlaybackPolicy::from_playback_options(request.options.as_ref())
                    .map_err(|error| BilibiliDownloadError::Failed(error.to_string()))?;
            self.plan_playback(
                &request.source,
                request.selection_id.as_deref(),
                download_options.as_ref(),
                request.request_context.as_ref(),
                playback_policy,
                || request.cancellation.is_cancel_requested(),
            )
            .await
        })
    }
}

fn playback_to_download_options(options: &BilibiliPlaybackOptions) -> BilibiliDownloadOptions {
    BilibiliDownloadOptions {
        quality_preference: options.quality_preference.clone(),
        encoding_preference: options.encoding_preference.clone(),
        prefer_tv_api: options.prefer_tv_api,
        download_subtitles: false,
        download_danmaku: false,
        audio_language: options.audio_language.clone(),
        subtitle_ai_policy: BilibiliSubtitleAiPolicy::Unspecified.into(),
        download_cover: false,
        danmaku_formats: Vec::new(),
        download_mode: 0,
    }
}
