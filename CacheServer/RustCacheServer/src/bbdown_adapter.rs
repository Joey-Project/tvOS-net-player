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
    DuplicateDecision, EntryDownloadReport, Input, MuxOptions, MuxReport, Selection,
    StreamSelection,
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
        let download_options = self.download_options(request.options.as_ref());
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

    fn download_options(&self, options: Option<&BilibiliDownloadOptions>) -> DownloadOptions {
        DownloadOptions::new(self.output_dir.clone())
            .with_stream_selection(stream_selection_from_options(options))
            .with_subtitles(options.is_some_and(|options| options.download_subtitles))
            .with_danmaku(options.is_some_and(|options| options.download_danmaku))
            .with_mux(MuxOptions::Disabled)
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
    let media_files = entry
        .files
        .iter()
        .filter(|file| is_media_kind(&file.kind))
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    if media_files.is_empty() {
        return Ok(None);
    }

    fs::create_dir_all(&entry.directory).await.map_err(failed)?;
    let output_path = entry.directory.join("cache-server-playback.mp4");
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
        mux_output_path.as_os_str().to_os_string(),
    ]);

    let output = match run_ffmpeg_mux(ffmpeg_path, &args, is_cancel_requested).await {
        Ok(output) => output,
        Err(error) => {
            let _ = fs::remove_file(&mux_output_path).await;
            return Err(error);
        }
    };
    if !output.status.success() {
        let _ = fs::remove_file(&mux_output_path).await;
        return Err(BilibiliDownloadError::Failed(format!(
            "BBDown adapter ffmpeg mux failed with status {}: {}",
            output.status.code().map_or_else(
                || "terminated by signal".to_owned(),
                |code| code.to_string()
            ),
            stderr_tail(&output.stderr)
        )));
    }

    let metadata = fs::metadata(&mux_output_path).await.map_err(failed)?;
    if !metadata.is_file() || metadata.len() == 0 {
        let _ = fs::remove_file(&mux_output_path).await;
        return Err(BilibiliDownloadError::Failed(
            "BBDown adapter ffmpeg mux produced no playable output.".to_owned(),
        ));
    }

    if let Err(error) = fs::rename(&mux_output_path, &output_path).await {
        if output_path.exists() {
            fs::remove_file(&output_path).await.map_err(failed)?;
            fs::rename(&mux_output_path, &output_path)
                .await
                .map_err(failed)?;
        } else {
            return Err(failed(error));
        }
    }

    Ok(Some(MuxReport {
        output_path,
        command: command_report(ffmpeg_path, &args),
    }))
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

fn temporary_mux_output_path(output_path: &Path) -> PathBuf {
    output_path.with_file_name(".cache-server-playback.tmp.mp4")
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

    #[cfg(unix)]
    #[tokio::test]
    async fn mux_download_report_uses_mp4_temporary_output() {
        let temp = tempfile::tempdir().unwrap();
        let entry_dir = temp.path().join("entry");
        std::fs::create_dir_all(&entry_dir).unwrap();
        let video_path = entry_dir.join("video.m4s");
        let audio_path = entry_dir.join("audio.m4s");
        std::fs::write(&video_path, b"video").unwrap();
        std::fs::write(&audio_path, b"audio").unwrap();
        let ffmpeg = write_fake_ffmpeg(temp.path());

        let report = DownloadReport {
            title: "Example".to_owned(),
            output_dir: temp.path().to_path_buf(),
            entries: vec![EntryDownloadReport {
                index: 1,
                title: "Entry".to_owned(),
                directory: entry_dir.clone(),
                files: vec![
                    DownloadedFile {
                        kind: DownloadFileKind::Video,
                        path: video_path,
                        bytes_written: 5,
                        resumed_from: 0,
                    },
                    DownloadedFile {
                        kind: DownloadFileKind::Audio,
                        path: audio_path,
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
        let output_path = entry_dir.join("cache-server-playback.mp4");
        assert_eq!(output_path, mux.output_path);
        assert_eq!(b"muxed", std::fs::read(&output_path).unwrap().as_slice());

        let args_log = std::fs::read_to_string(temp.path().join("ffmpeg-args.log")).unwrap();
        let args = args_log.lines().collect::<Vec<_>>();
        let mux_temp_arg = args.last().unwrap();
        assert!(mux_temp_arg.ends_with(".mp4"));
        assert!(mux_temp_arg.contains(".tmp."));
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
        let output_path = entry_dir.join("cache-server-playback.mp4");
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
                        path: video_path,
                        bytes_written: 5,
                        resumed_from: 0,
                    },
                    DownloadedFile {
                        kind: DownloadFileKind::Audio,
                        path: audio_path,
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
        wait_for_process_exit(pid).await;
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
