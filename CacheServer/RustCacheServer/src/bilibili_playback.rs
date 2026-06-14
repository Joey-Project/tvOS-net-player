use std::{future::Future, pin::Pin};

use crate::{
    bbdown_adapter::{BbdownBilibiliAdapter, BilibiliPlaybackPlan},
    bilibili_worker::BilibiliDownloadError,
    generated::tvos_net_player::v1::{BilibiliDownloadOptions, BilibiliPlaybackOptions},
    task_registry::BilibiliTaskCancellation,
};

pub(crate) type BilibiliPlaybackPlanningFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BilibiliPlaybackPlan, BilibiliDownloadError>> + Send + 'a>>;

pub(crate) trait BilibiliPlaybackPlanner: Send + Sync + 'static {
    fn plan<'a>(
        &'a self,
        request: BilibiliPlaybackPlanningRequest,
    ) -> BilibiliPlaybackPlanningFuture<'a>;
}

#[derive(Clone)]
pub(crate) struct BilibiliPlaybackPlanningRequest {
    pub source: String,
    pub options: Option<BilibiliPlaybackOptions>,
    pub cancellation: BilibiliTaskCancellation,
}

impl BilibiliPlaybackPlanner for BbdownBilibiliAdapter {
    fn plan<'a>(
        &'a self,
        request: BilibiliPlaybackPlanningRequest,
    ) -> BilibiliPlaybackPlanningFuture<'a> {
        Box::pin(async move {
            let download_options = request.options.as_ref().map(playback_to_download_options);
            self.plan_playback(&request.source, download_options.as_ref(), || {
                request.cancellation.is_cancel_requested()
            })
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
