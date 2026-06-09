use std::{fmt::Display, path::PathBuf, sync::Arc};

use bbdown_core::{
    BiliClient, ClientConfig, DownloadArchive, DownloadFileKind, DownloadOptions, DownloadReport,
    DuplicateDecision, Input, MuxOptions, Selection, StreamSelection,
};
use tokio::sync::Mutex;

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

        context.report_progress(progress(
            0.02,
            "Planning Bilibili download with BBDown core.",
        ));
        let input = Input::parse(&request.source).map_err(failed)?;
        let selection = default_selection_for_input(&input);
        let plan = self.client.plan(input, selection).await.map_err(failed)?;

        if context.is_cancel_requested() {
            return Err(BilibiliDownloadError::Cancelled(
                "Cancelled after Bilibili planning completed.".to_owned(),
            ));
        }

        context.report_progress(progress(
            0.10,
            format!("Downloading {} Bilibili entry(s).", plan.entries.len()),
        ));
        let download_options = self.download_options(request.options.as_ref());
        let report = {
            let _archive_guard = self.archive_lock.lock().await;
            if context.is_cancel_requested() {
                return Err(BilibiliDownloadError::Cancelled(
                    "Cancelled before the BBDown download started.".to_owned(),
                ));
            }

            let mut archive = DownloadArchive::load(&self.archive_path).map_err(failed)?;
            let report = self
                .client
                .download_plan_with_archive_decision(
                    &plan,
                    download_options,
                    &mut archive,
                    DuplicateDecision::KeepBoth,
                )
                .await
                .map_err(failed)?;
            archive.save(&self.archive_path).map_err(failed)?;
            report
        };

        let downloaded_bytes = downloaded_bytes(&report);
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

    fn download_options(&self, options: Option<&BilibiliDownloadOptions>) -> DownloadOptions {
        DownloadOptions::new(self.output_dir.clone())
            .with_stream_selection(stream_selection_from_options(options))
            .with_subtitles(options.is_some_and(|options| options.download_subtitles))
            .with_danmaku(options.is_some_and(|options| options.download_danmaku))
            .with_mux(MuxOptions::ffmpeg(self.ffmpeg_path.clone()))
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
        Input::Season(_) | Input::Media(_) => Some(Selection::Latest),
        Input::Aid(_) | Input::Bvid(_) | Input::Episode(_) | Input::IntlEpisode(_) => {
            Some(Selection::Current)
        }
    }
}

fn stream_selection_from_options(options: Option<&BilibiliDownloadOptions>) -> StreamSelection {
    options
        .and_then(|options| video_quality_preference(&options.quality_preference))
        .map(StreamSelection::video)
        .unwrap_or_default()
}

fn video_quality_preference(value: &str) -> Option<u32> {
    let normalized = value
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '_', '-'], "");
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
                file.kind,
                DownloadFileKind::Video | DownloadFileKind::FlvSegment
            ) {
                candidates.push(file.path.clone());
            }
        }
    }
    candidates
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
}
