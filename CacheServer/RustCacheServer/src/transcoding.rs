use crate::{
    bbdown_adapter::{BilibiliMediaRequest, BilibiliPlaybackVariant},
    codecs::{codec_list_matches, is_aac_codec, is_h264_codec},
    config::CacheServerOptions,
};

pub(crate) const LAN_TRANSCODING_PROFILE_ID: &str = "avplayer-h264-aac-hls-v1";
const TARGET_CONTAINER: &str = "hls/fmp4";
const TARGET_VIDEO_CODEC: &str = "h264";
const TARGET_AUDIO_CODEC: &str = "aac";
const OUTPUT_PROTOCOL: &str = "hls";

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
    pub(crate) fn for_variant(
        options: &CacheServerOptions,
        variant: &BilibiliPlaybackVariant,
    ) -> Self {
        if variant.video.is_none() {
            return Self::with_state(
                HlsTranscodingPlanState::Unsupported,
                variant.id.clone(),
                "LAN transcoding requires a video media resource.",
            );
        }

        if variant_is_avplayer_h264_aac_hls_compatible(variant) {
            return Self::with_state(
                HlsTranscodingPlanState::NotRequired,
                variant.id.clone(),
                "Selected variant is already compatible with the conservative AVPlayer H.264/AAC HLS profile.",
            );
        }

        if !options.lan_transcoding_enabled {
            return Self::with_state(
                HlsTranscodingPlanState::Disabled,
                variant.id.clone(),
                "LAN transcoding is disabled; selected variant will be served through the existing HLS passthrough path.",
            );
        }

        Self::with_state(
            HlsTranscodingPlanState::Ready,
            variant.id.clone(),
            "Selected variant can be converted by the LAN server into the conservative AVPlayer H.264/AAC HLS profile when execution is enabled.",
        )
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

fn variant_is_avplayer_h264_aac_hls_compatible(variant: &BilibiliPlaybackVariant) -> bool {
    let Some(video) = variant.video.as_ref() else {
        return false;
    };
    variant_has_codec(variant, Some(video), is_h264_codec)
        && variant
            .audio
            .as_ref()
            .is_none_or(|audio| variant_has_codec(variant, Some(audio), is_aac_codec))
}

fn variant_has_codec(
    variant: &BilibiliPlaybackVariant,
    request: Option<&BilibiliMediaRequest>,
    predicate: fn(&str) -> bool,
) -> bool {
    if let Some(codecs) = request.and_then(|request| request.codecs.as_deref()) {
        return codec_list_matches(codecs, predicate);
    }

    variant
        .codecs
        .iter()
        .any(|codecs| codec_list_matches(codecs, predicate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bbdown_adapter::{
        BilibiliMediaCacheKey, BilibiliMediaRequestKind, BilibiliPlaybackVariantKind,
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
                lan_transcoding_max_concurrent_jobs: 2,
                ..CacheServerOptions::default()
            },
            1,
        );

        assert!(status.enabled);
        assert_eq!(LanTranscodingRuntimeState::Busy, status.state);
        assert_eq!(1, status.active_job_count);
        assert_eq!(2, status.max_concurrent_jobs);
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
            frame_rate: None,
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
            bandwidth: Some(1_000_000),
            width: Some(1920),
            height: Some(1080),
            frame_rate: None,
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
