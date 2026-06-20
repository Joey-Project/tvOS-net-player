use std::{future::Future, pin::Pin};

use crate::{
    bbdown_adapter::{BbdownBilibiliAdapter, BilibiliPlaybackPlan},
    bilibili_worker::BilibiliDownloadError,
    generated::tvos_net_player::v1::{BilibiliDownloadOptions, BilibiliPlaybackOptions},
    task_registry::BilibiliTaskCancellation,
};

pub(crate) type BilibiliPlaybackPlanningFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BilibiliPlaybackPlan, BilibiliDownloadError>> + Send + 'a>>;
pub(crate) type BilibiliInputResolveFuture<'a> = Pin<
    Box<dyn Future<Output = Result<BilibiliInputResolution, BilibiliDownloadError>> + Send + 'a>,
>;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BilibiliResolvedCandidate {
    pub selection_id: String,
    pub title: String,
    pub subtitle: String,
    pub source_kind: String,
    pub content_id: String,
    pub index: u32,
    pub duration_seconds: Option<u32>,
    pub cover_uri: String,
}

#[derive(Clone)]
pub(crate) struct BilibiliPlaybackPlanningRequest {
    pub source: String,
    pub options: Option<BilibiliPlaybackOptions>,
    pub selection_id: Option<String>,
    pub cancellation: BilibiliTaskCancellation,
}

impl BilibiliPlaybackPlanner for BbdownBilibiliAdapter {
    fn resolve_input<'a>(
        &'a self,
        request: BilibiliInputResolveRequest,
    ) -> BilibiliInputResolveFuture<'a> {
        Box::pin(async move {
            let download_options = request.options.as_ref().map(playback_to_download_options);
            self.resolve_playback_input(&request.source, download_options.as_ref(), || {
                request.cancellation.is_cancel_requested()
            })
            .await
        })
    }

    fn plan<'a>(
        &'a self,
        request: BilibiliPlaybackPlanningRequest,
    ) -> BilibiliPlaybackPlanningFuture<'a> {
        Box::pin(async move {
            let download_options = request.options.as_ref().map(playback_to_download_options);
            self.plan_playback(
                &request.source,
                request.selection_id.as_deref(),
                download_options.as_ref(),
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
    }
}
