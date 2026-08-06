use std::{
    ffi::OsString,
    path::Path,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::sleep,
};

use crate::{
    bbdown_adapter::BilibiliPlaybackVariant,
    config::CacheServerOptions,
    playback_policy::{
        PlaybackPolicy, TranscodingPreference, variant_is_avplayer_h264_aac_hls_compatible,
    },
};

pub(crate) const LAN_TRANSCODING_PROFILE_ID: &str = "avplayer-h264-aac-hls-v1";
pub(crate) const LAN_TRANSCODING_VIDEO_CODEC: &str = "avc1.64002A";
pub(crate) const LAN_TRANSCODING_AUDIO_CODEC: &str = "mp4a.40.2";
pub(crate) const LAN_TRANSCODING_MAX_WIDTH: u32 = 1920;
pub(crate) const LAN_TRANSCODING_MAX_HEIGHT: u32 = 1080;
pub(crate) const LAN_TRANSCODING_MAX_FRAME_RATE: f64 = 60.0;
pub(crate) const LAN_TRANSCODING_MAX_VIDEO_BANDWIDTH_BPS: u64 = 10_000_000;
pub(crate) const LAN_TRANSCODING_AUDIO_BANDWIDTH_BPS: u64 = 128_000;
const TARGET_CONTAINER: &str = "hls/fmp4";
const TARGET_VIDEO_CODEC: &str = "h264";
const TARGET_AUDIO_CODEC: &str = "aac";
const OUTPUT_PROTOCOL: &str = "hls";
const HLS_FFMPEG_TRANSCODE_LEVEL: &str = "4.2";
const HLS_FFMPEG_TRANSCODE_VIDEO_MAXRATE: &str = "10000k";
const HLS_FFMPEG_TRANSCODE_VIDEO_BUFSIZE: &str = "20000k";
const HLS_FFMPEG_TRANSCODE_FILTER: &str = "scale=w='min(1920,iw)':h='min(1080,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2,fps=fps='min(source_fps,60)'";
const FFMPEG_STDERR_TAIL_MAX_BYTES: usize = 16 * 1024;
const FFMPEG_STDERR_TAIL_MAX_CHARS: usize = 1200;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LanTranscodingProfile {
    pub(crate) id: String,
    pub(crate) target_container: String,
    pub(crate) target_video_codec: String,
    pub(crate) target_audio_codec: String,
    pub(crate) output_protocol: String,
}

impl LanTranscodingProfile {
    pub(crate) fn avplayer_h264_aac_hls() -> Self {
        Self {
            id: LAN_TRANSCODING_PROFILE_ID.to_owned(),
            target_container: TARGET_CONTAINER.to_owned(),
            target_video_codec: TARGET_VIDEO_CODEC.to_owned(),
            target_audio_codec: TARGET_AUDIO_CODEC.to_owned(),
            output_protocol: OUTPUT_PROTOCOL.to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LanTranscodingRuntimeState {
    Disabled,
    Idle,
    Busy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LanTranscodingStatusSnapshot {
    pub(crate) enabled: bool,
    pub(crate) state: LanTranscodingRuntimeState,
    pub(crate) message: String,
    pub(crate) profile: LanTranscodingProfile,
    pub(crate) max_concurrent_jobs: usize,
    pub(crate) active_job_count: usize,
}

impl LanTranscodingStatusSnapshot {
    pub(crate) fn from_options(options: &CacheServerOptions, active_job_count: usize) -> Self {
        let profile = LanTranscodingProfile::avplayer_h264_aac_hls();
        if !options.lan_transcoding_enabled {
            return Self {
                enabled: false,
                state: LanTranscodingRuntimeState::Disabled,
                message: "LAN transcoding is disabled.".to_owned(),
                profile,
                max_concurrent_jobs: options.lan_transcoding_max_concurrent_jobs,
                active_job_count: 0,
            };
        }

        let max_concurrent_jobs = options.lan_transcoding_max_concurrent_jobs.max(1);
        let active_job_count = active_job_count.min(max_concurrent_jobs);
        Self {
            enabled: true,
            state: if active_job_count > 0 {
                LanTranscodingRuntimeState::Busy
            } else {
                LanTranscodingRuntimeState::Idle
            },
            message: if active_job_count > 0 {
                format!(
                    "LAN transcoding worker boundary is ready; {active_job_count}/{max_concurrent_jobs} job(s) active."
                )
            } else {
                "LAN transcoding worker boundary is ready; no jobs are active.".to_owned()
            },
            profile,
            max_concurrent_jobs,
            active_job_count,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HlsTranscodingPlanState {
    Disabled,
    NotRequired,
    Ready,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HlsTranscodingPlan {
    pub(crate) state: HlsTranscodingPlanState,
    pub(crate) profile_id: String,
    pub(crate) reason: String,
    pub(crate) source_variant_id: String,
    pub(crate) target_container: String,
    pub(crate) target_video_codec: String,
    pub(crate) target_audio_codec: String,
    pub(crate) output_protocol: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LanTranscodingJobControl {
    Continue,
    Cancel,
    Preempt,
}

#[derive(Debug)]
pub(crate) enum LanTranscodingError {
    Io(std::io::Error),
    Failed {
        status: ExitStatus,
        stderr_tail: String,
    },
    Cancelled,
    Preempted,
}

impl From<std::io::Error> for LanTranscodingError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for LanTranscodingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "LAN transcoding I/O error: {error}"),
            Self::Failed {
                status,
                stderr_tail,
            } => write!(
                formatter,
                "LAN transcoding ffmpeg failed with status {status}: {stderr_tail}"
            ),
            Self::Cancelled => formatter.write_str("LAN transcoding was cancelled"),
            Self::Preempted => formatter.write_str("LAN transcoding was preempted"),
        }
    }
}

impl std::error::Error for LanTranscodingError {}

impl Default for HlsTranscodingPlan {
    fn default() -> Self {
        Self::with_state(
            HlsTranscodingPlanState::Disabled,
            String::new(),
            "LAN transcoding was not planned for this session.",
        )
    }
}

impl HlsTranscodingPlan {
    #[cfg(test)]
    pub(crate) fn for_variant(
        options: &CacheServerOptions,
        variant: &BilibiliPlaybackVariant,
    ) -> Self {
        Self::for_variant_with_policy(options, variant, PlaybackPolicy::default())
    }

    pub(crate) fn for_variant_with_policy(
        options: &CacheServerOptions,
        variant: &BilibiliPlaybackVariant,
        policy: PlaybackPolicy,
    ) -> Self {
        if variant.video.is_none() {
            return Self::with_state(
                HlsTranscodingPlanState::Unsupported,
                variant.id.clone(),
                "LAN transcoding requires a video media resource.",
            );
        }

        let compatible = variant_is_avplayer_h264_aac_hls_compatible(variant);
        match policy.transcoding_preference {
            TranscodingPreference::Auto if compatible => Self::with_state(
                HlsTranscodingPlanState::NotRequired,
                variant.id.clone(),
                "Selected variant is already compatible with the conservative AVPlayer H.264/AAC HLS profile.",
            ),
            TranscodingPreference::Never if compatible => Self::with_state(
                HlsTranscodingPlanState::NotRequired,
                variant.id.clone(),
                "Selected variant is already compatible with the conservative AVPlayer H.264/AAC HLS profile.",
            ),
            TranscodingPreference::Never => Self::with_state(
                HlsTranscodingPlanState::Disabled,
                variant.id.clone(),
                "LAN transcoding was disabled by playback policy; selected variant will be served through the existing HLS passthrough path.",
            ),
            TranscodingPreference::Force if !options.lan_transcoding_enabled => Self::with_state(
                HlsTranscodingPlanState::Disabled,
                variant.id.clone(),
                "LAN transcoding is disabled by server configuration; playback policy cannot force transcoding.",
            ),
            TranscodingPreference::Auto if !options.lan_transcoding_enabled => Self::with_state(
                HlsTranscodingPlanState::Disabled,
                variant.id.clone(),
                "LAN transcoding is disabled; selected variant will be served through the existing HLS passthrough path.",
            ),
            TranscodingPreference::Auto | TranscodingPreference::Force => Self::with_state(
                HlsTranscodingPlanState::Ready,
                variant.id.clone(),
                if compatible {
                    "Playback policy forces LAN transcoding into the conservative AVPlayer H.264/AAC HLS profile."
                } else {
                    "Selected variant can be converted by the LAN server into the conservative AVPlayer H.264/AAC HLS profile when execution is enabled."
                },
            ),
        }
    }

    pub(crate) fn with_state(
        state: HlsTranscodingPlanState,
        source_variant_id: String,
        reason: impl Into<String>,
    ) -> Self {
        let profile = LanTranscodingProfile::avplayer_h264_aac_hls();
        Self {
            state,
            profile_id: profile.id,
            reason: reason.into(),
            source_variant_id,
            target_container: profile.target_container,
            target_video_codec: profile.target_video_codec,
            target_audio_codec: profile.target_audio_codec,
            output_protocol: profile.output_protocol,
        }
    }
}

pub(crate) async fn run_hls_ffmpeg_transcode<F>(
    ffmpeg_path: &Path,
    video_path: &Path,
    audio_path: Option<&Path>,
    output_path: &Path,
    control: &F,
) -> Result<(), LanTranscodingError>
where
    F: Fn() -> LanTranscodingJobControl,
{
    match control() {
        LanTranscodingJobControl::Continue => {}
        LanTranscodingJobControl::Cancel => return Err(LanTranscodingError::Cancelled),
        LanTranscodingJobControl::Preempt => return Err(LanTranscodingError::Preempted),
    }

    let args = hls_ffmpeg_transcode_args(video_path, audio_path, output_path);
    let mut child = Command::new(ffmpeg_path)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let stderr = child.stderr.take().ok_or_else(|| {
        std::io::Error::other("LAN transcoding ffmpeg stderr pipe was not captured")
    })?;
    let stderr_task = tokio::spawn(read_ffmpeg_stderr_tail(stderr));

    loop {
        match control() {
            LanTranscodingJobControl::Continue => {}
            LanTranscodingJobControl::Cancel => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = stderr_task.await;
                return Err(LanTranscodingError::Cancelled);
            }
            LanTranscodingJobControl::Preempt => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = stderr_task.await;
                return Err(LanTranscodingError::Preempted);
            }
        }

        if let Some(status) = child.try_wait()? {
            let stderr = collect_ffmpeg_stderr(stderr_task).await?;
            if status.success() {
                return Ok(());
            }
            return Err(LanTranscodingError::Failed {
                status,
                stderr_tail: stderr_tail(&stderr),
            });
        }

        sleep(Duration::from_millis(250)).await;
    }
}

fn hls_ffmpeg_transcode_args(
    video_path: &Path,
    audio_path: Option<&Path>,
    output_path: &Path,
) -> Vec<OsString> {
    let mut args = Vec::new();
    push_args(&mut args, ["-y", "-nostdin", "-i"]);
    args.push(video_path.as_os_str().to_owned());
    if let Some(audio_path) = audio_path {
        args.push(OsString::from("-i"));
        args.push(audio_path.as_os_str().to_owned());
    }
    push_args(&mut args, ["-map", "0:v:0"]);
    if audio_path.is_some() {
        push_args(&mut args, ["-map", "1:a:0"]);
    }
    push_args(
        &mut args,
        [
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-profile:v",
            "high",
            "-level:v",
            HLS_FFMPEG_TRANSCODE_LEVEL,
            "-pix_fmt",
            "yuv420p",
            "-vf",
            HLS_FFMPEG_TRANSCODE_FILTER,
            "-maxrate",
            HLS_FFMPEG_TRANSCODE_VIDEO_MAXRATE,
            "-bufsize",
            HLS_FFMPEG_TRANSCODE_VIDEO_BUFSIZE,
        ],
    );
    if audio_path.is_some() {
        push_args(&mut args, ["-c:a", "aac", "-b:a", "128k"]);
    } else {
        args.push(OsString::from("-an"));
    }
    push_args(
        &mut args,
        [
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof",
            "-f",
            "mp4",
        ],
    );
    args.push(output_path.as_os_str().to_owned());
    args
}

fn push_args<const N: usize>(args: &mut Vec<OsString>, values: [&str; N]) {
    args.extend(values.into_iter().map(OsString::from));
}

async fn collect_ffmpeg_stderr(
    stderr_task: tokio::task::JoinHandle<Result<Vec<u8>, std::io::Error>>,
) -> Result<Vec<u8>, LanTranscodingError> {
    match stderr_task.await {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(error)) => Err(LanTranscodingError::Io(error)),
        Err(error) => Err(LanTranscodingError::Io(std::io::Error::other(error))),
    }
}

async fn read_ffmpeg_stderr_tail<R>(mut stderr: R) -> Result<Vec<u8>, std::io::Error>
where
    R: AsyncRead + Unpin,
{
    let mut tail = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let bytes_read = stderr.read(&mut buffer).await?;
        if bytes_read == 0 {
            return Ok(tail);
        }
        append_ffmpeg_stderr_tail(&mut tail, &buffer[..bytes_read]);
    }
}

fn append_ffmpeg_stderr_tail(tail: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.len() >= FFMPEG_STDERR_TAIL_MAX_BYTES {
        tail.clear();
        tail.extend_from_slice(&bytes[bytes.len() - FFMPEG_STDERR_TAIL_MAX_BYTES..]);
        return;
    }

    tail.extend_from_slice(bytes);
    let overflow = tail.len().saturating_sub(FFMPEG_STDERR_TAIL_MAX_BYTES);
    if overflow > 0 {
        tail.drain(..overflow);
    }
}

fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    let max_chars = FFMPEG_STDERR_TAIL_MAX_CHARS;
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let tail = text
        .chars()
        .rev()
        .take(max_chars)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("...{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bbdown_adapter::{
        BilibiliMediaCacheKey, BilibiliMediaRequest, BilibiliMediaRequestKind,
        BilibiliPlaybackVariantKind,
    };

    #[test]
    fn reports_disabled_runtime_status_by_default() {
        let status = LanTranscodingStatusSnapshot::from_options(&CacheServerOptions::default(), 1);

        assert!(!status.enabled);
        assert_eq!(LanTranscodingRuntimeState::Disabled, status.state);
        assert_eq!(0, status.active_job_count);
        assert_eq!(LAN_TRANSCODING_PROFILE_ID, status.profile.id);
    }

    #[test]
    fn reports_ready_runtime_status_when_enabled() {
        let status = LanTranscodingStatusSnapshot::from_options(
            &CacheServerOptions {
                lan_transcoding_enabled: true,
                lan_transcoding_max_concurrent_jobs: 1,
                ..CacheServerOptions::default()
            },
            1,
        );

        assert!(status.enabled);
        assert_eq!(LanTranscodingRuntimeState::Busy, status.state);
        assert_eq!(1, status.active_job_count);
        assert_eq!(1, status.max_concurrent_jobs);
    }

    #[test]
    fn hls_ffmpeg_args_pin_declared_h264_level() {
        let args = hls_ffmpeg_transcode_args(
            Path::new("video.m4s"),
            Some(Path::new("audio.m4s")),
            Path::new("output.m4s"),
        );
        let rendered = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("-profile:v\nhigh\n"));
        assert!(rendered.contains("-level:v\n4.2\n"));
        assert!(rendered.contains("-vf\nscale=w='min(1920,iw)'"));
        assert!(rendered.contains("fps=fps='min(source_fps,60)'"));
        assert!(rendered.contains("-maxrate\n10000k\n"));
        assert!(rendered.contains("-bufsize\n20000k\n"));
    }

    #[test]
    fn ffmpeg_stderr_tail_keeps_bounded_recent_bytes() {
        let mut tail = Vec::new();

        append_ffmpeg_stderr_tail(&mut tail, &vec![b'a'; FFMPEG_STDERR_TAIL_MAX_BYTES + 1024]);
        append_ffmpeg_stderr_tail(&mut tail, b"synthetic ffmpeg error tail");

        assert_eq!(FFMPEG_STDERR_TAIL_MAX_BYTES, tail.len());
        assert!(tail.ends_with(b"synthetic ffmpeg error tail"));
    }

    #[test]
    fn h264_aac_variant_does_not_require_transcoding() {
        let plan = HlsTranscodingPlan::for_variant(
            &CacheServerOptions {
                lan_transcoding_enabled: true,
                ..CacheServerOptions::default()
            },
            &variant("h264", "avc1.640028", Some("mp4a.40.2")),
        );

        assert_eq!(HlsTranscodingPlanState::NotRequired, plan.state);
    }

    #[test]
    fn h264_aac_variant_above_profile_dimensions_requires_transcoding() {
        let mut variant = variant("h264-4k", "avc1.640028", Some("mp4a.40.2"));
        variant.width = Some(3840);
        variant.height = Some(2160);
        variant.video.as_mut().unwrap().width = Some(3840);
        variant.video.as_mut().unwrap().height = Some(2160);

        let plan = HlsTranscodingPlan::for_variant(
            &CacheServerOptions {
                lan_transcoding_enabled: true,
                ..CacheServerOptions::default()
            },
            &variant,
        );

        assert_eq!(HlsTranscodingPlanState::Ready, plan.state);
    }

    #[test]
    fn h264_aac_variant_above_profile_frame_rate_requires_transcoding() {
        let mut variant = variant("h264-120fps", "avc1.640028", Some("mp4a.40.2"));
        variant.frame_rate = Some("120000/1000".to_owned());
        variant.video.as_mut().unwrap().frame_rate = Some("120000/1000".to_owned());

        let plan = HlsTranscodingPlan::for_variant(
            &CacheServerOptions {
                lan_transcoding_enabled: true,
                ..CacheServerOptions::default()
            },
            &variant,
        );

        assert_eq!(HlsTranscodingPlanState::Ready, plan.state);
    }

    #[test]
    fn h264_aac_variant_above_profile_bandwidth_requires_transcoding() {
        let mut variant = variant("h264-high-bitrate", "avc1.640028", Some("mp4a.40.2"));
        variant.bandwidth = Some(30_000_000);
        variant.video.as_mut().unwrap().bandwidth = Some(30_000_000);

        let plan = HlsTranscodingPlan::for_variant(
            &CacheServerOptions {
                lan_transcoding_enabled: true,
                ..CacheServerOptions::default()
            },
            &variant,
        );

        assert_eq!(HlsTranscodingPlanState::Ready, plan.state);
    }

    #[test]
    fn h264_aac_variant_above_profile_h264_level_requires_transcoding() {
        let plan = HlsTranscodingPlan::for_variant(
            &CacheServerOptions {
                lan_transcoding_enabled: true,
                ..CacheServerOptions::default()
            },
            &variant("h264-level-5", "avc1.640033", Some("mp4a.40.2")),
        );

        assert_eq!(HlsTranscodingPlanState::Ready, plan.state);
    }

    #[test]
    fn h264_aac_variant_with_unsupported_h264_profile_requires_transcoding() {
        let plan = HlsTranscodingPlan::for_variant(
            &CacheServerOptions {
                lan_transcoding_enabled: true,
                ..CacheServerOptions::default()
            },
            &variant("h264-high-444", "avc1.F4002A", Some("mp4a.40.2")),
        );

        assert_eq!(HlsTranscodingPlanState::Ready, plan.state);
    }

    #[test]
    fn h264_aac_variant_with_unknown_envelope_requires_transcoding() {
        let mut missing_dimensions =
            variant("missing-dimensions", "avc1.640028", Some("mp4a.40.2"));
        missing_dimensions.width = None;
        missing_dimensions.height = None;
        missing_dimensions.video.as_mut().unwrap().width = None;
        missing_dimensions.video.as_mut().unwrap().height = None;

        let mut missing_bandwidth = variant("missing-bandwidth", "avc1.640028", Some("mp4a.40.2"));
        missing_bandwidth.bandwidth = None;
        missing_bandwidth.video.as_mut().unwrap().bandwidth = None;
        missing_bandwidth.audio.as_mut().unwrap().bandwidth = None;

        let mut missing_frame_rate =
            variant("missing-frame-rate", "avc1.640028", Some("mp4a.40.2"));
        missing_frame_rate.frame_rate = None;
        missing_frame_rate.video.as_mut().unwrap().frame_rate = None;

        let mut malformed_frame_rate =
            variant("malformed-frame-rate", "avc1.640028", Some("mp4a.40.2"));
        malformed_frame_rate.frame_rate = Some("unknown".to_owned());
        malformed_frame_rate.video.as_mut().unwrap().frame_rate = Some("unknown".to_owned());

        let missing_profile = variant("missing-profile", "avc1", Some("mp4a.40.2"));
        let malformed_profile = variant("malformed-profile", "avc1.zzzzzz", Some("mp4a.40.2"));
        let options = CacheServerOptions {
            lan_transcoding_enabled: true,
            ..CacheServerOptions::default()
        };

        for candidate in [
            missing_dimensions,
            missing_bandwidth,
            missing_frame_rate,
            malformed_frame_rate,
            missing_profile,
            malformed_profile,
        ] {
            let plan = HlsTranscodingPlan::for_variant(&options, &candidate);
            assert_eq!(
                HlsTranscodingPlanState::Ready,
                plan.state,
                "{} should require transcoding",
                candidate.id
            );
        }
    }

    #[test]
    fn non_h264_variant_is_ready_only_when_enabled() {
        let variant = variant("hevc", "hvc1.1.6.L120.90", Some("mp4a.40.2"));

        let disabled = HlsTranscodingPlan::for_variant(&CacheServerOptions::default(), &variant);
        assert_eq!(HlsTranscodingPlanState::Disabled, disabled.state);

        let enabled = HlsTranscodingPlan::for_variant(
            &CacheServerOptions {
                lan_transcoding_enabled: true,
                ..CacheServerOptions::default()
            },
            &variant,
        );
        assert_eq!(HlsTranscodingPlanState::Ready, enabled.state);
        assert_eq!("hevc", enabled.source_variant_id);
    }

    #[test]
    fn never_policy_disables_transcoding_for_incompatible_variant() {
        let plan = HlsTranscodingPlan::for_variant_with_policy(
            &CacheServerOptions {
                lan_transcoding_enabled: true,
                ..CacheServerOptions::default()
            },
            &variant("hevc", "hvc1.1.6.L120.90", Some("mp4a.40.2")),
            PlaybackPolicy {
                transcoding_preference: TranscodingPreference::Never,
                ..PlaybackPolicy::default()
            },
        );

        assert_eq!(HlsTranscodingPlanState::Disabled, plan.state);
        assert!(plan.reason.contains("disabled by playback policy"));
    }

    #[test]
    fn force_policy_transcodes_compatible_variant_when_server_allows_it() {
        let plan = HlsTranscodingPlan::for_variant_with_policy(
            &CacheServerOptions {
                lan_transcoding_enabled: true,
                ..CacheServerOptions::default()
            },
            &variant("h264", "avc1.640028", Some("mp4a.40.2")),
            PlaybackPolicy {
                transcoding_preference: TranscodingPreference::Force,
                ..PlaybackPolicy::default()
            },
        );

        assert_eq!(HlsTranscodingPlanState::Ready, plan.state);
        assert!(plan.reason.contains("forces LAN transcoding"));
    }

    #[test]
    fn server_configuration_remains_hard_bound_for_force_policy() {
        let plan = HlsTranscodingPlan::for_variant_with_policy(
            &CacheServerOptions::default(),
            &variant("h264", "avc1.640028", Some("mp4a.40.2")),
            PlaybackPolicy {
                transcoding_preference: TranscodingPreference::Force,
                ..PlaybackPolicy::default()
            },
        );

        assert_eq!(HlsTranscodingPlanState::Disabled, plan.state);
        assert!(plan.reason.contains("server configuration"));
    }

    #[test]
    fn request_video_codec_is_authoritative_for_compatibility() {
        let mut variant = variant("mismatched-video", "avc1.640028", Some("mp4a.40.2"));
        variant.video.as_mut().unwrap().codecs = Some("hev1.1.6.L120.90".to_owned());

        let plan = HlsTranscodingPlan::for_variant(
            &CacheServerOptions {
                lan_transcoding_enabled: true,
                ..CacheServerOptions::default()
            },
            &variant,
        );

        assert_eq!(HlsTranscodingPlanState::Ready, plan.state);
    }

    #[test]
    fn request_audio_codec_is_authoritative_for_compatibility() {
        let mut variant = variant("mismatched-audio", "avc1.640028", Some("mp4a.40.2"));
        variant.audio.as_mut().unwrap().codecs = Some("flac".to_owned());

        let plan = HlsTranscodingPlan::for_variant(
            &CacheServerOptions {
                lan_transcoding_enabled: true,
                ..CacheServerOptions::default()
            },
            &variant,
        );

        assert_eq!(HlsTranscodingPlanState::Ready, plan.state);
    }

    #[test]
    fn non_aac_mp4a_audio_variant_requires_transcoding() {
        let plan = HlsTranscodingPlan::for_variant(
            &CacheServerOptions {
                lan_transcoding_enabled: true,
                ..CacheServerOptions::default()
            },
            &variant("mp3-audio", "avc1.640028", Some("mp4a.6B")),
        );

        assert_eq!(HlsTranscodingPlanState::Ready, plan.state);
    }

    fn variant(id: &str, video_codec: &str, audio_codec: Option<&str>) -> BilibiliPlaybackVariant {
        BilibiliPlaybackVariant {
            id: id.to_owned(),
            kind: BilibiliPlaybackVariantKind::Dash,
            content_id: "cid-1".to_owned(),
            codecs: vec![
                video_codec.to_owned(),
                audio_codec.unwrap_or_default().to_owned(),
            ],
            mime_types: vec!["video/mp4".to_owned()],
            bandwidth: Some(1_000_000),
            width: Some(1920),
            height: Some(1080),
            frame_rate: Some("60".to_owned()),
            duration_seconds: Some(60),
            abr: None,
            video: Some(media_request(BilibiliMediaRequestKind::Video, video_codec)),
            audio: audio_codec.map(|codec| media_request(BilibiliMediaRequestKind::Audio, codec)),
            flv_segments: Vec::new(),
        }
    }

    fn media_request(kind: BilibiliMediaRequestKind, codecs: &str) -> BilibiliMediaRequest {
        BilibiliMediaRequest {
            kind,
            url: "https://media.example.test/resource.m4s".to_owned(),
            backup_urls: Vec::new(),
            headers: Vec::new(),
            stream_id: None,
            mime_type: Some("video/mp4".to_owned()),
            codecs: Some(codecs.to_owned()),
            bandwidth: Some(match kind {
                BilibiliMediaRequestKind::Audio => LAN_TRANSCODING_AUDIO_BANDWIDTH_BPS,
                _ => 1_000_000,
            }),
            width: Some(1920),
            height: Some(1080),
            frame_rate: (kind == BilibiliMediaRequestKind::Video).then(|| "60".to_owned()),
            size: Some(10_000_000),
            duration_seconds: Some(60),
            cache_key: BilibiliMediaCacheKey {
                content_id: "cid-1".to_owned(),
                media_kind: kind,
                stream_id: None,
                codecs: Some(codecs.to_owned()),
                source_hash: "resource-source".to_owned(),
            },
        }
    }
}
